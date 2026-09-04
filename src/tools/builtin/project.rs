//! Desktop Project tools — port of hermes `tools/project_tools.py`
//! (v2026.8.3).
//!
//! Projects (per-profile `projects.db`) are the named workspaces the
//! desktop sidebar groups sessions into. Creating / switching a project is
//! a deliberate act expressed as explicit tools — never a side effect of a
//! terminal `cd`.
//!
//! Exposed only where a GUI can follow the move: the tools live in the
//! `project` toolset (kept off the core coding toolset), so no
//! CLI/messaging/cron schema carries them. A host application wires
//! [`set_project_workspace_callback`] so a create/switch re-anchors the
//! live session's cwd and the sidebar follows the move; the DB write is
//! the durable part.

use std::sync::{Arc, Mutex, OnceLock};

use crate::projects_db as pdb;
use crate::tools::{tool, ToolContext, ToolRegistry};
use serde_json::{json, Value};

/// Workspace re-anchor sink: `(task_id, primary_path, project_name)` —
/// hermes `_workspace_callback`, installed by the GUI gateway.
pub type ProjectWorkspaceFn = Arc<dyn Fn(&str, &str, &str) + Send + Sync>;

fn workspace_slot() -> &'static Mutex<Option<ProjectWorkspaceFn>> {
    static SLOT: OnceLock<Mutex<Option<ProjectWorkspaceFn>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// Install (or clear, with `None`) the GUI workspace callback.
pub fn set_project_workspace_callback(fn_: Option<ProjectWorkspaceFn>) {
    if let Ok(mut slot) = workspace_slot().lock() {
        *slot = fn_;
    }
}

fn apply_workspace(task_id: Option<&str>, path: Option<&str>, name: &str) {
    let cb = workspace_slot().lock().ok().and_then(|slot| slot.clone());
    if let (Some(cb), Some(task_id), Some(path)) = (cb, task_id, path) {
        // hermes swallows callback errors — the DB write already happened.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cb(task_id, path, name)));
    }
}

/// The primary folder of a project (hermes `_primary_path`).
fn primary_path(proj: &pdb::Project) -> Option<String> {
    if let Some(ref p) = proj.primary_path {
        if !p.is_empty() {
            return Some(p.clone());
        }
    }
    if let Some(folder) = proj.folders.iter().find(|f| f.is_primary) {
        return Some(folder.path.clone());
    }
    proj.folders.first().map(|f| f.path.clone())
}

/// Resolve a project by exact id/slug/name, then case-insensitive
/// slug/name (hermes `_resolve`).
fn resolve(conn: &rusqlite::Connection, token: &str) -> crate::error::Result<Option<pdb::Project>> {
    let token = token.trim();
    if token.is_empty() {
        return Ok(None);
    }
    let projects = pdb::list_projects(conn, true)?;
    for proj in &projects {
        if token == proj.id || token == proj.slug || token == proj.name {
            return Ok(Some(proj.clone()));
        }
    }
    let low = token.to_ascii_lowercase();
    for proj in &projects {
        if proj.slug.to_ascii_lowercase() == low || proj.name.to_ascii_lowercase() == low {
            return Ok(Some(proj.clone()));
        }
    }
    Ok(None)
}

fn open_db(ctx: &ToolContext) -> crate::error::Result<rusqlite::Connection> {
    pdb::connect(Some(&ctx.home.join("projects.db")))
}

fn project_list_impl(ctx: &ToolContext) -> Value {
    let result = (|| -> crate::error::Result<Value> {
        let conn = open_db(ctx)?;
        let active = pdb::get_active_id(&conn)?;
        let projects = pdb::list_projects(&conn, false)?;
        Ok(json!({
            "active_id": active,
            "projects": projects.iter().map(|p| json!({
                "id": p.id,
                "slug": p.slug,
                "name": p.name,
                "primary_path": primary_path(p),
                "active": Some(&p.id) == active.as_ref(),
            })).collect::<Vec<_>>(),
        }))
    })();
    match result {
        Ok(v) => v,
        Err(e) => json!({"success": false, "error": e.to_string()}),
    }
}

fn project_create_impl(ctx: &ToolContext, name: &str, path: Option<&str>) -> Value {
    let name = name.trim();
    if name.is_empty() {
        return json!({"success": false, "error": "name is required"});
    }
    let folder = path.map(|p| p.trim()).filter(|p| !p.is_empty());
    let result = (|| -> crate::error::Result<Value> {
        let conn = open_db(ctx)?;
        let folders: Vec<&str> = match folder {
            Some(f) => vec![f],
            None => vec![],
        };
        let pid = match pdb::create_project(
            &conn,
            &pdb::CreateProject {
                name,
                folders: &folders,
                primary_path: folder,
                ..Default::default()
            },
        ) {
            Ok(pid) => pid,
            Err(e) => return Ok(json!({"success": false, "error": e.to_string()})),
        };
        pdb::set_active(&conn, Some(&pid))?;
        let Some(proj) = pdb::get_project(&conn, &pid)? else {
            return Ok(json!({"success": false, "error": "project vanished after create"}));
        };
        let primary = primary_path(&proj);
        apply_workspace(Some(&ctx.session_id), primary.as_deref(), &proj.name);
        Ok(json!({
            "success": true,
            "id": proj.id,
            "slug": proj.slug,
            "name": proj.name,
            "primary_path": primary,
        }))
    })();
    match result {
        Ok(v) => v,
        Err(e) => json!({"success": false, "error": e.to_string()}),
    }
}

fn project_switch_impl(ctx: &ToolContext, project: &str) -> Value {
    let result = (|| -> crate::error::Result<Value> {
        let conn = open_db(ctx)?;
        let Some(proj) = resolve(&conn, project)? else {
            return Ok(
                json!({"success": false, "error": format!("no project matching '{}'", project.trim())}),
            );
        };
        pdb::set_active(&conn, Some(&proj.id))?;
        let primary = primary_path(&proj);
        apply_workspace(Some(&ctx.session_id), primary.as_deref(), &proj.name);
        Ok(json!({
            "success": true,
            "id": proj.id,
            "slug": proj.slug,
            "name": proj.name,
            "primary_path": primary,
        }))
    })();
    match result {
        Ok(v) => v,
        Err(e) => json!({"success": false, "error": e.to_string()}),
    }
}

pub fn register(registry: &mut ToolRegistry) {
    registry.register(
        tool("project_list")
            .description("List the desktop Projects (named workspaces) and which one is active.")
            .parameters(json!({"type": "object", "properties": {}}))
            .handler(|_args, ctx| async move { Ok(project_list_impl(&ctx)) })
            .toolset("project")
            .emoji("\u{1F4C1}")
            .build()
            .expect("project_list builds"),
    );
    registry.register(
        tool("project_create")
            .description(
                "Create a desktop Project (a named workspace) and switch this chat into it. \
                 Pass `path` to anchor it to a repo/folder — this chat's workspace moves \
                 there and the sidebar follows. Use when starting work in a new repo/folder; \
                 this is the intentional way to move the session, not `cd`.",
            )
            .parameters(json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Human name, e.g. 'Aurora Demo'"},
                    "path": {"type": "string", "description": "Primary repo/folder to anchor the project to"}
                },
                "required": ["name"]
            }))
            .handler(|args, ctx| async move {
                let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let path = args.get("path").and_then(|v| v.as_str());
                Ok(project_create_impl(&ctx, name, path))
            })
            .toolset("project")
            .emoji("\u{1F4C1}")
            .build()
            .expect("project_create builds"),
    );
    registry.register(
        tool("project_switch")
            .description(
                "Switch this chat into an existing desktop Project (by name, slug, or id). \
                 Moves the session's workspace to the project's primary folder and the \
                 sidebar follows. The intentional way to move between projects, not `cd`.",
            )
            .parameters(json!({
                "type": "object",
                "properties": {
                    "project": {"type": "string", "description": "Project name, slug, or id"}
                },
                "required": ["project"]
            }))
            .handler(|args, ctx| async move {
                let project = args.get("project").and_then(|v| v.as_str()).unwrap_or("");
                Ok(project_switch_impl(&ctx, project))
            })
            .toolset("project")
            .emoji("\u{1F4C1}")
            .build()
            .expect("project_switch builds"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_context(dir: &std::path::Path) -> ToolContext {
        ToolContext::new()
            .with_home(dir)
            .with_session_id("test-session")
    }

    #[tokio::test]
    async fn create_switch_list_roundtrip() {
        let dir = std::env::temp_dir().join(format!(
            "ulnclaw-project-tools-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = test_context(&dir);

        let created = project_create_impl(&ctx, "Aurora Demo", Some("/srv/aurora"));
        assert_eq!(created["success"], true);
        assert_eq!(created["slug"], "aurora-demo");
        assert_eq!(created["primary_path"], "/srv/aurora");

        let listed = project_list_impl(&ctx);
        assert_eq!(listed["active_id"], created["id"]);
        assert_eq!(listed["projects"][0]["active"], true);

        // Create a second project (auto-switches), then switch back by name.
        project_create_impl(&ctx, "Beta", Some("/srv/beta"));
        let switched = project_switch_impl(&ctx, "AURORA demo");
        assert_eq!(switched["success"], true);
        assert_eq!(switched["slug"], "aurora-demo");

        let missing = project_switch_impl(&ctx, "ghost");
        assert_eq!(missing["success"], false);
        assert!(missing["error"]
            .as_str()
            .unwrap()
            .contains("no project matching"));

        let no_name = project_create_impl(&ctx, "  ", None);
        assert_eq!(no_name["success"], false);

        // Workspace callback fires on switch.
        let seen: Arc<Mutex<Vec<(String, String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        set_project_workspace_callback(Some(Arc::new(move |task, path, name| {
            sink.lock()
                .unwrap()
                .push((task.to_string(), path.to_string(), name.to_string()));
        })));
        project_switch_impl(&ctx, "beta");
        set_project_workspace_callback(None);
        let calls = seen.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].2, "Beta");

        std::fs::remove_dir_all(&dir).unwrap();
    }
}

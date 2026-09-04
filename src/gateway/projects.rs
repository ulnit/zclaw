//! Projects registry HTTP API (P162) — the desktop Projects view and any
//! external client talk to these endpoints; they share the same
//! per-profile `projects.db` as the `ulnclaw project` CLI.

use axum::{
    extract::{Path, Query},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::json;

use crate::projects_db as pdb;

fn conn() -> Result<rusqlite::Connection, Response> {
    pdb::connect(None).map_err(|e| super::server_error(&e.to_string()))
}

fn resolve(conn: &rusqlite::Connection, id_or_slug: &str) -> Result<pdb::Project, Response> {
    match pdb::get_project(conn, id_or_slug) {
        Ok(Some(project)) => Ok(project),
        Ok(None) => Err(super::not_found(&format!("project {id_or_slug} not found"))),
        Err(e) => Err(super::server_error(&e.to_string())),
    }
}

/// Best-effort mirror of the CLI `bind-board` workdir sync (hermes
/// `_sync_board_default_workdir`): point the bound board's
/// `default_workdir` at the project's primary repo. Non-fatal.
fn sync_board_default_workdir(project: &pdb::Project, board_slug: &str) {
    let Some(primary) = project
        .primary_path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())
    else {
        return;
    };
    let slug = board_slug.trim().to_lowercase();
    if slug.is_empty() {
        return;
    }
    let Ok(store) = crate::kanban::KanbanStore::open_default() else {
        return;
    };
    let exists = store
        .list_boards()
        .map(|boards| boards.iter().any(|b| b.slug == slug))
        .unwrap_or(false);
    if !exists {
        return;
    }
    let _ = store.set_board_workdir(&slug, Some(primary));
}

/// `GET /api/projects?all=true` — projects (oldest first) + active id.
pub async fn list_projects(
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let include_archived = params
        .get("all")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false);
    let conn = match conn() {
        Ok(c) => c,
        Err(e) => return e,
    };
    let active_id = pdb::get_active_id(&conn).unwrap_or(None);
    let projects = match pdb::list_projects(&conn, include_archived) {
        Ok(p) => p,
        Err(e) => return super::server_error(&e.to_string()),
    };
    Json(json!({
        "object": "ulnclaw.projects.list",
        "active_id": active_id,
        "projects": projects.iter().map(pdb::Project::to_json).collect::<Vec<_>>(),
    }))
    .into_response()
}

#[derive(Deserialize, Default)]
pub struct CreateProjectBody {
    pub name: String,
    #[serde(default)]
    pub slug: Option<String>,
    #[serde(default)]
    pub folders: Option<Vec<String>>,
    #[serde(default)]
    pub primary_path: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub board_slug: Option<String>,
    /// Set as the active project after creating.
    #[serde(default, rename = "use")]
    pub use_it: Option<bool>,
}

/// `POST /api/projects` — create a project.
pub async fn create_project(Json(body): Json<CreateProjectBody>) -> Response {
    let conn = match conn() {
        Ok(c) => c,
        Err(e) => return e,
    };
    let folder_refs: Vec<&str> = body
        .folders
        .as_ref()
        .map(|folders| folders.iter().map(String::as_str).collect())
        .unwrap_or_default();
    let args = pdb::CreateProject {
        name: &body.name,
        slug: body.slug.as_deref(),
        folders: &folder_refs,
        primary_path: body.primary_path.as_deref(),
        description: body.description.as_deref(),
        icon: body.icon.as_deref(),
        color: body.color.as_deref(),
        board_slug: body.board_slug.as_deref(),
    };
    let pid = match pdb::create_project(&conn, &args) {
        Ok(id) => id,
        Err(e) => return super::bad_request(&e.to_string(), None),
    };
    if body.use_it.unwrap_or(false) {
        if let Err(e) = pdb::set_active(&conn, Some(&pid)) {
            return super::server_error(&e.to_string());
        }
    }
    let project = match pdb::get_project(&conn, &pid) {
        Ok(Some(p)) => p,
        Ok(None) => return super::server_error("project vanished after create"),
        Err(e) => return super::server_error(&e.to_string()),
    };
    Json(json!({
        "object": "ulnclaw.project",
        "project": project.to_json(),
    }))
    .into_response()
}

/// `GET /api/projects/:id` — one project by id or slug.
pub async fn get_project(Path(id): Path<String>) -> Response {
    let conn = match conn() {
        Ok(c) => c,
        Err(e) => return e,
    };
    let project = match resolve(&conn, &id) {
        Ok(p) => p,
        Err(e) => return e,
    };
    Json(json!({
        "object": "ulnclaw.project",
        "project": project.to_json(),
    }))
    .into_response()
}

#[derive(Deserialize, Default)]
pub struct UpdateProjectBody {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub color: Option<String>,
    /// Board slug; empty string unbinds.
    #[serde(default)]
    pub board_slug: Option<String>,
}

/// `PATCH /api/projects/:id` — patch fields; binding a board mirrors the
/// primary repo as the board's `default_workdir`.
pub async fn update_project(
    Path(id): Path<String>,
    Json(body): Json<UpdateProjectBody>,
) -> Response {
    let conn = match conn() {
        Ok(c) => c,
        Err(e) => return e,
    };
    let project = match resolve(&conn, &id) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let args = pdb::UpdateProject {
        name: body.name.as_deref(),
        description: body.description.as_deref(),
        icon: body.icon.as_deref(),
        color: body.color.as_deref(),
        board_slug: body.board_slug.as_deref(),
    };
    if let Err(e) = pdb::update_project(&conn, &project.id, &args) {
        return super::bad_request(&e.to_string(), None);
    }
    let updated = match pdb::get_project(&conn, &project.id) {
        Ok(Some(p)) => p,
        Ok(None) => return super::server_error("project vanished after update"),
        Err(e) => return super::server_error(&e.to_string()),
    };
    if let Some(board) = body
        .board_slug
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty())
    {
        sync_board_default_workdir(&updated, board);
    }
    Json(json!({
        "object": "ulnclaw.project",
        "project": updated.to_json(),
    }))
    .into_response()
}

/// `DELETE /api/projects/:id` — hard-delete a project (folders cascade).
pub async fn delete_project(Path(id): Path<String>) -> Response {
    let conn = match conn() {
        Ok(c) => c,
        Err(e) => return e,
    };
    let project = match resolve(&conn, &id) {
        Ok(p) => p,
        Err(e) => return e,
    };
    if let Err(e) = pdb::delete_project(&conn, &project.id) {
        return super::server_error(&e.to_string());
    }
    Json(json!({ "object": "ulnclaw.project.deleted", "id": project.id })).into_response()
}

#[derive(Deserialize)]
pub struct FolderBody {
    pub path: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub primary: Option<bool>,
}

/// `POST /api/projects/:id/folders` — add a folder.
pub async fn add_folder(Path(id): Path<String>, Json(body): Json<FolderBody>) -> Response {
    let conn = match conn() {
        Ok(c) => c,
        Err(e) => return e,
    };
    let project = match resolve(&conn, &id) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let norm = match pdb::add_folder(
        &conn,
        &project.id,
        &body.path,
        body.label.as_deref(),
        body.primary.unwrap_or(false),
    ) {
        Ok(p) => p,
        Err(e) => return super::bad_request(&e.to_string(), None),
    };
    let updated = match pdb::get_project(&conn, &project.id) {
        Ok(Some(p)) => p,
        Ok(None) => return super::server_error("project vanished after add_folder"),
        Err(e) => return super::server_error(&e.to_string()),
    };
    Json(json!({
        "object": "ulnclaw.project",
        "added": norm,
        "project": updated.to_json(),
    }))
    .into_response()
}

/// `DELETE /api/projects/:id/folders` — remove a folder (body: path).
pub async fn remove_folder(Path(id): Path<String>, Json(body): Json<FolderBody>) -> Response {
    let conn = match conn() {
        Ok(c) => c,
        Err(e) => return e,
    };
    let project = match resolve(&conn, &id) {
        Ok(p) => p,
        Err(e) => return e,
    };
    match pdb::remove_folder(&conn, &project.id, &body.path) {
        Ok(true) => {}
        Ok(false) => return super::not_found(&format!("folder not in project: {}", body.path)),
        Err(e) => return super::server_error(&e.to_string()),
    }
    let updated = match pdb::get_project(&conn, &project.id) {
        Ok(Some(p)) => p,
        Ok(None) => return super::server_error("project vanished after remove_folder"),
        Err(e) => return super::server_error(&e.to_string()),
    };
    Json(json!({
        "object": "ulnclaw.project",
        "project": updated.to_json(),
    }))
    .into_response()
}

/// `POST /api/projects/:id/primary` — set the primary folder.
pub async fn set_primary(Path(id): Path<String>, Json(body): Json<FolderBody>) -> Response {
    let conn = match conn() {
        Ok(c) => c,
        Err(e) => return e,
    };
    let project = match resolve(&conn, &id) {
        Ok(p) => p,
        Err(e) => return e,
    };
    match pdb::set_primary(&conn, &project.id, &body.path) {
        Ok(true) => {}
        Ok(false) => {
            return super::bad_request(
                &format!(
                    "'{}' is not a folder of project {}",
                    body.path, project.slug
                ),
                None,
            )
        }
        Err(e) => return super::server_error(&e.to_string()),
    }
    let updated = match pdb::get_project(&conn, &project.id) {
        Ok(Some(p)) => p,
        Ok(None) => return super::server_error("project vanished after set_primary"),
        Err(e) => return super::server_error(&e.to_string()),
    };
    Json(json!({
        "object": "ulnclaw.project",
        "project": updated.to_json(),
    }))
    .into_response()
}

/// `POST /api/projects/:id/archive`.
pub async fn archive_project(Path(id): Path<String>) -> Response {
    let conn = match conn() {
        Ok(c) => c,
        Err(e) => return e,
    };
    let project = match resolve(&conn, &id) {
        Ok(p) => p,
        Err(e) => return e,
    };
    if let Err(e) = pdb::archive_project(&conn, &project.id) {
        return super::server_error(&e.to_string());
    }
    Json(json!({ "object": "ulnclaw.project.archived", "id": project.id })).into_response()
}

/// `POST /api/projects/:id/restore`.
pub async fn restore_project(Path(id): Path<String>) -> Response {
    let conn = match conn() {
        Ok(c) => c,
        Err(e) => return e,
    };
    let project = match resolve(&conn, &id) {
        Ok(p) => p,
        Err(e) => return e,
    };
    if let Err(e) = pdb::restore_project(&conn, &project.id) {
        return super::server_error(&e.to_string());
    }
    Json(json!({ "object": "ulnclaw.project.restored", "id": project.id })).into_response()
}

#[derive(Deserialize, Default)]
pub struct ActiveBody {
    /// Project id or slug; null/empty clears the active pointer.
    #[serde(default)]
    pub id: Option<String>,
}

/// `POST /api/projects/active` — set (or clear) the active project.
pub async fn set_active(Json(body): Json<ActiveBody>) -> Response {
    let conn = match conn() {
        Ok(c) => c,
        Err(e) => return e,
    };
    let target = body
        .id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty());
    let resolved = match target {
        None => None,
        Some(id_or_slug) => match resolve(&conn, id_or_slug) {
            Ok(p) => Some(p),
            Err(e) => return e,
        },
    };
    if let Err(e) = pdb::set_active(&conn, resolved.as_ref().map(|p| p.id.as_str())) {
        return super::server_error(&e.to_string());
    }
    Json(json!({
        "object": "ulnclaw.projects.active",
        "active_id": resolved.as_ref().map(|p| &p.id),
        "active_slug": resolved.as_ref().map(|p| &p.slug),
    }))
    .into_response()
}

/// `GET /api/projects/repos` — cached discovered repos (most recent first).
pub async fn list_repos() -> Response {
    let conn = match conn() {
        Ok(c) => c,
        Err(e) => return e,
    };
    let repos = match pdb::list_discovered_repos(&conn) {
        Ok(r) => r,
        Err(e) => return super::server_error(&e.to_string()),
    };
    let policy = pdb::get_discovery_policy_key(&conn).unwrap_or(None);
    Json(json!({
        "object": "ulnclaw.projects.repos",
        "policy_key": policy,
        "repos": repos,
    }))
    .into_response()
}

/// `POST /api/projects/scan` — run a filesystem repo scan into the cache.
pub async fn scan_repos(Json(body): Json<ScanBody>) -> Response {
    let roots: Vec<std::path::PathBuf> = if body.roots.is_empty() {
        match dirs::home_dir() {
            Some(home) => vec![home],
            None => return super::bad_request("cannot determine home directory; pass roots", None),
        }
    } else {
        body.roots.iter().map(std::path::PathBuf::from).collect()
    };
    let max_depth = body
        .max_depth
        .unwrap_or(crate::projects_scan::DEFAULT_MAX_DEPTH);
    let found = crate::projects_scan::scan_for_repos(&roots, max_depth);
    let conn = match conn() {
        Ok(c) => c,
        Err(e) => return e,
    };
    let pairs: Vec<(String, Option<String>)> = found
        .iter()
        .map(|r| (r.root.clone(), Some(r.label.clone())))
        .collect();
    let recorded = match pdb::record_discovered_repos(
        &conn,
        &pairs,
        true,
        Some(crate::projects_scan::CLI_SCAN_POLICY_KEY),
    ) {
        Ok(n) => n,
        Err(e) => return super::server_error(&e.to_string()),
    };
    Json(json!({
        "object": "ulnclaw.projects.scan",
        "roots": roots.iter().map(|r| r.to_string_lossy().to_string()).collect::<Vec<_>>(),
        "max_depth": max_depth,
        "recorded": recorded,
        "repos": found.iter().map(|r| json!({ "root": r.root, "label": r.label })).collect::<Vec<_>>(),
    }))
    .into_response()
}

#[derive(Deserialize, Default)]
pub struct ScanBody {
    #[serde(default)]
    pub roots: Vec<String>,
    #[serde(default)]
    pub max_depth: Option<usize>,
}

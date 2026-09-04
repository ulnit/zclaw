//! Skills tools — port of hermes' tools/skills_tool.py
//!
//! Tools: skills_list, skill_view, skill_manage.

use crate::skills;
use crate::tools::{tool, ToolContext, ToolRegistry};
use serde_json::json;

pub fn register(registry: &mut ToolRegistry) {
    registry.register(skills_list_tool());
    registry.register(skill_view_tool());
    registry.register(skill_manage_tool());
}

fn skills_dir(ctx: &ToolContext) -> std::path::PathBuf {
    ctx.home.join("skills")
}

fn skills_list_tool() -> crate::tools::Tool {
    tool("skills_list")
        .description("List available skills (name + description). Use skill_view(name) to load full content.")
        .parameters(json!({
            "type": "object",
            "properties": {
                "category": {"type": "string", "description": "Optional category filter to narrow results"}
            },
            "required": []
        }))
        .handler(|args, ctx| async move {
            let category = args.get("category").and_then(|v| v.as_str()).map(String::from);
            let all = skills::list_skills(&skills_dir(&ctx));
            let filtered: Vec<serde_json::Value> = all
                .iter()
                .filter(|skill| {
                    category
                        .as_ref()
                        .map(|c| skill.category.eq_ignore_ascii_case(c))
                        .unwrap_or(true)
                })
                .map(|skill| {
                    json!({
                        "name": skill.name,
                        "description": skill.description,
                        "category": skill.category,
                    })
                })
                .collect();
            Ok(json!({
                "success": true,
                "skills": filtered,
                "skills_dir": skills_dir(&ctx).display().to_string(),
            }))
        })
        .toolset("skills")
        .emoji("📚")
        .build()
        .expect("skills_list builds")
}

fn skill_view_tool() -> crate::tools::Tool {
    tool("skill_view")
        .description(
            "Skills allow for loading information about specific tasks and workflows, as well as \
             scripts and templates. Load a skill's full content or access its linked files \
             (references, templates, scripts). First call returns SKILL.md content plus a \
             'linked_files' list showing available references/templates/scripts. To access those, \
             call again with file_path parameter.",
        )
        .parameters(json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "The skill name (use skills_list to see available skills)."},
                "file_path": {"type": "string", "description": "OPTIONAL: Path to a linked file within the skill (e.g., 'references/api.md'). Omit to get the main SKILL.md content."}
            },
            "required": ["name"]
        }))
        .handler(|args, ctx| async move {
            let Some(name) = args.get("name").and_then(|v| v.as_str()) else {
                return Ok(json!({"success": false, "error": "skill_view: 'name' is required"}));
            };
            let Some(skill) = skills::find_skill(&skills_dir(&ctx), name) else {
                let available: Vec<String> = skills::list_skills(&skills_dir(&ctx))
                    .iter()
                    .map(|s| s.name.clone())
                    .collect();
                return Ok(json!({
                    "success": false,
                    "error": format!("Skill '{}' not found. Available: {}", name, available.join(", "))
                }));
            };

            if let Some(file_path) = args.get("file_path").and_then(|v| v.as_str()) {
                // Guard against path traversal.
                let candidate = skill.path.join(file_path);
                let canonical = candidate.canonicalize();
                let skill_canonical = skill.path.canonicalize();
                let allowed = match (canonical, skill_canonical) {
                    (Ok(c), Ok(s)) => c.starts_with(s),
                    _ => false,
                };
                if !allowed {
                    return Ok(json!({"success": false, "error": "file_path escapes the skill directory"}));
                }
                match std::fs::read_to_string(&candidate) {
                    Ok(content) => Ok(json!({
                        "success": true,
                        "skill": skill.name,
                        "file_path": file_path,
                        "content": content,
                    })),
                    Err(e) => Ok(json!({"success": false, "error": format!("read {}: {}", file_path, e)})),
                }
            } else {
                let content = std::fs::read_to_string(skill.path.join("SKILL.md")).unwrap_or_default();
                // Skill-declared env vars join the sandbox passthrough
                // allowlist (hermes env_passthrough semantics; provider
                // credentials are refused).
                let declared = skills::required_env_vars(&content);
                let accepted = ctx.register_env_passthrough(&declared);
                // Usage telemetry (hermes skills_tool bump_view).
                crate::skill_usage::bump_view(&ctx.home, &skill.name);
                let mut result = json!({
                    "success": true,
                    "skill": skill.name,
                    "description": skill.description,
                    "content": content,
                    "linked_files": skills::linked_files(&skill.path),
                });
                if !declared.is_empty() {
                    result["registered_environment_variables"] = json!(accepted);
                }
                Ok(result)
            }
        })
        .toolset("skills")
        .emoji("📖")
        .build()
        .expect("skill_view builds")
}

fn skill_manage_tool() -> crate::tools::Tool {
    tool("skill_manage")
        .description(
            "Create, update, or delete a skill. A skill is a directory with a SKILL.md file \
             containing YAML frontmatter (name, description) and markdown instructions.",
        )
        .parameters(json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["create", "update", "delete"], "description": "What to do"},
                "name": {"type": "string", "description": "Skill name (kebab-case directory name)"},
                "description": {"type": "string", "description": "For create/update: one-line description"},
                "content": {"type": "string", "description": "For create/update: full SKILL.md body (markdown instructions)"}
            },
            "required": ["action", "name"]
        }))
        .handler(|args, ctx| async move {
            let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
            let Some(name) = args.get("name").and_then(|v| v.as_str()) else {
                return Ok(json!({"success": false, "error": "skill_manage: 'name' is required"}));
            };
            if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
                return Ok(json!({"success": false, "error": "skill name must be alphanumeric/-/_ only"}));
            }
            let dir = skills_dir(&ctx);
            let skill_path = dir.join(name);
            match action {
                "create" | "update" => {
                    let description = args.get("description").and_then(|v| v.as_str()).unwrap_or("");
                    let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
                    if skill_path.exists() && action == "create" {
                        return Ok(json!({"success": false, "error": format!("Skill '{}' already exists — use action=update", name)}));
                    }
                    std::fs::create_dir_all(&skill_path).ok();
                    let body = format!(
                        "---\nname: {}\ndescription: {}\n---\n\n{}\n",
                        name, description, content
                    );
                    std::fs::write(skill_path.join("SKILL.md"), body)
                        .map_err(|e| crate::error::AgentError::tool(format!("write skill: {}", e)))?;
                    if action == "create" {
                        crate::skill_usage::mark_agent_created(&ctx.home, name);
                    } else {
                        crate::skill_usage::bump_patch(&ctx.home, name);
                    }
                    Ok(json!({
                        "success": true,
                        "action": action,
                        "skill": name,
                        "path": skill_path.display().to_string(),
                    }))
                }
                "delete" => {
                    if !skill_path.exists() {
                        return Ok(json!({"success": false, "error": format!("Skill '{}' not found", name)}));
                    }
                    std::fs::remove_dir_all(&skill_path)
                        .map_err(|e| crate::error::AgentError::tool(format!("delete skill: {}", e)))?;
                    crate::skill_usage::forget(&ctx.home, name);
                    Ok(json!({"success": true, "action": "delete", "skill": name}))
                }
                other => Ok(json!({"success": false, "error": format!("Unknown action: {}", other)})),
            }
        })
        .toolset("skills")
        .emoji("🛠️")
        .build()
        .expect("skill_manage builds")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn test_skill_create_view_list() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Arc::new(ToolContext::new().with_home(dir.path()));

        let manage = skill_manage_tool();
        let result = (manage.handler)(
            json!({"action": "create", "name": "test-skill", "description": "A test", "content": "# Do the thing"}),
            ctx.clone(),
        )
        .await
        .unwrap();
        assert_eq!(result["success"], json!(true));

        let list = skills_list_tool();
        let result = (list.handler)(json!({}), ctx.clone()).await.unwrap();
        assert_eq!(result["skills"].as_array().unwrap().len(), 1);

        let view = skill_view_tool();
        let result = (view.handler)(json!({"name": "test-skill"}), ctx)
            .await
            .unwrap();
        assert!(result["content"].as_str().unwrap().contains("Do the thing"));
    }
}

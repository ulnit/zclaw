//! Todo tool — port of hermes' tools/todo_tool.py
//!
//! Session-scoped task list. Stored per-session in `<home>/sessions/<id>.todos.json`
//! so it survives process restarts but not session boundaries.

use crate::error::Result;
use crate::tools::{tool, ToolContext, ToolRegistry};
use serde::{Deserialize, Serialize};
use serde_json::json;

pub fn register(registry: &mut ToolRegistry) {
    registry.register(todo_tool());
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
}

impl std::fmt::Display for TodoStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TodoStatus::Pending => write!(f, "pending"),
            TodoStatus::InProgress => write!(f, "in_progress"),
            TodoStatus::Completed => write!(f, "completed"),
            TodoStatus::Cancelled => write!(f, "cancelled"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoItem {
    pub id: String,
    pub content: String,
    pub status: TodoStatus,
}

fn todo_path(ctx: &ToolContext) -> std::path::PathBuf {
    ctx.home
        .join("sessions")
        .join(format!("{}.todos.json", ctx.session_id))
}

fn load_todos(ctx: &ToolContext) -> Vec<TodoItem> {
    std::fs::read_to_string(todo_path(ctx))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_todos(ctx: &ToolContext, todos: &[TodoItem]) -> Result<()> {
    let path = todo_path(ctx);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let content = serde_json::to_string_pretty(todos)
        .map_err(|e| crate::error::AgentError::tool(format!("serialize todos: {}", e)))?;
    std::fs::write(&path, content)
        .map_err(|e| crate::error::AgentError::tool(format!("write todos: {}", e)))?;
    Ok(())
}

fn render(todos: &[TodoItem]) -> serde_json::Value {
    let items: Vec<serde_json::Value> = todos
        .iter()
        .map(|item| {
            json!({
                "id": item.id,
                "content": item.content,
                "status": item.status.to_string(),
            })
        })
        .collect();
    let in_progress = todos
        .iter()
        .filter(|t| t.status == TodoStatus::InProgress)
        .count();
    let completed = todos
        .iter()
        .filter(|t| t.status == TodoStatus::Completed)
        .count();
    json!({
        "success": true,
        "todos": items,
        "summary": format!("{} total, {} in_progress, {} completed", todos.len(), in_progress, completed),
    })
}

fn todo_tool() -> crate::tools::Tool {
    tool("todo")
        .description(
            "Manage your task list for the current session. Use for complex tasks with 3+ steps \
             or when the user provides multiple tasks. Call with no parameters to read the \
             current list.\n\n\
             Writing:\n\
             - Provide 'todos' array to create/update items\n\
             - merge=false (default): replace the entire list with a fresh plan\n\
             - merge=true: update existing items by id, add any new ones\n\n\
             Each item: {id: string, content: string, status: pending|in_progress|completed|cancelled}\n\
             List order is priority. Only ONE item in_progress at a time.\n\
             Mark items completed immediately when done. If something fails, cancel it and add a \
             revised item.\n\n\
             Always returns the full current list.",
        )
        .parameters(json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "description": "Task items to write. Omit to read current list.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {"type": "string", "description": "Unique item identifier"},
                            "content": {"type": "string", "description": "Task description"},
                            "status": {"type": "string", "enum": ["pending", "in_progress", "completed", "cancelled"], "description": "Current status"}
                        },
                        "required": ["id", "content", "status"]
                    }
                },
                "merge": {
                    "type": "boolean",
                    "description": "true: update existing items by id, add new ones. false (default): replace the entire list.",
                    "default": false
                }
            },
            "required": []
        }))
        .handler(|args, ctx| async move {
            let merge = args.get("merge").and_then(|v| v.as_bool()).unwrap_or(false);
            let Some(items) = args.get("todos").and_then(|v| v.as_array()) else {
                return Ok(render(&load_todos(&ctx)));
            };

            let mut new_items: Vec<TodoItem> = Vec::new();
            for (idx, item) in items.iter().enumerate() {
                let id = item
                    .get("id")
                    .and_then(|v| v.as_str())
                    .map(String::from)
                    .unwrap_or_else(|| format!("{}", idx + 1));
                let Some(content) = item.get("content").and_then(|v| v.as_str()) else {
                    return Ok(json!({"success": false, "error": format!("todos[{}]: 'content' is required", idx)}));
                };
                let status = match item.get("status").and_then(|v| v.as_str()).unwrap_or("pending") {
                    "in_progress" => TodoStatus::InProgress,
                    "completed" => TodoStatus::Completed,
                    "cancelled" => TodoStatus::Cancelled,
                    _ => TodoStatus::Pending,
                };
                new_items.push(TodoItem {
                    id,
                    content: content.to_string(),
                    status,
                });
            }

            let in_progress = new_items.iter().filter(|t| t.status == TodoStatus::InProgress).count();
            if in_progress > 1 {
                return Ok(json!({
                    "success": false,
                    "error": format!("Only ONE item can be in_progress at a time (got {}). Mark others pending.", in_progress)
                }));
            }

            let final_items = if merge {
                let mut existing = load_todos(&ctx);
                for item in new_items {
                    if let Some(slot) = existing.iter_mut().find(|e| e.id == item.id) {
                        *slot = item;
                    } else {
                        existing.push(item);
                    }
                }
                existing
            } else {
                new_items
            };

            save_todos(&ctx, &final_items)?;
            Ok(render(&final_items))
        })
        .toolset("todo")
        .emoji("📝")
        .build()
        .expect("todo builds")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn test_todo_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Arc::new(ToolContext::new().with_home(dir.path()));
        let tool = todo_tool();

        let result = (tool.handler)(
            json!({"todos": [
                {"id": "1", "content": "Explore repo", "status": "in_progress"},
                {"id": "2", "content": "Write code", "status": "pending"}
            ]}),
            ctx.clone(),
        )
        .await
        .unwrap();
        assert_eq!(result["success"], json!(true));
        assert_eq!(result["todos"].as_array().unwrap().len(), 2);

        // merge update
        let result = (tool.handler)(
            json!({"todos": [{"id": "1", "content": "Explore repo", "status": "completed"}], "merge": true}),
            ctx.clone(),
        )
        .await
        .unwrap();
        assert_eq!(result["todos"].as_array().unwrap().len(), 2);
        assert_eq!(result["todos"][0]["status"], json!("completed"));

        // two in_progress rejected
        let result = (tool.handler)(
            json!({"todos": [
                {"id": "a", "content": "x", "status": "in_progress"},
                {"id": "b", "content": "y", "status": "in_progress"}
            ]}),
            ctx,
        )
        .await
        .unwrap();
        assert_eq!(result["success"], json!(false));
    }
}

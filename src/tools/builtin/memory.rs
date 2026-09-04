//! Memory tool — port of hermes' tools/memory_tool.py
//!
//! Persistent memory stored as markdown entries in `<home>/memory/MEMORY.md`
//! (agent notes) and `<home>/memory/USER.md` (user profile). Supports
//! add/replace/remove, batched atomically via `operations`.

use crate::error::Result;
use crate::tools::{tool, ToolContext, ToolRegistry};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;

pub fn register(registry: &mut ToolRegistry) {
    registry.register(memory_tool());
}

fn memory_tool() -> crate::tools::Tool {
    tool("memory")
        .description(
            "Save durable facts to persistent memory that survive across sessions. Memory is \
             injected into every future turn, so keep entries compact and high-signal.\n\n\
             HOW: make ALL your changes in ONE call via an 'operations' array (each item: \
             {action, content?, old_text?}). The batch applies atomically and the char limit is \
             checked only on the FINAL result — so a single call can remove/replace stale entries \
             to free room AND add new ones. Use the bare action/content/old_text fields only for \
             a single lone change.\n\n\
             WHEN: save proactively when the user states a preference, correction, or personal \
             detail, or you learn a stable fact about their environment, conventions, or workflow.\n\n\
             IF FULL: an add is rejected with the current entries shown. Reissue as ONE batch that \
             removes or shortens enough stale entries and adds the new one together.\n\n\
             TARGETS: 'user' = who the user is (name, role, preferences, style). 'memory' = your \
             notes (environment, conventions, tool quirks, lessons).\n\n\
             SKIP: trivial/obvious info, easily re-discovered facts, raw data dumps, task progress.",
        )
        .parameters(json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["add", "replace", "remove"], "description": "The action to perform (single-op shape). Omit when using 'operations'."},
                "target": {"type": "string", "enum": ["memory", "user"], "description": "Which memory store: 'memory' for personal notes, 'user' for user profile.", "default": "memory"},
                "content": {"type": "string", "description": "The entry content. Required for 'add' and 'replace' (single-op shape)."},
                "old_text": {"type": "string", "description": "REQUIRED for 'replace' and 'remove' (single-op shape): text identifying the entry to change."},
                "operations": {
                    "type": "array",
                    "description": "Batch of operations applied atomically. Each: {action: add|replace|remove, target?: memory|user, content?, old_text?}",
                    "items": {"type": "object"}
                }
            },
            "required": []
        }))
        .handler(|args, ctx| async move { memory_handler(args, ctx) })
        .toolset("memory")
        .emoji("🧠")
        .build()
        .expect("memory builds")
}

#[derive(Debug, Clone)]
struct MemoryOp {
    action: String,
    target: String,
    content: Option<String>,
    old_text: Option<String>,
}

fn memory_file(home: &std::path::Path, target: &str) -> PathBuf {
    let dir = home.join("memory");
    if target == "user" {
        dir.join("USER.md")
    } else {
        dir.join("MEMORY.md")
    }
}

/// Parse `- ` bullet entries from memory file content (shared with the
/// learning graph / journey mutations so indices stay aligned).
pub fn read_entries(content: &str) -> Vec<String> {
    content
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                None
            } else {
                Some(trimmed.trim_start_matches("- ").to_string())
            }
        })
        .collect()
}

/// Serialize entries back to the bullet file format.
pub fn entries_to_content(entries: &[String]) -> String {
    if entries.is_empty() {
        String::new()
    } else {
        entries
            .iter()
            .map(|e| format!("- {}", e))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n"
    }
}

fn load_entries(path: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .map(|content| read_entries(&content))
        .unwrap_or_default()
}

fn save_entries(path: &std::path::Path, entries: &[String]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let content = entries_to_content(entries);
    std::fs::write(path, content)
        .map_err(|e| crate::error::AgentError::tool(format!("write memory: {}", e)))?;
    Ok(())
}

fn char_limit(config: &crate::config::UlncLawConfig, target: &str) -> usize {
    if target == "user" {
        config.memory.user_char_limit
    } else {
        config.memory.memory_char_limit
    }
}

fn entries_text(entries: &[String]) -> String {
    entries.join("\n")
}

fn find_entry_index(entries: &[String], old_text: &str) -> Option<usize> {
    let needle = old_text.trim().to_lowercase();
    // Exact entry match first, then substring.
    entries
        .iter()
        .position(|e| e.to_lowercase() == needle)
        .or_else(|| {
            entries
                .iter()
                .position(|e| e.to_lowercase().contains(&needle))
        })
}

fn memory_handler(args: serde_json::Value, ctx: Arc<ToolContext>) -> Result<serde_json::Value> {
    // Build the op list: single-op fields or operations array.
    let mut ops: Vec<MemoryOp> = Vec::new();
    if let Some(batch) = args.get("operations").and_then(|v| v.as_array()) {
        for item in batch {
            let action = item
                .get("action")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if action.is_empty() {
                return Ok(
                    json!({"success": false, "error": "Each operation needs an 'action' (add|replace|remove)"}),
                );
            }
            ops.push(MemoryOp {
                action,
                target: item
                    .get("target")
                    .and_then(|v| v.as_str())
                    .unwrap_or("memory")
                    .to_string(),
                content: item
                    .get("content")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                old_text: item
                    .get("old_text")
                    .and_then(|v| v.as_str())
                    .map(String::from),
            });
        }
    } else if let Some(action) = args.get("action").and_then(|v| v.as_str()) {
        ops.push(MemoryOp {
            action: action.to_string(),
            target: args
                .get("target")
                .and_then(|v| v.as_str())
                .unwrap_or("memory")
                .to_string(),
            content: args
                .get("content")
                .and_then(|v| v.as_str())
                .map(String::from),
            old_text: args
                .get("old_text")
                .and_then(|v| v.as_str())
                .map(String::from),
        });
    }

    if ops.is_empty() {
        // Read-only: return current memory.
        let memory = load_entries(&memory_file(&ctx.home, "memory"));
        let user = load_entries(&memory_file(&ctx.home, "user"));
        return Ok(json!({
            "success": true,
            "memory": memory,
            "user": user,
            "memory_chars": entries_text(&memory).len(),
            "memory_limit": char_limit(&ctx.config, "memory"),
            "user_chars": entries_text(&user).len(),
            "user_limit": char_limit(&ctx.config, "user"),
        }));
    }

    // Load both stores; apply all ops atomically; validate limits on final state.
    let memory_path = memory_file(&ctx.home, "memory");
    let user_path = memory_file(&ctx.home, "user");
    let mut memory_entries = load_entries(&memory_path);
    let mut user_entries = load_entries(&user_path);
    let mut changes: Vec<String> = Vec::new();

    for op in &ops {
        let (entries, target_name) = if op.target == "user" {
            (&mut user_entries, "user")
        } else {
            (&mut memory_entries, "memory")
        };
        match op.action.as_str() {
            "add" => {
                let Some(content) = op.content.as_deref() else {
                    return Ok(json!({"success": false, "error": "add requires 'content'"}));
                };
                let content = content.trim();
                if content.is_empty() {
                    return Ok(json!({"success": false, "error": "add: content is empty"}));
                }
                if entries.iter().any(|e| e.eq_ignore_ascii_case(content)) {
                    changes.push(format!("{}: already present, skipped", target_name));
                    continue;
                }
                entries.push(content.to_string());
                changes.push(format!("{}: added '{}'", target_name, content));
            }
            "replace" => {
                let Some(old_text) = op.old_text.as_deref() else {
                    return Ok(json!({"success": false, "error": "replace requires 'old_text'"}));
                };
                let Some(content) = op.content.as_deref() else {
                    return Ok(json!({"success": false, "error": "replace requires 'content'"}));
                };
                let Some(idx) = find_entry_index(entries, old_text) else {
                    return Ok(json!({
                        "success": false,
                        "error": format!("replace: no entry matching '{}' in {}. Current entries: {}", old_text, target_name, entries_text(entries))
                    }));
                };
                entries[idx] = content.trim().to_string();
                changes.push(format!("{}: replaced entry {}", target_name, idx + 1));
            }
            "remove" => {
                let Some(old_text) = op.old_text.as_deref() else {
                    return Ok(json!({"success": false, "error": "remove requires 'old_text'"}));
                };
                let Some(idx) = find_entry_index(entries, old_text) else {
                    return Ok(json!({
                        "success": false,
                        "error": format!("remove: no entry matching '{}' in {}", old_text, target_name)
                    }));
                };
                let removed = entries.remove(idx);
                changes.push(format!("{}: removed '{}'", target_name, removed));
            }
            other => {
                return Ok(
                    json!({"success": false, "error": format!("Unknown action: {}", other)}),
                );
            }
        }
    }

    // Char-limit check on FINAL state (hermes contract).
    for (entries, target, path) in [
        (&memory_entries, "memory", &memory_path),
        (&user_entries, "user", &user_path),
    ] {
        let text = entries_text(entries);
        let limit = char_limit(&ctx.config, target);
        if text.len() > limit {
            return Ok(json!({
                "success": false,
                "error": format!(
                    "{} would exceed its limit after this batch ({} > {} chars). Remove or shorten entries in the same batch. Current entries:\n{}",
                    target, text.len(), limit, text
                ),
            }));
        }
        save_entries(path, entries)?;
    }

    Ok(json!({
        "success": true,
        "changes": changes,
        "memory_chars": entries_text(&memory_entries).len(),
        "memory_limit": char_limit(&ctx.config, "memory"),
        "user_chars": entries_text(&user_entries).len(),
        "user_limit": char_limit(&ctx.config, "user"),
    }))
}

/// Load memory content for system-prompt injection (used by Agent/PromptBuilder).
pub fn load_memory_for_prompt(home: &std::path::Path) -> Option<String> {
    let memory = load_entries(&memory_file(home, "memory"));
    let user = load_entries(&memory_file(home, "user"));
    if memory.is_empty() && user.is_empty() {
        return None;
    }
    let mut out = String::new();
    if !user.is_empty() {
        out.push_str("### User profile\n");
        for entry in &user {
            out.push_str(&format!("- {}\n", entry));
        }
    }
    if !memory.is_empty() {
        out.push_str("### Agent memory\n");
        for entry in &memory {
            out.push_str(&format!("- {}\n", entry));
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_replace_remove() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Arc::new(ToolContext::new().with_home(dir.path()));

        let result = memory_handler(
            json!({"action": "add", "target": "memory", "content": "User prefers Rust"}),
            ctx.clone(),
        )
        .unwrap();
        assert_eq!(result["success"], json!(true));

        let result = memory_handler(
            json!({"action": "add", "target": "user", "content": "Name: Alice"}),
            ctx.clone(),
        )
        .unwrap();
        assert_eq!(result["success"], json!(true));

        let result = memory_handler(json!({}), ctx.clone()).unwrap();
        assert_eq!(result["memory"][0], json!("User prefers Rust"));
        assert_eq!(result["user"][0], json!("Name: Alice"));

        // Batch: replace + add
        let result = memory_handler(
            json!({"operations": [
                {"action": "replace", "target": "memory", "old_text": "prefers Rust", "content": "User prefers Rust and Go"},
                {"action": "add", "target": "memory", "content": "Deploy target: musl"}
            ]}),
            ctx.clone(),
        )
        .unwrap();
        assert_eq!(result["success"], json!(true));

        let result = memory_handler(json!({}), ctx.clone()).unwrap();
        assert_eq!(result["memory"].as_array().unwrap().len(), 2);

        let prompt = load_memory_for_prompt(dir.path()).unwrap();
        assert!(prompt.contains("Alice"));
        assert!(prompt.contains("musl"));
    }

    #[test]
    fn test_char_limit_enforced_on_final_state() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = crate::config::UlncLawConfig::default();
        config.memory.user_char_limit = 30;
        let ctx = Arc::new(ToolContext::new().with_home(dir.path()).with_config(config));
        let result = memory_handler(
            json!({"action": "add", "target": "user", "content": "x".repeat(50)}),
            ctx,
        )
        .unwrap();
        assert_eq!(result["success"], json!(false));
        assert!(result["error"].as_str().unwrap().contains("exceed"));
    }
}

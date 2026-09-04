//! session_search — port of hermes' tools/session_search_tool.py
//!
//! FTS-backed retrieval over the SQLite message store. Two shapes:
//! discovery (query → top sessions with snippets) and scroll (session_id →
//! its messages).

use crate::tools::{tool, ToolAvailability, ToolRegistry};
use serde_json::json;

pub fn register(registry: &mut ToolRegistry) {
    registry.register(session_search_tool());
}

fn check_store() -> ToolAvailability {
    // Availability is per-context (store wired at runtime); keep the tool
    // registered and let the handler emit a helpful error when unwired.
    ToolAvailability::available()
}

fn session_search_tool() -> crate::tools::Tool {
    tool("session_search")
        .description(
            "Search past sessions stored in the local session DB, or scroll inside one. \
             FTS-backed retrieval over the SQLite message store. No LLM calls — every shape \
             returns actual messages from the DB.\n\n\
             SHAPES:\n\
             1) DISCOVERY — pass `query`: returns top matching sessions with snippet excerpts \
                and the first/last messages for orientation.\n\
             2) SCROLL — pass `session_id` (plus optional offset/limit): returns that session's \
                messages for close reading.",
        )
        .parameters(json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Search terms (discovery shape)"},
                "session_id": {"type": "string", "description": "Session to scroll (scroll shape)"},
                "limit": {"type": "integer", "description": "Max sessions for discovery (default 3), max messages for scroll (default 20)", "default": 3},
                "offset": {"type": "integer", "description": "Message offset for scroll shape", "default": 0}
            },
            "required": []
        }))
        .handler(|args, ctx| async move {
            let Some(store) = ctx.store.clone() else {
                return Ok(json!({
                    "success": false,
                    "error": "session_search: no state database is wired into this run (open the CLI with persistence enabled)."
                }));
            };

            if let Some(session_id) = args.get("session_id").and_then(|v| v.as_str()) {
                let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
                let messages = match store.load_messages(session_id) {
                    Ok(messages) => messages,
                    Err(e) => return Ok(json!({"success": false, "error": e.to_string()})),
                };
                if messages.is_empty() {
                    return Ok(json!({"success": false, "error": format!("No messages found for session {}", session_id)}));
                }
                let total = messages.len();
                let items: Vec<serde_json::Value> = messages
                    .iter()
                    .skip(offset)
                    .take(limit)
                    .map(|message| {
                        json!({
                            "role": message.role.to_string(),
                            "content": message.content.clone().unwrap_or_default(),
                            "tool_name": message.name,
                        })
                    })
                    .collect();
                return Ok(json!({
                    "success": true,
                    "session_id": session_id,
                    "messages": items,
                    "total_messages": total,
                    "next_offset": if offset + limit < total { Some(offset + limit) } else { None },
                }));
            }

            let Some(query) = args.get("query").and_then(|v| v.as_str()) else {
                return Ok(json!({"success": false, "error": "session_search: pass either 'query' (discovery) or 'session_id' (scroll)"}));
            };
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(3) as usize;
            let hits = match store.search_messages(query, limit * 4) {
                Ok(hits) => hits,
                Err(e) => return Ok(json!({"success": false, "error": e.to_string()})),
            };
            let mut sessions = Vec::new();
            let mut seen = std::collections::HashSet::new();
            for (session_id, snippet) in hits {
                if !seen.insert(session_id.clone()) {
                    continue;
                }
                let messages = store.load_messages(&session_id).unwrap_or_default();
                let bookend: Vec<serde_json::Value> = messages
                    .iter()
                    .filter(|m| matches!(m.role, crate::provider::Role::User | crate::provider::Role::Assistant))
                    .take(3)
                    .map(|m| json!({"role": m.role.to_string(), "content": m.content.clone().unwrap_or_default()}))
                    .collect();
                sessions.push(json!({
                    "session_id": session_id,
                    "snippet": snippet,
                    "message_count": messages.len(),
                    "bookend_start": bookend,
                }));
                if sessions.len() >= limit {
                    break;
                }
            }
            Ok(json!({
                "success": true,
                "query": query,
                "sessions": sessions,
            }))
        })
        .toolset("session_search")
        .emoji("🔎")
        .check_fn(check_store)
        .build()
        .expect("session_search builds")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{Message, Role};
    use crate::session::sqlite::SqliteSessionStore;
    use crate::tools::ToolContext;
    use std::sync::Arc;

    #[tokio::test]
    async fn test_discovery_and_scroll() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(SqliteSessionStore::open(dir.path().join("state.db")).unwrap());
        let sid = store.create_session("cli", None, None).unwrap();
        for text in ["discussed the auth refactor plan", "then shipped it"] {
            store
                .append_message(
                    &sid,
                    &Message {
                        role: Role::User,
                        content: Some(text.into()),
                        tool_calls: None,
                        tool_call_id: None,
                        name: None,
                    },
                )
                .unwrap();
        }
        let ctx = Arc::new(ToolContext::new().with_store(store.clone()));
        let tool = session_search_tool();

        let result = (tool.handler)(json!({"query": "auth refactor"}), ctx.clone())
            .await
            .unwrap();
        assert_eq!(result["success"], json!(true));
        assert_eq!(result["sessions"].as_array().unwrap().len(), 1);
        assert_eq!(result["sessions"][0]["session_id"], json!(sid));

        let result = (tool.handler)(json!({"session_id": sid}), ctx)
            .await
            .unwrap();
        assert_eq!(result["total_messages"], json!(2));
    }
}

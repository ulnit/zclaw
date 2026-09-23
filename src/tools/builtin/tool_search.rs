//! tool_search — aligned with hermes' tools/tool_search.py
//!
//! Keyword search over the registered tool catalog (names + descriptions).
//! Becomes important when MCP servers add many tools to the schema.

use crate::tools::{tool, ToolRegistry};
use serde_json::json;

pub fn register(registry: &mut ToolRegistry) {
    registry.register(tool_search_tool());
}

fn tool_search_tool() -> crate::tools::Tool {
    tool("tool_search")
        .description(
            "Search the registered tool catalog by keyword (names and descriptions). Use when \
             unsure which tool fits a task — returns matching tool names with descriptions.",
        )
        .parameters(json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Keyword(s) to search for, e.g. 'file', 'browser', 'schedule'"},
                "limit": {"type": "integer", "description": "Max results (default 10)", "default": 10}
            },
            "required": ["query"]
        }))
        .handler(|args, ctx| async move {
            let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("").to_lowercase();
            if query.is_empty() {
                return Ok(json!({"success": false, "error": "tool_search: 'query' is required"}));
            }
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
            let terms: Vec<&str> = query.split_whitespace().collect();
            let registry = ctx.tool_registry_snapshot();
            let mut matches: Vec<serde_json::Value> = Vec::new();
            for def in registry {
                let name = def.name.to_lowercase();
                let description = def.description.to_lowercase();
                let score: usize = terms
                    .iter()
                    .map(|term| {
                        let mut score = 0usize;
                        if name.contains(term) {
                            score += 3;
                        }
                        if description.contains(term) {
                            score += 1;
                        }
                        score
                    })
                    .sum();
                if score > 0 {
                    matches.push(json!({
                        "name": def.name,
                        "description": def.description,
                        "score": score,
                    }));
                }
            }
            matches.sort_by(|a, b| b["score"].as_u64().unwrap_or(0).cmp(&a["score"].as_u64().unwrap_or(0)));
            matches.truncate(limit);
            Ok(json!({"success": true, "matches": matches, "count": matches.len()}))
        })
        .toolset("default")
        .emoji("🧭")
        .build()
        .expect("tool_search builds")
}

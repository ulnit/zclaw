//! Clarify tool — aligned with hermes' tools/clarify_tool.py
//!
//! Asks the user a question (single/multi-select or open-ended) via the
//! clarify callback installed on the ToolContext. Non-interactive runs get a
//! structured error so the model can proceed on its best judgment.

use crate::tools::{tool, ToolRegistry};
use serde_json::json;

pub fn register(registry: &mut ToolRegistry) {
    registry.register(clarify_tool());
}

fn clarify_tool() -> crate::tools::Tool {
    tool("clarify")
        .description(
            "Ask the user a question when you need clarification, feedback, or a decision before \
             proceeding. Supports three modes:\n\
             1. Single-select multiple choice — provide up to 4 choices.\n\
             2. Multi-select multiple choice — set multi_select=true.\n\
             3. Open-ended — omit choices entirely.\n\n\
             CRITICAL: when you are offering options, put each option ONLY in the `choices` \
             array — NEVER enumerate the options inside the `question` text.\n\n\
             Do NOT use this tool for simple yes/no confirmation of dangerous commands.",
        )
        .parameters(json!({
            "type": "object",
            "properties": {
                "question": {"type": "string", "description": "The question to ask the user. Keep it short and specific."},
                "choices": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Up to 4 selectable options. Omit for an open-ended question.",
                    "maxItems": 4
                },
                "multi_select": {"type": "boolean", "description": "Allow selecting multiple choices.", "default": false}
            },
            "required": ["question"]
        }))
        .handler(|args, ctx| async move {
            let Some(question) = args.get("question").and_then(|v| v.as_str()) else {
                return Ok(json!({"success": false, "error": "clarify: 'question' is required"}));
            };
            let choices: Vec<String> = args
                .get("choices")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|item| item.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            if choices.len() > 4 {
                return Ok(json!({"success": false, "error": "clarify: at most 4 choices"}));
            }
            let multi_select = args.get("multi_select").and_then(|v| v.as_bool()).unwrap_or(false);

            let Some(clarify) = ctx.clarify.clone() else {
                return Ok(json!({
                    "success": false,
                    "error": "No user is available to answer (non-interactive session). Proceed with your best judgment and state your assumptions in the final answer."
                }));
            };

            match clarify(question.to_string(), choices, multi_select).await {
                Ok(answer) => Ok(json!({"success": true, "user_response": answer})),
                Err(e) => Ok(json!({"success": false, "error": format!("clarify failed: {}", e)})),
            }
        })
        .toolset("clarify")
        .emoji("❓")
        .build()
        .expect("clarify builds")
}

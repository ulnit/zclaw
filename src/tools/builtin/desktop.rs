//! Desktop GUI tools — port of hermes' `close_terminal_tool.py`,
//! `read_terminal_tool.py`, `focus_pane_tool.py`, `open_preview_tool.py`.
//!
//! Each `terminal(background=true)` process can be mirrored as a
//! read-only tab in a desktop terminal pane; these tools let the agent
//! manage that surface without touching the processes themselves. They
//! are gated on `ULNCLAW_DESKTOP` (hermes: `HERMES_DESKTOP`) and route
//! through the [`crate::desktop`] bridge a host application installs —
//! outside a desktop host they are never registered, and at runtime they
//! report "desktop only" if the bridge was cleared.

use crate::tools::{tool, ToolAvailability, ToolRegistry};
use serde_json::json;

const PANES: &[&str] = &["chat", "files", "terminal", "review", "sessions"];

/// Desktop GUI only — `ULNCLAW_DESKTOP` is set on the agent a host app spawns.
fn check_desktop_requirements() -> ToolAvailability {
    if crate::desktop::desktop_env_enabled() {
        ToolAvailability::available()
    } else {
        ToolAvailability::unavailable("desktop GUI tools need ULNCLAW_DESKTOP=1 (host app context)")
    }
}

pub fn register(registry: &mut ToolRegistry) {
    registry.register(close_terminal_tool());
    registry.register(read_terminal_tool());
    registry.register(focus_pane_tool());
    registry.register(open_preview_tool());
    registry.register(react_to_message_tool());
}

fn react_to_message_tool() -> crate::tools::Tool {
    tool("react_to_message")
        .description(
            "React to a message with a single emoji, the way you'd tapback in iMessage.              Reach for it when a reaction is what a person would do: something funny gets              a 😂, warmth gets a ❤️, a plan you're on board with gets a 👍 — then just              carry on with whatever the message actually needs. If a reaction says it              all, it can BE the reply (skip the redundant 'sounds good!' turn). Use it              like a person would: occasionally, when felt — not on every message, and              never as a status signal. NEVER narrate or explain a reaction ('I reacted              with...', 'Reacting now') — the emoji appearing on the bubble is the whole              point, and commentary kills it. Defaults to the user's most recent message.              One reaction per message: a different emoji replaces yours, an empty string              retracts it.",
        )
        .parameters(json!({
            "type": "object",
            "properties": {
                "emoji": {
                    "type": "string",
                    "description": "The emoji to react with (e.g. '❤️', '😂', '👍'). Pass an empty string to remove your reaction."
                },
                "message_row_id": {
                    "type": "integer",
                    "description": "Optional. The specific message to react to. Omit to react to the user's latest message, which is almost always what you want."
                },
                "messages_back": {
                    "type": "integer",
                    "description": "Optional. React to an EARLIER user message: 1 = the one before the latest, 2 = two before, and so on."
                }
            },
            "required": ["emoji"]
        }))
        .handler(|args, ctx| async move {
            if !ctx.config.display.message_reactions {
                return Ok(json!({
                    "success": false,
                    "error": "Message reactions are disabled (set [display] message_reactions = true to enable)."
                }));
            }
            let emoji = args
                .get("emoji")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let Some(store) = ctx.store.as_ref() else {
                return Ok(json!({
                    "success": false,
                    "error": "No active session — reactions need a persisted conversation."
                }));
            };
            let session_key = ctx.session_id.clone();

            // Default target: the latest user message (hermes photon
            // precedent — the model never threads row ids). `messages_back`
            // steps to earlier user turns for retroactive reactions.
            let mut target_role = "user".to_string();
            let row_id: i64 = if let Some(id) = args.get("message_row_id").and_then(|v| v.as_i64()) {
                match store.message_role(&session_key, id) {
                    Some(role) => {
                        target_role = role;
                        id
                    }
                    None => {
                        return Ok(json!({
                            "success": false,
                            "error": format!("Message {id} is not part of this conversation.")
                        }))
                    }
                }
            } else {
                let back = args.get("messages_back").and_then(|v| v.as_i64()).unwrap_or(0).max(0);
                match store.latest_message_row_id(&session_key, "user", back, true) {
                    Some(id) => id,
                    None => {
                        let error = if back > 0 {
                            format!("No user message found {back} back.")
                        } else {
                            "No user message to react to yet.".to_string()
                        };
                        return Ok(json!({"success": false, "error": error}));
                    }
                }
            };

            let emoji_opt = if emoji.is_empty() { None } else { Some(emoji.as_str()) };
            let Some(reactions) = store.set_message_reaction(&session_key, row_id, emoji_opt, "agent") else {
                return Ok(json!({
                    "success": false,
                    "error": format!("Message {row_id} is not part of this conversation.")
                }));
            };

            // Paint it live. A missing bridge (non-desktop surface) is not an
            // error — the reaction is persisted either way and shows on the
            // next load. `role` lets the renderer match a live message that
            // doesn't know its durable row id yet.
            crate::desktop::emit(
                &session_key,
                "message.reaction",
                &json!({"row_id": row_id, "reactions": reactions, "role": target_role}),
            );

            Ok(json!({"success": true, "row_id": row_id, "reactions": reactions}))
        })
        .toolset("terminal")
        .emoji("💛")
        .check_fn(check_desktop_requirements)
        .build()
        .expect("react_to_message builds")
}

fn close_terminal_tool() -> crate::tools::Tool {
    tool("close_terminal")
        .description(
            "Close the read-only terminal tab for one of your background processes in the \
             ulnclaw desktop GUI (the tabs mirroring terminal(background=true) runs). This \
             does NOT kill the process — it only drops the tab/view; the output keeps \
             buffering and the user can reopen it from the status stack. Use it to tidy up \
             when a background process's live terminal is no longer worth showing. To \
             actually stop the process, use process(action='kill') instead.",
        )
        .parameters(json!({
            "type": "object",
            "properties": {
                "process_id": {
                    "type": "string",
                    "description": "The background process's session id (from terminal(background=true) output or process(action='list')) whose tab should be closed."
                }
            },
            "required": ["process_id"]
        }))
        .handler(|args, ctx| async move {
            let pid = args
                .get("process_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if pid.is_empty() {
                return Ok(json!({
                    "status": "error",
                    "error": "process_id is required (the background process whose tab to close)."
                }));
            }
            Ok(crate::tools::builtin::terminal::request_close_terminal(&ctx.session_id, &pid))
        })
        .toolset("terminal")
        .emoji("🖥️")
        .check_fn(check_desktop_requirements)
        .build()
        .expect("close_terminal builds")
}

fn read_terminal_tool() -> crate::tools::Tool {
    tool("read_terminal")
        .description(
            "Read what's currently shown in the in-app terminal pane of the ulnclaw desktop \
             GUI (the embedded shell beside this chat). Call with no arguments to get the \
             visible screen plus the total line count (`total_lines`). To page through \
             scrollback, pass `start_line` (0 = oldest line) and `count`; valid lines are \
             [0, total_lines). Returns JSON: {total_lines, start, end, viewport_rows, \
             cursor_row, text}.",
        )
        .parameters(json!({
            "type": "object",
            "properties": {
                "start_line": {
                    "type": "integer",
                    "description": "0-indexed first line (0 = oldest). Omit for the visible screen."
                },
                "count": {
                    "type": "integer",
                    "description": "Lines to read from start_line. Defaults to the visible row count."
                }
            }
        }))
        .handler(|args, _ctx| async move {
            // hermes clamps start>=0 and count>=1; both must be integers.
            let parse = |key: &str| -> Result<Option<u64>, String> {
                match args.get(key) {
                    None | Some(serde_json::Value::Null) => Ok(None),
                    Some(v) => v
                        .as_u64()
                        .map(Some)
                        .ok_or_else(|| "start_line and count must be integers.".to_string()),
                }
            };
            let start_line = match parse("start_line") {
                Ok(v) => v,
                Err(e) => return Ok(json!({"error": e})),
            };
            let count = match parse("count") {
                Ok(v) => v.map(|c| c.max(1)),
                Err(e) => return Ok(json!({"error": e})),
            };
            let Some(result) = crate::desktop::read_terminal_window(start_line, count) else {
                return Ok(json!({
                    "error": "read_terminal is only available in the ulnclaw desktop app."
                }));
            };
            match result {
                Err(exc) => Ok(json!({"error": format!("Failed to read terminal: {exc}")})),
                Ok(raw) if raw.trim().is_empty() => Ok(json!({
                    "error": "No in-app terminal is open, or the read timed out."
                })),
                Ok(raw) => {
                    // Desktop answers with a JSON object; pass it through,
                    // else wrap the raw text.
                    match serde_json::from_str::<serde_json::Value>(&raw) {
                        Ok(v) if v.is_object() => Ok(v),
                        _ => Ok(json!({"text": raw})),
                    }
                }
            }
        })
        .toolset("terminal")
        .emoji("🖥️")
        .check_fn(check_desktop_requirements)
        .build()
        .expect("read_terminal builds")
}

fn focus_pane_tool() -> crate::tools::Tool {
    tool("focus_pane")
        .description(
            "Reveal and focus a pane in the ulnclaw desktop app when the user asks to see it \
             — e.g. \"show me the terminal\", \"open the file browser\", \"show the diff\". \
             Panes: chat (the conversation), files (project file browser), terminal (embedded \
             shell), review (git diff), sessions (the session list). To show a URL or file in \
             the preview pane, use open_preview instead.",
        )
        .parameters(json!({
            "type": "object",
            "properties": {
                "pane": {
                    "type": "string",
                    "enum": PANES,
                    "description": "Which pane to reveal."
                }
            },
            "required": ["pane"]
        }))
        .handler(|args, ctx| async move {
            let name = args
                .get("pane")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            if !PANES.contains(&name.as_str()) {
                return Ok(json!({
                    "error": format!("pane must be one of: {}.", PANES.join(", "))
                }));
            }
            if !crate::desktop::emit(&ctx.session_id, "pane.reveal", &json!({"pane": name})) {
                return Ok(json!({
                    "error": "Pane focus is only available in the ulnclaw desktop app."
                }));
            }
            Ok(json!({"success": true, "pane": name}))
        })
        .toolset("terminal")
        .emoji("🪟")
        .check_fn(check_desktop_requirements)
        .build()
        .expect("focus_pane builds")
}

fn open_preview_tool() -> crate::tools::Tool {
    tool("open_preview")
        .description(
            "Open something in the preview pane beside the chat in the ulnclaw desktop app. \
             Use this when the user asks to see a page, dev server, or file in the preview \
             pane — e.g. \"open cnn.com in the preview pane\" or \"preview localhost:3000\". \
             Accepts a web URL (a bare domain like www.cnn.com is fine), a localhost \
             dev-server URL, or a file path (HTML renders live; other files show their \
             contents). The pane opens for the current window only.",
        )
        .parameters(json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "What to preview: a web URL (https://… or a bare domain), a localhost URL (localhost:3000), or a file path."
                },
                "label": {
                    "type": "string",
                    "description": "Optional tab label; defaults to the target's name."
                }
            },
            "required": ["url"]
        }))
        .handler(|args, ctx| async move {
            let target = crate::desktop::normalize_preview_target(
                args.get("url").and_then(|v| v.as_str()).unwrap_or(""),
            );
            if target.is_empty() {
                return Ok(json!({
                    "error": "url is required — a web URL (https://…), a localhost dev server, or a file path to show in the preview pane."
                }));
            }
            let label = args
                .get("label")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if !crate::desktop::emit(
                &ctx.session_id,
                "preview.open",
                &json!({"url": target, "label": label}),
            ) {
                return Ok(json!({
                    "error": "The preview pane is only available in the ulnclaw desktop app."
                }));
            }
            Ok(json!({"success": true, "url": target, "label": label}))
        })
        .toolset("terminal")
        .emoji("🖼️")
        .check_fn(check_desktop_requirements)
        .build()
        .expect("open_preview builds")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desktop::BRIDGE_TEST_LOCK;
    use crate::tools::ToolContext;
    use std::sync::{Arc, Mutex};

    async fn call(tool: &crate::tools::Tool, args: serde_json::Value) -> serde_json::Value {
        let ctx = Arc::new(ToolContext::new().with_session_id("ui-test"));
        (tool.handler)(args, ctx).await.unwrap()
    }

    #[tokio::test]
    async fn close_terminal_requires_process_id_and_desktop() {
        let _guard = BRIDGE_TEST_LOCK.lock().unwrap();
        let tool = close_terminal_tool();
        let out = call(&tool, json!({"process_id": "  "})).await;
        assert!(out["error"]
            .as_str()
            .unwrap()
            .contains("process_id is required"));
        // No sink wired (CLI context) → desktop-only error.
        crate::tools::builtin::terminal::set_close_sink(None);
        let out = call(&tool, json!({"process_id": "bg-1234"})).await;
        assert_eq!(out["status"], json!("error"));
        assert!(out["error"].as_str().unwrap().contains("desktop"));
    }

    #[tokio::test]
    async fn read_terminal_desktop_only_and_validation() {
        let _guard = BRIDGE_TEST_LOCK.lock().unwrap();
        let tool = read_terminal_tool();
        crate::desktop::set_read_terminal_callback(None);
        let out = call(&tool, json!({})).await;
        assert!(out["error"].as_str().unwrap().contains("desktop"));
        let out = call(&tool, json!({"start_line": "abc"})).await;
        assert!(out["error"].as_str().unwrap().contains("integers"));
        // Wired callback returns JSON → passthrough.
        crate::desktop::set_read_terminal_callback(Some(Box::new(|_s, _c| {
            Ok(r#"{"total_lines": 42, "text": "screen"}"#.to_string())
        })));
        let out = call(&tool, json!({"start_line": 3, "count": 0})).await;
        assert_eq!(out["total_lines"], json!(42));
        // Non-JSON answer → wrapped as text.
        crate::desktop::set_read_terminal_callback(Some(Box::new(|_s, _c| {
            Ok("plain screen".to_string())
        })));
        let out = call(&tool, json!({})).await;
        assert_eq!(out["text"], json!("plain screen"));
        crate::desktop::set_read_terminal_callback(None);
    }

    #[tokio::test]
    async fn focus_pane_validates_and_emits() {
        let _guard = BRIDGE_TEST_LOCK.lock().unwrap();
        let tool = focus_pane_tool();
        let out = call(&tool, json!({"pane": "nope"})).await;
        assert!(out["error"]
            .as_str()
            .unwrap()
            .contains("pane must be one of"));
        // No emitter → desktop-only error.
        crate::desktop::set_emitter(None);
        let out = call(&tool, json!({"pane": "Terminal"})).await;
        assert!(out["error"].as_str().unwrap().contains("desktop"));
        // Emitter wired → success + pane.reveal event.
        let events = Arc::new(Mutex::new(Vec::<(String, String, serde_json::Value)>::new()));
        let ev = events.clone();
        crate::desktop::set_emitter(Some(Box::new(move |sid, event, payload| {
            ev.lock()
                .unwrap()
                .push((sid.to_string(), event.to_string(), payload.clone()));
        })));
        let out = call(&tool, json!({"pane": "Terminal"})).await;
        assert_eq!(out["success"], json!(true));
        assert_eq!(out["pane"], json!("terminal"));
        let events = events.lock().unwrap();
        assert_eq!(events[0].0, "ui-test");
        assert_eq!(events[0].1, "pane.reveal");
        assert_eq!(events[0].2["pane"], json!("terminal"));
        crate::desktop::set_emitter(None);
    }

    #[tokio::test]
    async fn open_preview_normalizes_and_emits() {
        let _guard = BRIDGE_TEST_LOCK.lock().unwrap();
        let tool = open_preview_tool();
        let out = call(&tool, json!({"url": "   "})).await;
        assert!(out["error"].as_str().unwrap().contains("url is required"));
        crate::desktop::set_emitter(None);
        let out = call(&tool, json!({"url": "localhost:3000"})).await;
        assert!(out["error"].as_str().unwrap().contains("desktop"));
        let events = Arc::new(Mutex::new(Vec::<(String, String, serde_json::Value)>::new()));
        let ev = events.clone();
        crate::desktop::set_emitter(Some(Box::new(move |sid, event, payload| {
            ev.lock()
                .unwrap()
                .push((sid.to_string(), event.to_string(), payload.clone()));
        })));
        let out = call(&tool, json!({"url": "localhost:3000", "label": " dev "})).await;
        assert_eq!(out["success"], json!(true));
        assert_eq!(out["url"], json!("http://localhost:3000"));
        assert_eq!(out["label"], json!("dev"));
        let events = events.lock().unwrap();
        assert_eq!(events[0].1, "preview.open");
        assert_eq!(events[0].2["url"], json!("http://localhost:3000"));
        crate::desktop::set_emitter(None);
    }

    #[tokio::test]
    async fn check_fn_gates_on_env() {
        let _guard = BRIDGE_TEST_LOCK.lock().unwrap();
        // Env mutation is process-global; this test only asserts the
        // current (unset-in-test) default stays unavailable.
        std::env::remove_var("ULNCLAW_DESKTOP");
        assert!(matches!(
            check_desktop_requirements(),
            ToolAvailability::Unavailable(_)
        ));
    }
}

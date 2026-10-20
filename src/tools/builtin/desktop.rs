//! Desktop GUI tools — aligned with hermes' `close_terminal_tool.py`,
//! `read_terminal_tool.py`, `focus_pane_tool.py`, `desktop_preview` (preview_tool.py).
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

/// Accent swatch ids the renderer knows (hermes `ACCENT_SWATCHES`).
const ACCENT_SWATCHES: &[&str] = &[
    "mono", "green", "cyan", "nous", "violet", "pink", "red", "orange",
];
/// The theme's own accent; the renderer stores it as "no accent pick".
const THEME_ACCENT: &str = "theme";
/// hermes `MODES`.
const MODES: &[&str] = &["light", "dark", "system"];
/// hermes `PICKERS`.
const PICKERS: &[&str] = &["accent", "layout"];
/// The bundled presets (`apps/desktop/src/app/contrib/layout-presets.ts`). Simple
/// mode shows only the first two; applying an Advanced preset switches the app to
/// Advanced mode.
const SIMPLE_LAYOUTS: &[&str] = &["sidebar-left", "sidebar-right"];
const ADVANCED_LAYOUTS: &[&str] = &["default", "basic", "focus", "terminal-deck", "quad"];

/// `^#[0-9a-fA-F]{6}$` (hermes `_HEX`).
fn is_hex_accent(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 7 || bytes[0] != b'#' {
        return false;
    }
    bytes[1..]
        .iter()
        .all(|b| b.is_ascii_digit() || (b"abcdef".contains(&b.to_ascii_lowercase())))
}

/// hermes `_accent_error`: `None` when the accent is acceptable.
fn accent_error(accent: &str) -> Option<serde_json::Value> {
    if ACCENT_SWATCHES.contains(&accent) || accent == THEME_ACCENT || is_hex_accent(accent) {
        return None;
    }
    let mut available: Vec<serde_json::Value> = ACCENT_SWATCHES
        .iter()
        .map(|s| serde_json::Value::String((*s).to_string()))
        .collect();
    available.push(serde_json::Value::String(THEME_ACCENT.to_string()));
    Some(json!({
        "success": false,
        "error": format!(
            "Unknown accent {accent:?}: use a swatch id, 'theme' or #rrggbb."
        ),
        "available": available,
    }))
}

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
    registry.register(desktop_preview_tool());
    registry.register(set_theme_tool());
    registry.register(react_to_message_tool());
}

/// Change the desktop app's appearance — accent colour, light/dark mode, layout —
/// or show a picker card and wait for the user's pick.
///
/// Port of hermes `tools/set_theme_tool.py` (97 lines). 🔴 The renderer applies the
/// values and **answers with what it applied**, so the model reports what the user
/// sees: an unknown layout preset, a session that is not on screen and an absent
/// desktop window all come back as errors. Swatch ids and preset ids are validated
/// again in the renderer, which also knows the user's own saved layouts — so the
/// checks here exist to fail fast on the cases upstream fails fast on, not to be the
/// authority.
///
/// 🔴 **Toolset deviation, recorded not hidden:** upstream registers this (and
/// `focus_pane`, `desktop_preview`, `read_terminal`, `close_terminal`) under
/// `toolset="desktop_ui"`, one of `CLIENT_SURFACE_TOOLSETS` (`toolsets.py:71,139`).
/// The port has **no `desktop_ui` toolset at all** (0 hits) and its five existing
/// desktop tools all declare `.toolset("terminal")`. This follows the port's
/// existing convention for consistency; introducing `desktop_ui` is a separate
/// toolset-surface port that must move all five together, not one tool alone.
fn set_theme_tool() -> crate::tools::Tool {
    tool("set_theme")
        .description(
            "Change how the ulnclaw desktop app looks: accent colour, light/dark mode, layout. \
             Pass the values to apply them now; the result says what was applied. Pass picker \
             instead to show the user an accent or layout picker card in the chat and wait for \
             their pick. To reveal ONE pane, use focus_pane instead.",
        )
        .parameters(json!({
            "type": "object",
            "properties": {
                "accent": {
                    "type": "string",
                    "description": format!(
                        "A swatch id ({}), 'theme' for the theme's own accent, or a hex colour '#rrggbb'.",
                        ACCENT_SWATCHES.join(", ")
                    ),
                },
                "mode": {"type": "string", "enum": MODES},
                "layout": {
                    "type": "string",
                    "description": format!(
                        "Layout preset id. Simple mode: {}. Advanced mode: {}. The user's saved layouts by id.",
                        SIMPLE_LAYOUTS.join(", "),
                        ADVANCED_LAYOUTS.join(", ")
                    ),
                },
                "picker": {
                    "type": "string",
                    "enum": PICKERS,
                    "description": "Show that picker card and wait for the user (up to 5 minutes). Pass it alone.",
                },
            },
        }))
        .handler(|args, ctx| async move {
            // hermes: `values = {k: v.strip() for k, v in (…) if isinstance(v, str) and v.strip()}`
            let mut values = serde_json::Map::new();
            for key in ["accent", "mode", "layout"] {
                if let Some(raw) = args.get(key).and_then(|v| v.as_str()) {
                    let trimmed = raw.trim();
                    if !trimmed.is_empty() {
                        values.insert(key.to_string(), json!(trimmed));
                    }
                }
            }
            let picker = args.get("picker").and_then(|v| v.as_str());

            let payload = match picker {
                Some(picker) => {
                    if !PICKERS.contains(&picker) {
                        return Ok(json!({
                            "error": format!("picker must be one of: {}.", PICKERS.join(", "))
                        }));
                    }
                    // 🔴 A picker shows a card for the user to choose; values
                    // would be silently discarded by that card, so upstream
                    // refuses the combination rather than picking a winner.
                    if !values.is_empty() {
                        return Ok(json!({
                            "error": "picker shows a card for the user to choose; pass picker alone, without values."
                        }));
                    }
                    json!({"picker": picker})
                }
                None => {
                    if values.is_empty() {
                        return Ok(json!({
                            "error": "Pass at least one of accent, mode or layout, or picker to let the user choose."
                        }));
                    }
                    // 🔴 mode is checked BEFORE accent, matching upstream's order.
                    if let Some(mode) = values.get("mode").and_then(|v| v.as_str()) {
                        if !MODES.contains(&mode) {
                            return Ok(json!({
                                "success": false,
                                "error": format!("Unknown mode {mode:?}."),
                                "available": MODES,
                            }));
                        }
                    }
                    if let Some(accent) = values.get("accent").and_then(|v| v.as_str()) {
                        if let Some(error) = accent_error(accent) {
                            return Ok(error);
                        }
                    }
                    serde_json::Value::Object(values)
                }
            };

            // The renderer applies it (or shows the card and waits) and answers
            // with what it did — `None` means no host wired the bridge.
            let Some(result) = crate::desktop::set_theme(&ctx.session_id, payload) else {
                return Ok(json!({
                    "error": "set_theme is only available in the ulnclaw desktop app."
                }));
            };
            match result {
                Err(exc) => Ok(json!({"error": format!("Failed to set theme: {exc}")})),
                Ok(raw) if raw.trim().is_empty() => Ok(json!({
                    "error": "The desktop app did not answer (window closed, or the picker timed out)."
                })),
                // The renderer answers with a JSON object; pass it through so the
                // model sees exactly what was applied, else wrap the raw text.
                Ok(raw) => match serde_json::from_str::<serde_json::Value>(&raw) {
                    Ok(v) if v.is_object() => Ok(v),
                    _ => Ok(json!({"text": raw})),
                },
            }
        })
        .toolset("terminal")
        .emoji("🎨")
        .check_fn(check_desktop_requirements)
        .build()
        .expect("set_theme builds")
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
             the preview pane, use desktop_preview instead.",
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

fn desktop_preview_tool() -> crate::tools::Tool {
    tool("desktop_preview")
        .description(
            "Open something in the preview pane beside the chat in the ulnclaw desktop app. \
             Use this when the user asks to see a page, dev server, or file in the preview \
             pane — e.g. \"open cnn.com in the preview pane\" or \"preview localhost:3000\". \
             Accepts a web URL (a bare domain like www.cnn.com is fine), a localhost \
             dev-server URL, or a file path (HTML renders live; other files show their \
             contents). The pane opens for the current window only. `action` defaults to \
             \"open\" (close/read are not wired in this build).",
        )
        .parameters(json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["open"],
                    "description": "What to do with the preview pane. Only 'open' is supported in this build; omit for 'open'."
                },
                "url": {
                    "type": "string",
                    "description": "open: what to preview — a web URL (https://… or a bare domain), a localhost URL (localhost:3000), or a file path."
                },
                "label": {
                    "type": "string",
                    "description": "open: optional tab label; defaults to the target's name."
                }
            },
            "required": ["url"]
        }))
        .handler(|args, ctx| async move {
            let action = args
                .get("action")
                .and_then(|v| v.as_str())
                .unwrap_or("open");
            if action != "open" {
                return Ok(json!({
                    "error": "desktop_preview: only action='open' is supported in this build."
                }));
            }
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
        .expect("desktop_preview builds")
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

    /// 🔴 P2b `set_theme`: validation order + the blocking bridge contract.
    /// The renderer is the authority on presets/swatches; these checks exist to
    /// fail fast exactly where upstream does.
    #[tokio::test]
    async fn set_theme_validates_and_roundtrips() {
        let _guard = BRIDGE_TEST_LOCK.lock().unwrap();
        let tool = set_theme_tool();
        crate::desktop::set_theme_callback(None);

        // No bridge wired (CLI context) → desktop-only error, for BOTH shapes.
        let out = call(&tool, json!({"accent": "nous"})).await;
        assert!(out["error"].as_str().unwrap().contains("desktop"), "{out}");
        let out = call(&tool, json!({"picker": "accent"})).await;
        assert!(out["error"].as_str().unwrap().contains("desktop"), "{out}");

        // Argument validation fires BEFORE the bridge, so it needs no callback.
        let out = call(&tool, json!({})).await;
        assert!(
            out["error"].as_str().unwrap().contains("at least one"),
            "{out}"
        );
        let out = call(&tool, json!({"picker": "nope"})).await;
        assert!(
            out["error"]
                .as_str()
                .unwrap()
                .contains("picker must be one of"),
            "{out}"
        );
        // 🔴 picker + values is refused rather than silently discarding values.
        let out = call(&tool, json!({"picker": "accent", "mode": "dark"})).await;
        assert!(
            out["error"].as_str().unwrap().contains("pass picker alone"),
            "{out}"
        );
        // 🔴 mode is checked before accent (upstream order): an invalid mode wins.
        let out = call(&tool, json!({"mode": "neon", "accent": "nope"})).await;
        assert!(
            out["error"].as_str().unwrap().contains("Unknown mode"),
            "{out}"
        );
        assert_eq!(out["available"], json!(MODES));
        // Invalid accent lists what IS accepted.
        let out = call(&tool, json!({"accent": "chartreuse"})).await;
        assert!(
            out["error"].as_str().unwrap().contains("Unknown accent"),
            "{out}"
        );
        assert_eq!(out["success"], json!(false));
        assert_eq!(
            out["available"].as_array().unwrap().len(),
            ACCENT_SWATCHES.len() + 1
        );
        // Accepted accents: every swatch, `theme`, and #rrggbb (not #rgb, not 8 digits).
        for accent in ACCENT_SWATCHES.iter().chain(std::iter::once(&THEME_ACCENT)) {
            assert!(accent_error(accent).is_none(), "{accent} must be accepted");
        }
        assert!(accent_error("#1a2b3c").is_none());
        assert!(accent_error("#ABCDEF").is_none());
        assert!(accent_error("#abc").is_some(), "3-digit hex is not #rrggbb");
        assert!(accent_error("#12345678").is_some());
        assert!(accent_error("1a2b3c").is_some(), "a bare hex needs the #");

        // Wired callback: values pass through, and the answer is passed back
        // verbatim when it is a JSON object (the model must see what was applied).
        let seen: Arc<Mutex<Vec<(String, serde_json::Value)>>> = Arc::new(Mutex::new(Vec::new()));
        let recorder = seen.clone();
        crate::desktop::set_theme_callback(Some(Box::new(move |sid, payload| {
            recorder.lock().unwrap().push((sid.to_string(), payload));
            Ok(r#"{"success": true, "applied": {"accent": "nous"}}"#.to_string())
        })));
        let out = call(&tool, json!({"accent": "  nous  ", "mode": "dark"})).await;
        assert_eq!(out["applied"]["accent"], json!("nous"));
        {
            let calls = seen.lock().unwrap();
            assert_eq!(calls.len(), 1);
            assert_eq!(
                calls[0].0, "ui-test",
                "the turn's ui session id must ride along"
            );
            // 🔴 Whitespace is stripped (upstream `.strip()`), and only non-empty
            // values are forwarded — no `layout` key was passed, none appears.
            assert_eq!(calls[0].1, json!({"accent": "nous", "mode": "dark"}));
        }
        // A picker reaches the renderer as {"picker": …} alone.
        let out = call(&tool, json!({"picker": "layout"})).await;
        assert_eq!(out["success"], json!(true));
        assert_eq!(seen.lock().unwrap()[1].1, json!({"picker": "layout"}));

        // Renderer errors surface as errors; a non-JSON answer is wrapped.
        crate::desktop::set_theme_callback(Some(Box::new(|_s, _p| Err("no window".into()))));
        let out = call(&tool, json!({"mode": "light"})).await;
        assert!(
            out["error"].as_str().unwrap().contains("no window"),
            "{out}"
        );
        crate::desktop::set_theme_callback(Some(Box::new(|_s, _p| Ok("   ".into()))));
        let out = call(&tool, json!({"mode": "light"})).await;
        assert!(
            out["error"].as_str().unwrap().contains("did not answer"),
            "{out}"
        );
        crate::desktop::set_theme_callback(Some(Box::new(|_s, _p| Ok("applied".into()))));
        let out = call(&tool, json!({"mode": "light"})).await;
        assert_eq!(out["text"], json!("applied"));
        crate::desktop::set_theme_callback(None);
    }

    /// The schema advertises the enums the renderer validates against, and the
    /// tool is desktop-gated like its four siblings.
    #[test]
    fn set_theme_schema_and_gating() {
        let tool = set_theme_tool();
        assert_eq!(tool.definition.name, "set_theme");
        let props = &tool.definition.parameters["properties"];
        assert_eq!(props["mode"]["enum"], json!(MODES));
        assert_eq!(props["picker"]["enum"], json!(PICKERS));
        assert!(props["accent"]["description"]
            .as_str()
            .unwrap()
            .contains("mono, green"));
        assert!(props["layout"]["description"]
            .as_str()
            .unwrap()
            .contains("terminal-deck"));
        // 🔴 Same gate as the other desktop tools: never registered outside a host.
        assert_eq!(
            (check_desktop_requirements)().is_available(),
            crate::desktop::desktop_env_enabled()
        );
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
    async fn desktop_preview_normalizes_and_emits() {
        let _guard = BRIDGE_TEST_LOCK.lock().unwrap();
        let tool = desktop_preview_tool();
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

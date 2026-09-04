//! Session recap — port of hermes' `session_recap.py` (inspired by Claude
//! Code's `/recap`): an instant, local-only summary of what happened in a
//! conversation so users juggling multiple sessions can re-orient quickly.
//! No LLM call — pure computation over the message history.

use crate::provider::{Message, Role};
use std::collections::HashMap;

/// How many recent user/assistant turns we consider "recent activity".
const RECENT_TURN_WINDOW: usize = 20;
/// How many characters of the latest user prompt to show.
const PROMPT_PREVIEW_CHARS: usize = 140;
/// How many characters of the latest assistant text to show.
const ASSISTANT_PREVIEW_CHARS: usize = 200;
/// How many recently-touched files to list.
const MAX_FILES_LISTED: usize = 5;

/// File-editing tools and the argument key that holds the path.
fn file_edit_tools() -> &'static HashMap<&'static str, &'static str> {
    static MAP: std::sync::OnceLock<HashMap<&'static str, &'static str>> =
        std::sync::OnceLock::new();
    MAP.get_or_init(|| {
        let mut map = HashMap::new();
        map.insert("write_file", "path");
        map.insert("patch", "path");
        map.insert("read_file", "path");
        map.insert("skill_manage", "file_path");
        map.insert("skill_view", "file_path");
        map
    })
}

fn message_text(message: &Message) -> String {
    message.content.clone().unwrap_or_default()
}

fn tool_call_name_and_args(message: &Message) -> Vec<(String, serde_json::Value)> {
    let Some(tool_calls) = message.tool_calls.as_ref() else {
        return Vec::new();
    };
    tool_calls
        .iter()
        .map(|call| {
            let args: serde_json::Value =
                serde_json::from_str(&call.function.arguments).unwrap_or(serde_json::Value::Null);
            (call.function.name.clone(), args)
        })
        .filter(|(name, _)| !name.is_empty())
        .collect()
}

/// `(user_turns, assistant_turns, tool_messages)`.
fn count_visible_turns(messages: &[Message]) -> (usize, usize, usize) {
    let (mut users, mut assistants, mut tools) = (0, 0, 0);
    for message in messages {
        match message.role {
            Role::User => users += 1,
            Role::Assistant => assistants += 1,
            Role::Tool => tools += 1,
            Role::System => {}
        }
    }
    (users, assistants, tools)
}

fn latest_user_prompt(messages: &[Message]) -> Option<String> {
    messages.iter().rev().find_map(|message| {
        if message.role == Role::User {
            let text = message_text(message).trim().to_string();
            if !text.is_empty() {
                return Some(text);
            }
        }
        None
    })
}

fn latest_assistant_text(messages: &[Message]) -> Option<String> {
    messages.iter().rev().find_map(|message| {
        if message.role == Role::Assistant {
            let text = message_text(message).trim().to_string();
            if !text.is_empty() {
                return Some(text);
            }
        }
        None
    })
}

/// Tail slice covering at most `window` user+assistant turns (tool
/// messages ride along inside the window).
fn recent_window(messages: &[Message], window: usize) -> &[Message] {
    let mut count = 0usize;
    for (i, message) in messages.iter().enumerate().rev() {
        if matches!(message.role, Role::User | Role::Assistant) {
            count += 1;
            if count >= window {
                return &messages[i..];
            }
        }
    }
    messages
}

/// Show a path relative to cwd when possible, otherwise `~`-expanded.
fn shortened_path(path: &str) -> String {
    if path.is_empty() {
        return path.to_string();
    }
    let expanded = if let Some(rest) = path.strip_prefix("~/") {
        dirs_home().map(|home| home.join(rest).display().to_string())
    } else {
        None
    }
    .unwrap_or_else(|| path.to_string());
    let abs = std::path::Path::new(&expanded);
    let abs = if abs.is_absolute() {
        expanded.clone()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(abs).display().to_string())
            .unwrap_or(expanded.clone())
    };
    if let Ok(cwd) = std::env::current_dir() {
        let cwd = cwd.display().to_string();
        if abs == cwd {
            return ".".to_string();
        }
        if let Some(rest) = abs.strip_prefix(&format!("{}/", cwd)) {
            return rest.to_string();
        }
    }
    if let Some(home) = dirs_home() {
        let home = home.display().to_string();
        if let Some(rest) = abs.strip_prefix(&format!("{}/", home)) {
            return format!("~/{}", rest);
        }
    }
    abs
}

fn dirs_home() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
}

/// Strip ANSI escape sequences and control characters so untrusted history
/// can't retitle/clear a terminal when echoed (hermes `ansi_strip`).
pub fn sanitize_display_text(text: &str) -> String {
    let re = regex::Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]").unwrap();
    let stripped = re.replace_all(text, "");
    stripped
        .chars()
        .filter(|c| *c == '\n' || *c == '\t' || !c.is_control())
        .collect()
}

fn truncate(text: &str, limit: usize) -> String {
    let text = sanitize_display_text(text);
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let count = text.chars().count();
    if count <= limit {
        return text;
    }
    let cut: String = text.chars().take(limit.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}

/// `(tool_counts sorted desc, recently edited files newest-first)`.
fn summarise_tool_activity(
    tool_calls: &[(String, serde_json::Value)],
) -> (Vec<(String, usize)>, Vec<String>) {
    let edit_tools = file_edit_tools();
    let mut counter: HashMap<String, usize> = HashMap::new();
    let mut files_seen: Vec<String> = Vec::new();
    let mut files_set: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (name, args) in tool_calls.iter().rev() {
        *counter.entry(name.clone()).or_insert(0) += 1;
        if let Some(arg_key) = edit_tools.get(name.as_str()) {
            if let Some(path) = args.get(arg_key).and_then(|v| v.as_str()) {
                if !path.is_empty() && files_set.insert(path.to_string()) {
                    files_seen.push(shortened_path(path));
                }
            }
        }
    }
    let mut tool_counts: Vec<(String, usize)> = counter.into_iter().collect();
    tool_counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    (tool_counts, files_seen)
}

/// Build a multi-line recap of recent activity (hermes `build_recap`).
pub fn build_recap(
    messages: &[Message],
    session_title: Option<&str>,
    session_id: Option<&str>,
) -> String {
    let mut lines: Vec<String> = Vec::new();

    let mut header = String::from("Session recap");
    if let Some(title) = session_title.filter(|t| !t.is_empty()) {
        header.push_str(&format!(" — {}", title));
    } else if let Some(id) = session_id {
        header.push_str(&format!(" — {}", &id[..id.len().min(8)]));
    }
    lines.push(header);

    if messages.is_empty() {
        lines.push("  (nothing to recap — no messages yet)".to_string());
        return lines.join("\n");
    }

    let (users, assistants, tool_msgs) = count_visible_turns(messages);
    let window = recent_window(messages, RECENT_TURN_WINDOW);
    let (win_users, win_assistants, _) = count_visible_turns(window);

    let mut scope = format!(
        "{} user turn{} / {} assistant repl{}",
        win_users,
        if win_users != 1 { "s" } else { "" },
        win_assistants,
        if win_assistants != 1 { "ies" } else { "y" }
    );
    if (users, assistants) != (win_users, win_assistants) {
        scope.push_str(&format!(" (of {}/{} total)", users, assistants));
    }
    lines.push(format!(
        "  Recent: {}, {} tool result{}",
        scope,
        tool_msgs,
        if tool_msgs != 1 { "s" } else { "" }
    ));

    let tool_calls: Vec<(String, serde_json::Value)> = window
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .flat_map(tool_call_name_and_args)
        .collect();
    let (tool_counts, files) = summarise_tool_activity(&tool_calls);
    if !tool_counts.is_empty() {
        let top: Vec<String> = tool_counts
            .iter()
            .take(5)
            .map(|(name, count)| format!("{}×{}", name, count))
            .collect();
        let mut line = format!("  Tools used: {}", top.join(", "));
        if tool_counts.len() > 5 {
            line.push_str(&format!(" (+{} more)", tool_counts.len() - 5));
        }
        lines.push(line);
    }
    if !files.is_empty() {
        let shown: Vec<&String> = files.iter().take(MAX_FILES_LISTED).collect();
        let mut entry = shown
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        if files.len() > shown.len() {
            entry.push_str(&format!(" (+{} more)", files.len() - shown.len()));
        }
        lines.push(format!("  Files touched: {}", entry));
    }

    if let Some(prompt) = latest_user_prompt(window) {
        lines.push(format!(
            "  Last ask: {}",
            truncate(&prompt, PROMPT_PREVIEW_CHARS)
        ));
    }
    if let Some(reply) = latest_assistant_text(window) {
        lines.push(format!(
            "  Last reply: {}",
            truncate(&reply, ASSISTANT_PREVIEW_CHARS)
        ));
    }

    if lines.len() == 2 {
        lines.push("  (no assistant activity yet in this window)".to_string());
    }

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{FunctionCall, ToolCall};

    fn msg(role: Role, content: &str) -> Message {
        Message {
            role,
            content: Some(content.to_string()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }

    fn assistant_with_tool(name: &str, args: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: None,
            tool_calls: Some(vec![ToolCall {
                id: format!("call_{}", name),
                call_type: "function".into(),
                function: FunctionCall {
                    name: name.into(),
                    arguments: args.into(),
                },
            }]),
            tool_call_id: None,
            name: None,
        }
    }

    #[test]
    fn test_recap_empty() {
        let recap = build_recap(&[], None, Some("abcdef12-345"));
        assert!(recap.starts_with("Session recap — abcdef12"));
        assert!(recap.contains("(nothing to recap — no messages yet)"));
    }

    #[test]
    fn test_recap_full() {
        let messages = vec![
            msg(Role::User, "fix the login bug"),
            assistant_with_tool("read_file", "{\"path\":\"/repo/src/auth.rs\"}"),
            msg(Role::Tool, "fn login() {}"),
            assistant_with_tool("patch", "{\"path\":\"/repo/src/auth.rs\"}"),
            msg(Role::Tool, "patched"),
            assistant_with_tool("terminal", "{\"command\":\"cargo test\"}"),
            msg(Role::Tool, "all tests passed"),
            msg(Role::Assistant, "Fixed the login bug; all tests pass."),
        ];
        let recap = build_recap(&messages, Some("Auth work"), None);
        assert!(recap.starts_with("Session recap — Auth work"));
        assert!(recap.contains("Recent: 1 user turn / 4 assistant replies, 3 tool results"));
        assert!(recap.contains("Tools used:"));
        assert!(recap.contains("read_file×1"));
        assert!(recap.contains("Files touched:"));
        assert!(recap.contains("src/auth.rs"));
        assert!(recap.contains("Last ask: fix the login bug"));
        assert!(recap.contains("Last reply: Fixed the login bug; all tests pass."));
    }

    #[test]
    fn test_recap_window_scopes_long_histories() {
        let mut messages = Vec::new();
        for i in 0..30 {
            messages.push(msg(Role::User, &format!("question {}", i)));
            messages.push(msg(Role::Assistant, &format!("answer {}", i)));
        }
        let recap = build_recap(&messages, None, None);
        // Window covers the last 20 turns (10 user + 10 assistant).
        assert!(recap.contains("10 user turns / 10 assistant replies (of 30/30 total)"));
        assert!(recap.contains("Last ask: question 29"));
    }

    #[test]
    fn test_truncate_and_sanitize() {
        assert_eq!(truncate("hello world", 50), "hello world");
        let long = "x".repeat(200);
        let cut = truncate(&long, 20);
        assert_eq!(cut.chars().count(), 20);
        assert!(cut.ends_with('…'));
        // Newlines collapse; ANSI escapes and control chars are stripped.
        let dirty = "line1\nline2\x1b[2J\x07end";
        assert_eq!(truncate(dirty, 100), "line1 line2end");
    }

    #[test]
    fn test_files_most_recent_first_and_deduped() {
        let calls = vec![
            ("patch".to_string(), serde_json::json!({"path": "a.rs"})),
            ("patch".to_string(), serde_json::json!({"path": "b.rs"})),
            ("patch".to_string(), serde_json::json!({"path": "a.rs"})),
        ];
        let (counts, files) = summarise_tool_activity(&calls);
        assert_eq!(counts[0], ("patch".to_string(), 3));
        assert_eq!(files, vec!["a.rs", "b.rs"]);
    }
}

//! Session export — render a stored session transcript as Markdown or a
//! standalone HTML file (gateway `GET /api/sessions/:id/export`, surfaced
//! by the desktop session actions). Hermes offers transcript export from
//! its web dashboard; this is the portable-file equivalent.

use chrono::{DateTime, Utc};

use crate::provider::{Message, Role};
use crate::session::sqlite::SessionRow;

fn fmt_ts(epoch_seconds: f64) -> String {
    DateTime::<Utc>::from_timestamp(epoch_seconds as i64, 0)
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| format!("{epoch_seconds}"))
}

fn role_heading(role: &Role) -> &'static str {
    match role {
        Role::System => "## ⚙️ System",
        Role::User => "## 🧑 User",
        Role::Assistant => "## 🤖 Assistant",
        Role::Tool => "## 🔧 Tool result",
    }
}

fn role_class(role: &Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

/// Filename stem from the session title (slugified) or the short id.
pub fn export_stem(row: &SessionRow) -> String {
    let slug: String = row
        .title
        .as_deref()
        .unwrap_or("")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else if c.is_whitespace() || c == '-' || c == '_' {
                '-'
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches(|c| c == '-' || c == '_')
        .chars()
        .take(48)
        .collect::<String>()
        .trim_matches(|c| c == '-' || c == '_')
        .to_string();
    if slug.is_empty() {
        row.id.chars().take(8).collect()
    } else {
        slug
    }
}

fn metadata_pairs(row: &SessionRow) -> Vec<(String, String)> {
    let mut pairs = vec![
        ("Session".to_string(), row.id.clone()),
        ("Source".to_string(), row.source.clone()),
    ];
    if let Some(model) = &row.model {
        pairs.push(("Model".to_string(), model.clone()));
    }
    if let Some(cwd) = &row.cwd {
        pairs.push(("Working dir".to_string(), cwd.clone()));
    }
    pairs.push(("Started".to_string(), fmt_ts(row.started_at)));
    if let Some(ended) = row.ended_at {
        pairs.push(("Ended".to_string(), fmt_ts(ended)));
    }
    if let Some(reason) = &row.end_reason {
        pairs.push(("End reason".to_string(), reason.clone()));
    }
    pairs.push(("Messages".to_string(), row.message_count.to_string()));
    pairs.push((
        "Tokens".to_string(),
        format!("{} in / {} out", row.input_tokens, row.output_tokens),
    ));
    pairs
}

/// Render the transcript as Markdown (content fenced with 4 backticks so
/// embedded triple-backtick blocks cannot break out).
pub fn render_markdown(row: &SessionRow, messages: &[(f64, Message)]) -> String {
    let title = row
        .title
        .clone()
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| row.id.chars().take(8).collect());
    let mut out = String::new();
    out.push_str(&format!("# Session {title}\n\n"));
    out.push_str("| | |\n|---|---|\n");
    for (key, value) in metadata_pairs(row) {
        out.push_str(&format!("| **{key}** | {value} |\n"));
    }
    out.push('\n');
    for (ts, message) in messages {
        out.push_str(role_heading(&message.role));
        out.push_str(&format!(" — {}\n\n", fmt_ts(*ts)));
        if message.role == Role::Tool {
            if let Some(name) = &message.name {
                out.push_str(&format!("**Tool:** `{name}`\n\n"));
            }
            if let Some(call_id) = &message.tool_call_id {
                out.push_str(&format!("**Call id:** `{call_id}`\n\n"));
            }
        }
        if let Some(calls) = &message.tool_calls {
            for call in calls {
                out.push_str(&format!(
                    "**→ Tool call:** `{}` (id `{}`)\n\n",
                    call.function.name, call.id
                ));
                out.push_str("````json\n");
                out.push_str(&pretty_json_or_raw(&call.function.arguments));
                out.push_str("\n````\n\n");
            }
        }
        if let Some(content) = &message.content {
            if !content.is_empty() {
                out.push_str("````\n");
                out.push_str(content);
                out.push_str("\n````\n\n");
            }
        }
        out.push_str("---\n\n");
    }
    out
}

fn pretty_json_or_raw(raw: &str) -> String {
    serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or_else(|| raw.to_string())
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Render the transcript as a standalone HTML document.
pub fn render_html(row: &SessionRow, messages: &[(f64, Message)]) -> String {
    let title = html_escape(
        &row.title
            .clone()
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| row.id.chars().take(8).collect()),
    );
    let mut out = String::new();
    out.push_str("<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n");
    out.push_str(&format!(
        "<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<title>Session {title}</title>\n"
    ));
    out.push_str(
        "<style>\n\
         body { font-family: -apple-system, 'Segoe UI', Roboto, sans-serif; margin: 2rem auto; \
         max-width: 860px; padding: 0 1rem; background: #101014; color: #e6e6e6; }\n\
         h1 { font-size: 1.4rem; }\n\
         table.meta { border-collapse: collapse; margin-bottom: 1.5rem; font-size: 0.9rem; }\n\
         table.meta td { border: 1px solid #333; padding: 4px 10px; }\n\
         table.meta td:first-child { color: #9aa0a6; }\n\
         .msg { border: 1px solid #2a2a33; border-radius: 8px; padding: 10px 14px; margin: 12px 0; }\n\
         .msg h2 { font-size: 0.95rem; margin: 0 0 6px; }\n\
         .msg .when { color: #9aa0a6; font-weight: normal; font-size: 0.8rem; }\n\
         .user h2 { color: #7aa2f7; } .assistant h2 { color: #2ecc40; }\n\
         .system h2 { color: #9aa0a6; } .tool h2 { color: #ffc107; }\n\
         pre { background: #16161c; border: 1px solid #2a2a33; border-radius: 6px; \
         padding: 10px; overflow-x: auto; white-space: pre-wrap; font-size: 0.85rem; }\n\
         .call { color: #c792ea; font-size: 0.85rem; margin: 6px 0 2px; }\n\
         </style>\n</head>\n<body>\n",
    );
    out.push_str(&format!("<h1>Session {title}</h1>\n"));
    out.push_str("<table class=\"meta\">\n");
    for (key, value) in metadata_pairs(row) {
        out.push_str(&format!(
            "<tr><td>{}</td><td>{}</td></tr>\n",
            html_escape(&key),
            html_escape(&value)
        ));
    }
    out.push_str("</table>\n");
    for (ts, message) in messages {
        let class = role_class(&message.role);
        let heading = match message.role {
            Role::System => "System",
            Role::User => "User",
            Role::Assistant => "Assistant",
            Role::Tool => "Tool result",
        };
        out.push_str(&format!(
            "<div class=\"msg {class}\"><h2>{heading} <span class=\"when\">{}</span></h2>\n",
            html_escape(&fmt_ts(*ts))
        ));
        if message.role == Role::Tool {
            if let Some(name) = &message.name {
                out.push_str(&format!(
                    "<div class=\"call\">tool: <code>{}</code></div>\n",
                    html_escape(name)
                ));
            }
        }
        if let Some(calls) = &message.tool_calls {
            for call in calls {
                out.push_str(&format!(
                    "<div class=\"call\">→ tool call: <code>{}</code></div>\n",
                    html_escape(&call.function.name)
                ));
                out.push_str(&format!(
                    "<pre>{}</pre>\n",
                    html_escape(&pretty_json_or_raw(&call.function.arguments))
                ));
            }
        }
        if let Some(content) = &message.content {
            if !content.is_empty() {
                out.push_str(&format!("<pre>{}</pre>\n", html_escape(content)));
            }
        }
        out.push_str("</div>\n");
    }
    out.push_str("</body>\n</html>\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{FunctionCall, ToolCall};

    fn row() -> SessionRow {
        SessionRow {
            id: "sess-12345678".into(),
            source: "cli".into(),
            model: Some("test-model".into()),
            title: Some("My Demo Session!".into()),
            cwd: Some("/tmp/work".into()),
            parent_session_id: None,
            started_at: 1_750_000_000.0,
            last_activity_at: 1_750_000_300.0,
            ended_at: Some(1_750_000_300.0),
            end_reason: Some("completed".into()),
            message_count: 3,
            input_tokens: 120,
            output_tokens: 45,
            archived: false,
            pinned: false,
            tool_call_count: 0,
        }
    }

    fn messages() -> Vec<(f64, Message)> {
        vec![
            (
                1_750_000_010.0,
                Message {
                    role: Role::User,
                    content: Some("What is <b>2+2</b>?".into()),
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                },
            ),
            (
                1_750_000_020.0,
                Message {
                    role: Role::Assistant,
                    content: Some("Let me compute.".into()),
                    tool_calls: Some(vec![ToolCall {
                        id: "call-1".into(),
                        call_type: "function".into(),
                        function: FunctionCall {
                            name: "calculator".into(),
                            arguments: "{\"expr\":\"2+2\"}".into(),
                        },
                    }]),
                    tool_call_id: None,
                    name: None,
                },
            ),
            (
                1_750_000_030.0,
                Message {
                    role: Role::Tool,
                    content: Some("4".into()),
                    tool_calls: None,
                    tool_call_id: Some("call-1".into()),
                    name: Some("calculator".into()),
                },
            ),
        ]
    }

    #[test]
    fn export_stem_slugifies_title_or_falls_back_to_id() {
        let mut r = row();
        assert_eq!(export_stem(&r), "my-demo-session");
        r.title = None;
        assert_eq!(export_stem(&r), "sess-123");
    }

    #[test]
    fn render_markdown_includes_metadata_and_fenced_content() {
        let md = render_markdown(&row(), &messages());
        assert!(md.starts_with("# Session My Demo Session!"));
        assert!(md.contains("| **Model** | test-model |"));
        assert!(md.contains("| **Tokens** | 120 in / 45 out |"));
        assert!(md.contains("## 🧑 User — 2025-06-15"));
        assert!(md.contains("````\nWhat is <b>2+2</b>?\n````"));
        assert!(md.contains("**→ Tool call:** `calculator` (id `call-1`)"));
        assert!(md.contains("\"expr\": \"2+2\""));
        assert!(md.contains("**Tool:** `calculator`"));
    }

    #[test]
    fn render_html_escapes_and_structures() {
        let html = render_html(&row(), &messages());
        assert!(html.contains("<title>Session My Demo Session!</title>"));
        assert!(html.contains("What is &lt;b&gt;2+2&lt;/b&gt;?"));
        assert!(html.contains("class=\"msg user\""));
        assert!(html.contains("class=\"msg assistant\""));
        assert!(html.contains("class=\"msg tool\""));
        assert!(html.contains("→ tool call: <code>calculator</code>"));
    }
}

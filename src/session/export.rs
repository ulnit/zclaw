//! Session export — port of hermes' `session_export_md.py`: renders a
//! session as a verifiable Markdown document (frontmatter + message
//! headings + tool-call blocks + SHA256 export verification) and keeps a
//! `manifest.jsonl` alongside the exports.
//!
//! Also renders standalone HTML (`session_export_html.py` counterpart):
//! the same session data with inline styling, message cards, and
//! HTML-escaped content.

use crate::error::{AgentError, Result};
use crate::provider::Message;
use serde_json::json;
use std::path::{Path, PathBuf};

pub const EXPORTER_VERSION: &str = "ulnclaw sessions export (md) v1";

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Everything the exporter needs about one session.
pub struct ExportSession {
    pub id: String,
    pub title: Option<String>,
    pub source: String,
    pub model: Option<String>,
    pub cwd: Option<String>,
    pub started_at: f64,
    pub ended_at: Option<f64>,
    pub messages: Vec<(f64, Message)>,
}

fn iso_timestamp(value: f64) -> String {
    chrono::DateTime::from_timestamp(value as i64, ((value.fract()) * 1e9) as u32)
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_default()
}

fn frontmatter_value(value: &serde_json::Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
}

fn frontmatter_line(key: &str, value: serde_json::Value) -> String {
    format!("{}: {}", key, frontmatter_value(&value))
}

fn message_heading(timestamp: f64, message: &Message) -> String {
    let role = match message.role {
        crate::provider::Role::System => "System",
        crate::provider::Role::User => "User",
        crate::provider::Role::Assistant => "Assistant",
        crate::provider::Role::Tool => "Tool",
    };
    let label = if message.role == crate::provider::Role::Tool {
        match message.name.as_deref().filter(|n| !n.is_empty()) {
            Some(name) => format!("Tool — {}", name),
            None => "Tool".to_string(),
        }
    } else {
        role.to_string()
    };
    let iso = iso_timestamp(timestamp);
    if iso.is_empty() {
        format!("### {}", label)
    } else {
        format!("### {} — {}", label, iso)
    }
}

fn render_content(message: &Message) -> String {
    message
        .content
        .as_deref()
        .map(|content| content.trim_end().to_string())
        .unwrap_or_default()
}

fn render_tool_calls(message: &Message) -> String {
    let Some(tool_calls) = message
        .tool_calls
        .as_ref()
        .filter(|calls| !calls.is_empty())
    else {
        return String::new();
    };
    let pretty = serde_json::to_string_pretty(tool_calls).unwrap_or_else(|_| "[]".to_string());
    format!("\n\n## Tool calls\n\n```json\n{}\n```", pretty)
}

/// Render the session as Markdown with the SHA256 verification footer
/// (hermes `render_session_markdown`). The digest covers the body with the
/// SHA line set to `pending`, so `verify_export_content` can re-check it.
pub fn render_session_markdown(session: &ExportSession, include_verification: bool) -> String {
    let exported_at = now_secs();
    render_session_markdown_at(session, exported_at, include_verification)
}

/// Render at an explicit export timestamp (tests use a fixed value).
pub fn render_session_markdown_at(
    session: &ExportSession,
    exported_at: f64,
    include_verification: bool,
) -> String {
    let title = session
        .title
        .clone()
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| session.id.clone());
    let exported_iso = iso_timestamp(exported_at);
    let message_count = session.messages.len();

    let frontmatter = vec![
        "---".to_string(),
        frontmatter_line("session_id", json!(session.id)),
        frontmatter_line("title", json!(session.title)),
        frontmatter_line("source", json!(session.source)),
        frontmatter_line("created_at", json!(iso_timestamp(session.started_at))),
        frontmatter_line("ended_at", json!(session.ended_at.map(iso_timestamp))),
        frontmatter_line("model", json!(session.model)),
        frontmatter_line("cwd", json!(session.cwd)),
        frontmatter_line("message_count", json!(message_count)),
        frontmatter_line("format", json!("md")),
        frontmatter_line("exported_at", json!(exported_iso)),
        frontmatter_line("exporter", json!(EXPORTER_VERSION)),
        "---".to_string(),
        String::new(),
    ];

    let mut parts: Vec<String> = Vec::new();
    parts.push(frontmatter.join("\n"));
    parts.push(format!("# {}\n", title));
    parts.push(format!("Session ID: `{}`\n", session.id));
    if !session.source.is_empty() {
        parts.push(format!("Source: `{}`\n", session.source));
    }
    if let Some(ref cwd) = session.cwd {
        if !cwd.is_empty() {
            parts.push(format!("Working directory: `{}`\n", cwd));
        }
    }

    let mut messages_section = vec!["## Messages\n".to_string()];
    if session.messages.is_empty() {
        messages_section.push("_No messages in this session._\n".to_string());
    } else {
        for (timestamp, message) in &session.messages {
            messages_section.push(format!("{}\n", message_heading(*timestamp, message)));
            let content = render_content(message);
            if !content.is_empty() {
                messages_section.push(format!("{}\n", content));
            }
            let tool_calls = render_tool_calls(message);
            if !tool_calls.is_empty() {
                messages_section.push(format!("{}\n", tool_calls));
            }
            messages_section.push(String::new());
        }
    }
    parts.push(messages_section.join("\n").trim_end().to_string() + "\n");

    let body_without_sha = parts.join("\n").trim_end().to_string() + "\n";
    if !include_verification {
        return body_without_sha;
    }

    let pending_body = format!(
        "{}\n## Export verification\n\n- Session id: `{}`\n- Exported messages: `{}`\n- Exported at: `{}`\n- SHA256 of exported body: `pending`\n",
        body_without_sha.trim_end(),
        session.id,
        message_count,
        exported_iso
    );
    let digest = sha256_hex(pending_body.as_bytes());
    pending_body.replace(
        "SHA256 of exported body: `pending`",
        &format!("SHA256 of exported body: `{}`", digest),
    )
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn html_role_class(message: &Message) -> &'static str {
    match message.role {
        crate::provider::Role::System => "system",
        crate::provider::Role::User => "user",
        crate::provider::Role::Assistant => "assistant",
        crate::provider::Role::Tool => "tool",
    }
}

fn html_role_label(message: &Message) -> String {
    if message.role == crate::provider::Role::Tool {
        match message.name.as_deref().filter(|n| !n.is_empty()) {
            Some(name) => format!("Tool — {}", name),
            None => "Tool".to_string(),
        }
    } else {
        match message.role {
            crate::provider::Role::System => "System".to_string(),
            crate::provider::Role::User => "User".to_string(),
            crate::provider::Role::Assistant => "Assistant".to_string(),
            crate::provider::Role::Tool => "Tool".to_string(),
        }
    }
}

/// Render the session as a standalone HTML document (hermes
/// `session_export_html.py` counterpart): inline CSS, one card per
/// message, tool calls in code blocks, verification footer.
pub fn render_session_html(session: &ExportSession, include_verification: bool) -> String {
    let exported_at = now_secs();
    render_session_html_at(session, exported_at, include_verification)
}

/// Render HTML at an explicit export timestamp (tests use a fixed value).
pub fn render_session_html_at(
    session: &ExportSession,
    exported_at: f64,
    include_verification: bool,
) -> String {
    let title = session
        .title
        .clone()
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| session.id.clone());
    let exported_iso = iso_timestamp(exported_at);
    let message_count = session.messages.len();

    let mut html = String::new();
    html.push_str("<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n");
    html.push_str(&format!(
        "<title>{}</title>\n",
        html_escape(&format!("Session {} — {}", session.id, title))
    ));
    html.push_str(
        "<style>\n\
         body { font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif; margin: 2rem auto; max-width: 860px; padding: 0 1rem; color: #1f2328; background: #ffffff; }\n\
         h1 { font-size: 1.4rem; border-bottom: 1px solid #d0d7de; padding-bottom: .5rem; }\n\
         .meta { color: #57606a; font-size: .9rem; margin-bottom: 1.5rem; }\n\
         .meta dt { font-weight: 600; display: inline; }\n\
         .meta dd { display: inline; margin: 0 1rem 0 .25rem; }\n\
         .meta div { margin: .15rem 0; }\n\
         .message { border: 1px solid #d0d7de; border-radius: 8px; margin: .9rem 0; overflow: hidden; }\n\
         .message .head { padding: .4rem .8rem; font-size: .85rem; font-weight: 600; border-bottom: 1px solid #d0d7de; background: #f6f8fa; }\n\
         .message.user .head { background: #ddf4ff; }\n\
         .message.assistant .head { background: #dafbe1; }\n\
         .message.tool .head { background: #fff8c5; }\n\
         .message.system .head { background: #f6f8fa; color: #57606a; }\n\
         .message .body { padding: .6rem .8rem; white-space: pre-wrap; word-wrap: break-word; }\n\
         .message pre { background: #f6f8fa; border-radius: 6px; padding: .6rem; overflow-x: auto; margin: .4rem 0 0; }\n\
         .empty { color: #57606a; font-style: italic; }\n\
         footer { margin-top: 2rem; color: #57606a; font-size: .85rem; border-top: 1px solid #d0d7de; padding-top: .75rem; }\n\
         </style>\n",
    );
    html.push_str("</head>\n<body>\n");
    html.push_str(&format!("<h1>{}</h1>\n", html_escape(&title)));
    html.push_str("<dl class=\"meta\">\n");
    html.push_str(&format!(
        "<div><dt>Session ID:</dt><dd><code>{}</code></dd></div>\n",
        html_escape(&session.id)
    ));
    if !session.source.is_empty() {
        html.push_str(&format!(
            "<div><dt>Source:</dt><dd><code>{}</code></dd></div>\n",
            html_escape(&session.source)
        ));
    }
    if let Some(ref model) = session.model {
        html.push_str(&format!(
            "<div><dt>Model:</dt><dd><code>{}</code></dd></div>\n",
            html_escape(model)
        ));
    }
    if let Some(ref cwd) = session.cwd {
        if !cwd.is_empty() {
            html.push_str(&format!(
                "<div><dt>Working directory:</dt><dd><code>{}</code></dd></div>\n",
                html_escape(cwd)
            ));
        }
    }
    html.push_str(&format!(
        "<div><dt>Started:</dt><dd>{}</dd></div>\n",
        html_escape(&iso_timestamp(session.started_at))
    ));
    if let Some(ended) = session.ended_at {
        html.push_str(&format!(
            "<div><dt>Ended:</dt><dd>{}</dd></div>\n",
            html_escape(&iso_timestamp(ended))
        ));
    }
    html.push_str(&format!(
        "<div><dt>Messages:</dt><dd>{}</dd></div>\n",
        message_count
    ));
    html.push_str("</dl>\n");

    if session.messages.is_empty() {
        html.push_str("<p class=\"empty\">No messages in this session.</p>\n");
    }
    for (timestamp, message) in &session.messages {
        html.push_str(&format!(
            "<div class=\"message {}\">\n",
            html_role_class(message)
        ));
        html.push_str(&format!(
            "<div class=\"head\">{} · {}</div>\n",
            html_escape(&html_role_label(message)),
            html_escape(&iso_timestamp(*timestamp))
        ));
        html.push_str("<div class=\"body\">");
        let content = render_content(message);
        if content.is_empty() {
            html.push_str("<span class=\"empty\">(no content)</span>");
        } else {
            html.push_str(&html_escape(&content));
        }
        if let Some(tool_calls) = message
            .tool_calls
            .as_ref()
            .filter(|calls| !calls.is_empty())
        {
            let pretty =
                serde_json::to_string_pretty(tool_calls).unwrap_or_else(|_| "[]".to_string());
            html.push_str(&format!("<pre><code>{}</code></pre>", html_escape(&pretty)));
        }
        html.push_str("</div>\n</div>\n");
    }

    if include_verification {
        html.push_str("<footer>\n");
        html.push_str(&format!(
            "<div>Exported {} · {} message{} · {}</div>\n",
            html_escape(&exported_iso),
            message_count,
            if message_count != 1 { "s" } else { "" },
            html_escape(EXPORTER_VERSION)
        ));
        html.push_str("</footer>\n");
    }
    html.push_str("</body>\n</html>\n");
    html
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect()
}

/// Re-verify exported Markdown: recomputes the digest with the SHA line
/// reset to `pending` and compares. Returns `(ok, embedded_sha)`.
pub fn verify_export_content(content: &str) -> (bool, String) {
    let re = regex::Regex::new(r"SHA256 of exported body: `([0-9a-f]{64})`").unwrap();
    let Some(captures) = re.captures(content) else {
        return (false, String::new());
    };
    let embedded = captures
        .get(1)
        .map(|m| m.as_str().to_string())
        .unwrap_or_default();
    let pending_body = re
        .replace(content, "SHA256 of exported body: `pending`")
        .to_string();
    (sha256_hex(pending_body.as_bytes()) == embedded, embedded)
}

/// Deterministic path-safe filename (hermes `safe_session_filename`).
pub fn safe_session_filename(session_id: &str, title: Option<&str>, fmt: &str) -> String {
    let slug_source = title.filter(|t| !t.is_empty()).unwrap_or("session");
    let mut slug: String = slug_source
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches(|c| matches!(c, '.' | '-' | '_'))
        .to_lowercase();
    if slug.is_empty() {
        slug = "session".to_string();
    }
    if slug.len() > 60 {
        slug.truncate(60);
    }
    format!("{}-{}.{}", session_id, slug, fmt)
}

/// Write the export file and append a `manifest.jsonl` entry next to it
/// (hermes `write_session_markdown` + `append_manifest_entry`).
pub fn write_session_markdown(output_dir: &Path, session: &ExportSession) -> Result<PathBuf> {
    write_session_export(output_dir, session, "md")
}

/// Write the export file in the given format (`md` or `html`) and append a
/// `manifest.jsonl` entry next to it.
pub fn write_session_export(
    output_dir: &Path,
    session: &ExportSession,
    fmt: &str,
) -> Result<PathBuf> {
    let body = match fmt {
        "md" => render_session_markdown(session, true),
        "html" => render_session_html(session, true),
        other => {
            return Err(AgentError::session(format!(
                "unsupported export format: {}",
                other
            )))
        }
    };
    std::fs::create_dir_all(output_dir)
        .map_err(|e| AgentError::session(format!("create export dir: {}", e)))?;
    let path = output_dir.join(safe_session_filename(
        &session.id,
        session.title.as_deref(),
        fmt,
    ));
    std::fs::write(&path, &body)
        .map_err(|e| AgentError::session(format!("write export: {}", e)))?;

    let entry = json!({
        "session_id": session.id,
        "lineage_session_ids": [session.id],
        "path": path.display().to_string(),
        "format": fmt,
        "message_count": session.messages.len(),
        "sha256": sha256_hex(body.as_bytes()),
        "exported_at": now_secs(),
    });
    let manifest = output_dir.join("manifest.jsonl");
    let mut line = serde_json::to_string(&entry).unwrap_or_default();
    line.push('\n');
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&manifest)
        .and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()))
        .map_err(|e| AgentError::session(format!("append manifest: {}", e)))?;
    Ok(path)
}

/// P740: raw-mode session rendering (hermes trace-display parity) —
/// every message with timestamp, role, tool name/id and tool_calls
/// JSON; no decoration, stable and grep-friendly. With `json` the
/// transcript renders as a JSON array instead (one object per message).
pub fn render_session_raw(messages: &[(f64, crate::provider::Message)], json: bool) -> String {
    if json {
        let rows: Vec<serde_json::Value> = messages
            .iter()
            .map(|(ts, message)| {
                serde_json::json!({
                    "timestamp": ts,
                    "time": crate::message_timestamps::format_message_timestamp(*ts),
                    "role": message.role.to_string(),
                    "content": message.content,
                    "name": message.name,
                    "tool_call_id": message.tool_call_id,
                    "tool_calls": message.tool_calls,
                })
            })
            .collect();
        return serde_json::to_string_pretty(&rows).unwrap_or_else(|_| "[]".to_string());
    }
    let mut out = String::new();
    for (ts, message) in messages {
        let time = crate::message_timestamps::format_message_timestamp(*ts);
        let mut header = format!("[{time}] role={}", message.role);
        if let Some(name) = &message.name {
            header.push_str(&format!(" tool={name}"));
        }
        if let Some(id) = &message.tool_call_id {
            header.push_str(&format!(" tool_call_id={id}"));
        }
        if let Some(calls) = &message.tool_calls {
            header.push_str(&format!(" tool_calls={}", calls.len()));
        }
        out.push_str(&header);
        out.push('\n');
        if let Some(content) = &message.content {
            if !content.is_empty() {
                out.push_str(content);
                if !content.ends_with('\n') {
                    out.push('\n');
                }
            }
        }
        if let Some(calls) = &message.tool_calls {
            for call in calls {
                out.push_str(&format!(
                    "  tool_call id={} function={} arguments={}\n",
                    call.id, call.function.name, call.function.arguments
                ));
            }
        }
    }
    out
}

/// P740: pretty-view line for `sessions show` with an optional
/// timestamp prefix (`--timestamps`).
pub fn render_show_line(
    ts: f64,
    message: &crate::provider::Message,
    with_timestamp: bool,
) -> String {
    let content = message.content.as_deref().unwrap_or("");
    if with_timestamp {
        let time = crate::message_timestamps::format_message_timestamp(ts);
        format!("── {} [{}] ──\n{}", message.role, time, content)
    } else {
        format!("── {} ──\n{}", message.role, content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{FunctionCall, Role, ToolCall};

    fn sample_session() -> ExportSession {
        ExportSession {
            id: "sess-123".into(),
            title: Some("My Session".into()),
            source: "cli".into(),
            model: Some("test-model".into()),
            cwd: Some("/tmp/work".into()),
            started_at: 1_750_000_000.0,
            ended_at: None,
            messages: vec![
                (
                    1_750_000_010.0,
                    Message {
                        role: Role::User,
                        content: Some("hello".into()),
                        tool_calls: None,
                        tool_call_id: None,
                        name: None,
                    },
                ),
                (
                    1_750_000_020.0,
                    Message {
                        role: Role::Assistant,
                        content: Some("running ls".into()),
                        tool_calls: Some(vec![ToolCall {
                            id: "call_1".into(),
                            call_type: "function".into(),
                            function: FunctionCall {
                                name: "terminal".into(),
                                arguments: "{\"command\":\"ls\"}".into(),
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
                        content: Some("file.txt".into()),
                        tool_calls: None,
                        tool_call_id: Some("call_1".into()),
                        name: Some("terminal".into()),
                    },
                ),
            ],
        }
    }

    #[test]
    fn test_render_markdown_structure() {
        let session = sample_session();
        let body = render_session_markdown_at(&session, 1_750_000_999.0, true);
        assert!(body.starts_with("---\n"));
        assert!(body.contains("session_id: \"sess-123\""));
        assert!(body.contains("title: \"My Session\""));
        assert!(body.contains("exporter: \"ulnclaw sessions export (md) v1\""));
        assert!(body.contains("# My Session"));
        assert!(body.contains("Session ID: `sess-123`"));
        assert!(body.contains("Working directory: `/tmp/work`"));
        assert!(body.contains("### User — 2025-06-15T15:06:50Z"));
        assert!(body.contains("### Tool — terminal —"));
        assert!(body.contains("## Tool calls"));
        assert!(body.contains("\"name\": \"terminal\""));
        assert!(body.contains("## Export verification"));
        assert!(body.contains("SHA256 of exported body: `"));
    }

    #[test]
    fn test_verify_roundtrip_and_tamper() {
        let session = sample_session();
        let body = render_session_markdown_at(&session, 1_750_000_999.0, true);
        let (ok, sha) = verify_export_content(&body);
        assert!(ok, "fresh export should verify");
        assert_eq!(sha.len(), 64);

        let tampered = body.replace("hello", "HELLO");
        let (ok, _) = verify_export_content(&tampered);
        assert!(!ok, "tampered export must fail verification");

        let (ok, _) = verify_export_content("no verification footer here");
        assert!(!ok);
    }

    #[test]
    fn test_safe_filename() {
        assert_eq!(
            safe_session_filename("abc", Some("My Cool Session!"), "md"),
            "abc-my-cool-session.md"
        );
        assert_eq!(safe_session_filename("abc", None, "md"), "abc-session.md");
        assert_eq!(
            safe_session_filename("abc", Some("///"), "md"),
            "abc-session.md"
        );
        assert_eq!(
            safe_session_filename("abc", Some("My Cool Session!"), "html"),
            "abc-my-cool-session.html"
        );
        let long_title = "x".repeat(100);
        let name = safe_session_filename("id", Some(&long_title), "md");
        assert!(name.len() <= "id-".len() + 60 + 3);
    }

    #[test]
    fn test_write_and_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let session = sample_session();
        let path = write_session_markdown(dir.path(), &session).unwrap();
        assert!(path.exists());
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(verify_export_content(&content).0);

        let manifest = std::fs::read_to_string(dir.path().join("manifest.jsonl")).unwrap();
        let entry: serde_json::Value =
            serde_json::from_str(manifest.lines().next().unwrap()).unwrap();
        assert_eq!(entry["session_id"], "sess-123");
        assert_eq!(entry["format"], "md");
        assert_eq!(entry["message_count"], 3);
        assert_eq!(entry["sha256"].as_str().unwrap().len(), 64);
    }

    #[test]
    fn test_render_html_structure_and_escaping() {
        let mut session = sample_session();
        // Inject markup that must be escaped.
        session.messages.push((
            1_750_000_040.0,
            Message {
                role: Role::User,
                content: Some("<script>alert('x')</script>".into()),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            },
        ));
        let html = render_session_html_at(&session, 1_750_000_999.0, true);
        assert!(html.starts_with("<!DOCTYPE html>"));
        assert!(html.contains("<title>"));
        assert!(html.contains("Session ID:"));
        assert!(html.contains("class=\"message user\""));
        assert!(html.contains("class=\"message assistant\""));
        assert!(html.contains("Tool — terminal"));
        assert!(html.contains("<pre><code>"));
        assert!(html.contains("Exported 2025-06-15T15:23:19Z"));
        // Escaped, never raw.
        assert!(!html.contains("<script>alert"));
        assert!(html.contains("&lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt;"));
    }

    #[test]
    fn test_write_html_and_format_error() {
        let dir = tempfile::tempdir().unwrap();
        let session = sample_session();
        let path = write_session_export(dir.path(), &session, "html").unwrap();
        assert!(path.to_string_lossy().ends_with(".html"));
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("<!DOCTYPE html>"));
        let manifest = std::fs::read_to_string(dir.path().join("manifest.jsonl")).unwrap();
        let entry: serde_json::Value =
            serde_json::from_str(manifest.lines().next().unwrap()).unwrap();
        assert_eq!(entry["format"], "html");

        assert!(write_session_export(dir.path(), &session, "pdf").is_err());
    }

    #[test]
    fn test_render_session_raw_text() {
        use crate::provider::{FunctionCall, Message, Role, ToolCall};
        let messages = vec![
            (
                1770700000.0,
                Message {
                    role: Role::User,
                    content: Some("hello".into()),
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                },
            ),
            (
                1770700010.0,
                Message {
                    role: Role::Assistant,
                    content: Some("".into()),
                    tool_calls: Some(vec![ToolCall {
                        id: "call_1".into(),
                        call_type: "function".into(),
                        function: FunctionCall {
                            name: "search".into(),
                            arguments: "{\"q\":\"x\"}".into(),
                        },
                    }]),
                    tool_call_id: None,
                    name: None,
                },
            ),
            (
                1770700020.0,
                Message {
                    role: Role::Tool,
                    content: Some("results".into()),
                    tool_calls: None,
                    tool_call_id: Some("call_1".into()),
                    name: Some("search".into()),
                },
            ),
        ];
        let out = super::render_session_raw(&messages, false);
        assert!(out.contains("role=user"), "{out}");
        assert!(out.contains("hello"), "{out}");
        assert!(out.contains("tool_calls=1"), "{out}");
        assert!(out.contains("function=search"), "{out}");
        assert!(out.contains("tool_call_id=call_1"), "{out}");
        assert!(out.contains("results"), "{out}");
    }

    #[test]
    fn test_render_session_raw_json() {
        use crate::provider::{Message, Role};
        let messages = vec![(
            1770700000.0,
            Message {
                role: Role::User,
                content: Some("hi".into()),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            },
        )];
        let out = super::render_session_raw(&messages, true);
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed[0]["role"], "user");
        assert_eq!(parsed[0]["content"], "hi");
        assert!(parsed[0]["timestamp"].is_number());
    }

    #[test]
    fn test_render_show_line_timestamp_toggle() {
        use crate::provider::{Message, Role};
        let message = Message {
            role: Role::User,
            content: Some("text".into()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        };
        let plain = super::render_show_line(1770700000.0, &message, false);
        let stamped = super::render_show_line(1770700000.0, &message, true);
        assert!(plain.starts_with("── user ──"), "{plain}");
        assert!(stamped.contains("user ["), "{stamped}");
    }
}

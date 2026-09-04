//! x_search — port of hermes' tools/x_search_tool.py
//!
//! Search X (Twitter) posts via xAI's built-in `x_search` Responses-API
//! server tool. Registered only when `XAI_API_KEY` is available (hermes
//! additionally supports SuperGrok OAuth; ulnclaw uses the API key path).
//!
//! Defensive output parity: client-side date validation (strict
//! YYYY-MM-DD, no inverted or pure-future ranges), handle normalisation
//! (max 10, `@` stripped, allow/exclude mutually exclusive), retries on
//! 5xx/transient failures, and `degraded`/`degraded_reason` markers when
//! narrowing filters were active but xAI returned no citations.

use std::time::Duration;

use crate::tools::{tool, ToolAvailability, ToolRegistry};
use serde_json::{json, Value};

const DEFAULT_XAI_BASE_URL: &str = "https://api.x.ai/v1";
const DEFAULT_X_SEARCH_MODEL: &str = "grok-4.5";
const REASONING_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh"];
const MAX_HANDLES: usize = 10;
/// Hermes `max_result_size_chars` for this tool.
const MAX_RESULT_CHARS: usize = 100_000;

pub fn register(registry: &mut ToolRegistry) {
    registry.register(x_search_tool());
}

fn check_x_search_requirements() -> ToolAvailability {
    if crate::config::get_env_value("XAI_API_KEY")
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
    {
        ToolAvailability::available()
    } else {
        ToolAvailability::unavailable("x_search needs XAI_API_KEY")
    }
}

fn normalize_handles(value: Option<&Value>, field_name: &str) -> Result<Vec<String>, String> {
    let mut cleaned = Vec::new();
    if let Some(items) = value.and_then(|v| v.as_array()) {
        for item in items {
            let handle = item
                .as_str()
                .unwrap_or("")
                .trim()
                .trim_start_matches('@')
                .to_string();
            if !handle.is_empty() {
                cleaned.push(handle);
            }
        }
    }
    if cleaned.len() > MAX_HANDLES {
        return Err(format!(
            "{field_name} supports at most {MAX_HANDLES} handles"
        ));
    }
    Ok(cleaned)
}

fn parse_iso_date(value: &str, field_name: &str) -> Result<chrono::NaiveDate, String> {
    chrono::NaiveDate::parse_from_str(value.trim(), "%Y-%m-%d")
        .map_err(|_| format!("{field_name} must be YYYY-MM-DD (got {value:?})"))
}

/// Hermes `_validate_date_range`: strict formats, from <= to, and
/// `from_date` must not be later than today UTC (X only indexes the past).
fn validate_date_range(from_date: &str, to_date: &str) -> Result<(), String> {
    let parsed_from = if from_date.trim().is_empty() {
        None
    } else {
        Some(parse_iso_date(from_date, "from_date")?)
    };
    let parsed_to = if to_date.trim().is_empty() {
        None
    } else {
        Some(parse_iso_date(to_date, "to_date")?)
    };
    if let (Some(from), Some(to)) = (parsed_from, parsed_to) {
        if from > to {
            return Err(format!(
                "from_date ({from}) must be on or before to_date ({to})"
            ));
        }
    }
    if let Some(from) = parsed_from {
        let today_utc = chrono::Utc::now().date_naive();
        if from > today_utc {
            return Err(format!(
                "from_date ({from}) is in the future; X Search only indexes past posts \
                 (today UTC is {today_utc})"
            ));
        }
    }
    Ok(())
}

fn extract_response_text(payload: &Value) -> String {
    let output_text = payload
        .get("output_text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if !output_text.is_empty() {
        return output_text.to_string();
    }
    let mut parts = Vec::new();
    for item in payload
        .get("output")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
    {
        if item.get("type").and_then(|v| v.as_str()) != Some("message") {
            continue;
        }
        for content in item
            .get("content")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
        {
            let ctype = content.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if ctype == "output_text" || ctype == "text" {
                let text = content
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim();
                if !text.is_empty() {
                    parts.push(text.to_string());
                }
            }
        }
    }
    parts.join("\n\n")
}

fn extract_inline_citations(payload: &Value) -> Vec<Value> {
    let mut citations = Vec::new();
    for item in payload
        .get("output")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
    {
        if item.get("type").and_then(|v| v.as_str()) != Some("message") {
            continue;
        }
        for content in item
            .get("content")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
        {
            for annotation in content
                .get("annotations")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default()
            {
                if annotation.get("type").and_then(|v| v.as_str()) != Some("url_citation") {
                    continue;
                }
                citations.push(json!({
                    "url": annotation.get("url").and_then(|v| v.as_str()).unwrap_or(""),
                    "title": annotation.get("title").and_then(|v| v.as_str()).unwrap_or(""),
                    "start_index": annotation.get("start_index"),
                    "end_index": annotation.get("end_index"),
                }));
            }
        }
    }
    citations
}

/// Compact hermes-style HTTP error message (code + error fields).
fn http_error_message(status: u16, body: &str) -> String {
    if let Ok(payload) = serde_json::from_str::<Value>(body) {
        if payload.is_object() {
            let code = payload
                .get("code")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let error = payload
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let message = if error.is_empty() {
                body.to_string()
            } else {
                error
            };
            let message = if !code.is_empty() && !message.contains(&code) {
                format!("{code}: {message}")
            } else {
                message
            };
            if !message.is_empty() {
                return message.chars().take(500).collect();
            }
        }
    }
    let text = body.trim();
    if text.is_empty() {
        format!("HTTP {status}")
    } else {
        format!(
            "HTTP {status}: {}",
            text.chars().take(500).collect::<String>()
        )
    }
}

fn clamp_result(value: String) -> String {
    if value.chars().count() <= MAX_RESULT_CHARS {
        value
    } else {
        value.chars().take(MAX_RESULT_CHARS).collect()
    }
}

fn x_search_tool() -> crate::tools::Tool {
    tool("x_search")
        .description(
            "Search X (Twitter) posts, profiles, and threads using xAI's built-in X Search \
             tool. Read-only discovery only: use this for current discussion, reactions, or \
             claims on public X rather than general web pages. Do not use it to post, reply, \
             like, DM, upload media, delete, or inspect the user's authenticated X account — \
             those require a separate authenticated X API surface outside this tool. \
             Available when xAI credentials are configured (XAI_API_KEY).",
        )
        .parameters(json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "What to look up on X."},
                "allowed_x_handles": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Optional list of X handles to include exclusively (max 10)."
                },
                "excluded_x_handles": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Optional list of X handles to exclude (max 10)."
                },
                "from_date": {"type": "string", "description": "Optional start date in YYYY-MM-DD format."},
                "to_date": {"type": "string", "description": "Optional end date in YYYY-MM-DD format."},
                "enable_image_understanding": {
                    "type": "boolean",
                    "description": "Whether xAI should analyze images attached to matching X posts.",
                    "default": false
                },
                "enable_video_understanding": {
                    "type": "boolean",
                    "description": "Whether xAI should analyze videos attached to matching X posts.",
                    "default": false
                }
            },
            "required": ["query"]
        }))
        .handler(|args, ctx| async move {
            let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
            if query.is_empty() {
                return Ok(json!({"success": false, "provider": "xai", "tool": "x_search", "error": "query is required for x_search"}));
            }
            let Some(api_key) = crate::config::get_env_value("XAI_API_KEY").map(|v| v.trim().to_string()).filter(|v| !v.is_empty()) else {
                return Ok(json!({
                    "success": false,
                    "provider": "xai",
                    "tool": "x_search",
                    "error": "No xAI credentials available. Set XAI_API_KEY."
                }));
            };

            let allowed = match normalize_handles(args.get("allowed_x_handles"), "allowed_x_handles") {
                Ok(v) => v,
                Err(e) => return Ok(json!({"success": false, "provider": "xai", "tool": "x_search", "error": e})),
            };
            let excluded = match normalize_handles(args.get("excluded_x_handles"), "excluded_x_handles") {
                Ok(v) => v,
                Err(e) => return Ok(json!({"success": false, "provider": "xai", "tool": "x_search", "error": e})),
            };
            if !allowed.is_empty() && !excluded.is_empty() {
                return Ok(json!({
                    "success": false, "provider": "xai", "tool": "x_search",
                    "error": "allowed_x_handles and excluded_x_handles cannot be used together"
                }));
            }

            let from_date = args.get("from_date").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let to_date = args.get("to_date").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if let Err(e) = validate_date_range(&from_date, &to_date) {
                return Ok(json!({"success": false, "provider": "xai", "tool": "x_search", "error": e}));
            }

            // [x_search] config: model / reasoning_effort / timeout_seconds / retries.
            let xs = &ctx.config.x_search;
            let model = xs.model.trim();
            let model = if model.is_empty() { DEFAULT_X_SEARCH_MODEL } else { model };
            let reasoning_effort = xs.reasoning_effort.trim().to_ascii_lowercase();
            let reasoning_effort = if reasoning_effort.is_empty() {
                None
            } else if REASONING_EFFORTS.contains(&reasoning_effort.as_str()) {
                Some(reasoning_effort)
            } else {
                return Ok(json!({
                    "success": false, "provider": "xai", "tool": "x_search",
                    "error": format!(
                        "x_search.reasoning_effort must be one of: {} (got {:?})",
                        REASONING_EFFORTS.join(", "), reasoning_effort
                    )
                }));
            };
            let timeout_seconds = xs.timeout_seconds.max(30);
            let max_retries = xs.retries;

            let mut tool_def = json!({"type": "x_search"});
            if !allowed.is_empty() {
                tool_def["allowed_x_handles"] = json!(allowed);
            }
            if !excluded.is_empty() {
                tool_def["excluded_x_handles"] = json!(excluded);
            }
            if !from_date.trim().is_empty() {
                tool_def["from_date"] = json!(from_date.trim());
            }
            if !to_date.trim().is_empty() {
                tool_def["to_date"] = json!(to_date.trim());
            }
            if args.get("enable_image_understanding").and_then(|v| v.as_bool()).unwrap_or(false) {
                tool_def["enable_image_understanding"] = json!(true);
            }
            if args.get("enable_video_understanding").and_then(|v| v.as_bool()).unwrap_or(false) {
                tool_def["enable_video_understanding"] = json!(true);
            }

            let mut payload = json!({
                "model": model,
                "input": [{"role": "user", "content": query}],
                "tools": [tool_def],
                "store": false,
            });
            if let Some(ref effort) = reasoning_effort {
                payload["reasoning"] = json!({"effort": effort});
            }

            let base_url = crate::config::get_env_value("XAI_BASE_URL")
                .map(|v| v.trim().trim_end_matches('/').to_string())
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| DEFAULT_XAI_BASE_URL.to_string());
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(timeout_seconds))
                .build()
                .unwrap_or_default();

            let mut last_error = String::from("x_search request did not return a response");
            let mut data: Option<Value> = None;
            for attempt in 0..=max_retries {
                let response = client
                    .post(format!("{base_url}/responses"))
                    .bearer_auth(&api_key)
                    .header("User-Agent", format!("ulnclaw/{}", crate::VERSION))
                    .json(&payload)
                    .send()
                    .await;
                match response {
                    Ok(resp) => {
                        let status = resp.status().as_u16();
                        let body = resp.text().await.unwrap_or_default();
                        if status >= 500 && attempt < max_retries {
                            tracing::warn!(
                                "x_search upstream failure on attempt {}/{}: {}",
                                attempt + 1,
                                max_retries + 1,
                                http_error_message(status, &body)
                            );
                            last_error = http_error_message(status, &body);
                            tokio::time::sleep(Duration::from_secs_f64(
                                (1.5 * (attempt + 1) as f64).min(5.0),
                            ))
                            .await;
                            continue;
                        }
                        if status >= 400 {
                            last_error = http_error_message(status, &body);
                            return Ok(json!({
                                "success": false, "provider": "xai", "tool": "x_search",
                                "error": last_error, "error_type": format!("HTTP {status}")
                            }));
                        }
                        match serde_json::from_str::<Value>(&body) {
                            Ok(parsed) => {
                                data = Some(parsed);
                                break;
                            }
                            Err(e) => {
                                last_error = format!("invalid JSON from xAI: {e}");
                                return Ok(json!({
                                    "success": false, "provider": "xai", "tool": "x_search",
                                    "error": last_error, "error_type": "invalid_json"
                                }));
                            }
                        }
                    }
                    Err(e) => {
                        if e.is_timeout() {
                            if attempt >= max_retries {
                                return Ok(json!({
                                    "success": false, "provider": "xai", "tool": "x_search",
                                    "error": format!("xAI x_search timed out after {timeout_seconds} seconds"),
                                    "error_type": "timeout"
                                }));
                            }
                        } else if attempt >= max_retries {
                            return Ok(json!({
                                "success": false, "provider": "xai", "tool": "x_search",
                                "error": e.to_string(), "error_type": "connection"
                            }));
                        }
                        tracing::warn!(
                            "x_search transient failure on attempt {}/{}: {e}",
                            attempt + 1,
                            max_retries + 1
                        );
                        last_error = e.to_string();
                        tokio::time::sleep(Duration::from_secs_f64(
                            (1.5 * (attempt + 1) as f64).min(5.0),
                        ))
                        .await;
                    }
                }
            }

            let Some(data) = data else {
                return Ok(json!({
                    "success": false, "provider": "xai", "tool": "x_search",
                    "error": last_error, "error_type": "no_response"
                }));
            };

            let answer = extract_response_text(&data);
            let citations = data.get("citations").and_then(|v| v.as_array()).cloned().unwrap_or_default();
            let inline_citations = extract_inline_citations(&data);

            // Degraded-result detection (hermes): narrowing filters active
            // but no citations in either channel → answer came from the
            // model's own knowledge, not the X index.
            let mut active_filters: Vec<&str> = Vec::new();
            if !allowed.is_empty() {
                active_filters.push("allowed_x_handles");
            }
            if !excluded.is_empty() {
                active_filters.push("excluded_x_handles");
            }
            if !from_date.trim().is_empty() {
                active_filters.push("from_date");
            }
            if !to_date.trim().is_empty() {
                active_filters.push("to_date");
            }
            let degraded = !active_filters.is_empty() && citations.is_empty() && inline_citations.is_empty();
            let degraded_reason = if degraded {
                Some(format!(
                    "no citations returned despite filters: {}",
                    active_filters.join(", ")
                ))
            } else {
                None
            };

            Ok(json!({
                "success": true,
                "provider": "xai",
                "credential_source": "xai",
                "tool": "x_search",
                "model": model,
                "query": query,
                "answer": clamp_result(answer),
                "citations": citations,
                "inline_citations": inline_citations,
                "degraded": degraded,
                "degraded_reason": degraded_reason,
            }))
        })
        .toolset("x_search")
        .emoji("🐦")
        .check_fn(check_x_search_requirements)
        .build()
        .expect("x_search builds")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_normalized_and_capped() {
        let list = json!(["@alice", "bob", "  ", "@carol"]);
        let out = normalize_handles(Some(&list), "allowed_x_handles").unwrap();
        assert_eq!(out, vec!["alice", "bob", "carol"]);
        let many: Vec<String> = (0..11).map(|i| format!("user{i}")).collect();
        let list = json!(many);
        assert!(normalize_handles(Some(&list), "allowed_x_handles").is_err());
        assert!(normalize_handles(None, "allowed_x_handles")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn date_range_validation() {
        assert!(validate_date_range("", "").is_ok());
        assert!(validate_date_range("2026-01-01", "2026-01-31").is_ok());
        assert!(
            validate_date_range("2026-01-31", "2026-01-01").is_err(),
            "inverted"
        );
        assert!(validate_date_range("not-a-date", "").is_err(), "malformed");
        assert!(
            validate_date_range("2099-01-01", "").is_err(),
            "future from_date"
        );
        // to_date in the future is allowed.
        assert!(validate_date_range("2026-01-01", "2099-01-01").is_ok());
    }

    #[test]
    fn response_text_and_citations_extraction() {
        let payload = json!({
            "output_text": "direct answer",
        });
        assert_eq!(extract_response_text(&payload), "direct answer");

        let payload = json!({
            "output": [
                {"type": "reasoning", "content": [{"type": "text", "text": "hidden"}]},
                {"type": "message", "content": [
                    {"type": "output_text", "text": "part one", "annotations": [
                        {"type": "url_citation", "url": "https://x.com/a/status/1", "title": "Post A", "start_index": 0, "end_index": 4},
                        {"type": "other"}
                    ]},
                    {"type": "text", "text": "part two"}
                ]}
            ]
        });
        assert_eq!(extract_response_text(&payload), "part one\n\npart two");
        let inline = extract_inline_citations(&payload);
        assert_eq!(inline.len(), 1);
        assert_eq!(inline[0]["url"], "https://x.com/a/status/1");
    }

    #[test]
    fn http_error_message_formats() {
        assert_eq!(
            http_error_message(402, r#"{"code":"insufficient_quota","error":"no credits"}"#),
            "insufficient_quota: no credits"
        );
        assert_eq!(http_error_message(500, "boom"), "HTTP 500: boom");
        assert_eq!(http_error_message(503, ""), "HTTP 503");
    }

    #[test]
    fn result_clamped_to_limit() {
        let big: String = "x".repeat(MAX_RESULT_CHARS + 10);
        assert_eq!(clamp_result(big).chars().count(), MAX_RESULT_CHARS);
        assert_eq!(clamp_result("small".into()), "small");
    }
}

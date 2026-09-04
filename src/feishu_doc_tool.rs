//! Feishu/Lark document + drive comment tools (hermes
//! `tools/feishu_doc_tool.py` + `tools/feishu_drive_tool.py`).
//!
//! Five tools across two toolsets:
//! - `feishu_doc_read` (`feishu_doc`) — read a document's full content as
//!   plain text (`/open-apis/docx/v1/documents/:id/raw_content`).
//! - `feishu_drive_list_comments`, `feishu_drive_list_comment_replies`,
//!   `feishu_drive_reply_comment`, `feishu_drive_add_comment`
//!   (`feishu_drive`) — document comment thread operations.
//!
//! hermes injects a thread-local lark client from the Feishu comment event
//! handler, so there the tools only work inside a comment context. ulnclaw
//! resolves app credentials directly — secret scope → env/`.env` →
//! `[messaging.feishu]` config — so the tools work in any session whenever
//! credentials are configured (a superset of hermes behavior). Tenant
//! tokens are cached process-wide per app id and refreshed early (~110
//! min), mirroring `feishu.rs`.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const RAW_CONTENT_URI: &str = "/open-apis/docx/v1/documents/{document_id}/raw_content";
const LIST_COMMENTS_URI: &str = "/open-apis/drive/v1/files/{file_token}/comments";
const LIST_REPLIES_URI: &str =
    "/open-apis/drive/v1/files/{file_token}/comments/{comment_id}/replies";
const ADD_COMMENT_URI: &str = "/open-apis/drive/v1/files/{file_token}/new_comments";

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Tenant tokens live ~2 h; refresh early (`feishu.rs` parity).
const TOKEN_REFRESH_SECS: u64 = 6600;
/// Feishu `page_size` hard cap.
const PAGE_SIZE_MAX: u64 = 100;

/// Resolved Feishu app credentials for the Open API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeishuAppCredentials {
    pub app_id: String,
    pub app_secret: String,
}

/// Credential resolution order:
/// 1. profile secret scope (`FEISHU_APP_ID` / `FEISHU_APP_SECRET`),
/// 2. process env / `.env` (`config::get_env_value`),
/// 3. `[messaging.feishu]` config file fields.
pub fn resolve_credentials() -> Option<FeishuAppCredentials> {
    let mut app_id = String::new();
    let mut app_secret = String::new();

    if let Ok(cfg) = crate::config::UlncLawConfig::load(None) {
        app_id = cfg.messaging.feishu.app_id.trim().to_string();
        app_secret = cfg.messaging.feishu.app_secret.trim().to_string();
    }
    let pairs: [(&str, &mut String); 2] = [
        ("FEISHU_APP_ID", &mut app_id),
        ("FEISHU_APP_SECRET", &mut app_secret),
    ];
    for (name, slot) in pairs {
        if let Some(value) = crate::config::get_env_value(name) {
            let value = value.trim().to_string();
            if !value.is_empty() {
                *slot = value;
            }
        }
        if let Some(value) = crate::secret_scope::get_secret_lenient(name, None) {
            let value = value.trim().to_string();
            if !value.is_empty() {
                *slot = value;
            }
        }
    }
    if app_id.is_empty() || app_secret.is_empty() {
        None
    } else {
        Some(FeishuAppCredentials { app_id, app_secret })
    }
}

fn token_cache() -> &'static tokio::sync::Mutex<HashMap<String, (String, Instant)>> {
    static CACHE: OnceLock<tokio::sync::Mutex<HashMap<String, (String, Instant)>>> =
        OnceLock::new();
    CACHE.get_or_init(|| tokio::sync::Mutex::new(HashMap::new()))
}

/// Tenant access token with early-refresh caching (hermes lark client
/// `AccessTokenType.TENANT` parity).
async fn tenant_access_token(creds: &FeishuAppCredentials) -> Result<String, String> {
    {
        let cache = token_cache().lock().await;
        if let Some((token, fetched_at)) = cache.get(&creds.app_id) {
            if fetched_at.elapsed() < Duration::from_secs(TOKEN_REFRESH_SECS) {
                return Ok(token.clone());
            }
        }
    }
    let client = reqwest::Client::new();
    let resp = client
        .post(format!(
            "{}/open-apis/auth/v3/tenant_access_token/internal",
            crate::feishu::OPEN_API_BASE
        ))
        .json(&json!({"app_id": creds.app_id, "app_secret": creds.app_secret}))
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("tenant token: {e}"))?;
    let value: Value = resp
        .json()
        .await
        .map_err(|e| format!("tenant token JSON: {e}"))?;
    let code = value.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
    if code != 0 {
        return Err(format!(
            "tenant token error: {}",
            value
                .get("msg")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
        ));
    }
    let token = value
        .get("tenant_access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "tenant token missing".to_string())?
        .to_string();
    token_cache()
        .lock()
        .await
        .insert(creds.app_id.clone(), (token.clone(), Instant::now()));
    Ok(token)
}

/// One Open API request — returns `(code, msg, data)` (hermes
/// `feishu_drive_tool._do_request` parity).
async fn feishu_call(
    creds: &FeishuAppCredentials,
    method: reqwest::Method,
    uri: &str,
    paths: &[(&str, &str)],
    queries: &[(&str, &str)],
    body: Option<&Value>,
) -> (i64, String, Value) {
    let token = match tenant_access_token(creds).await {
        Ok(t) => t,
        Err(e) => return (-1, e, json!({})),
    };
    let mut url = uri.to_string();
    for (key, value) in paths {
        url = url.replace(&format!("{{{key}}}"), value);
    }
    let client = reqwest::Client::new();
    let mut request = client
        .request(method, format!("{}{}", crate::feishu::OPEN_API_BASE, url))
        .bearer_auth(&token)
        .timeout(REQUEST_TIMEOUT);
    for (key, value) in queries {
        request = request.query(&[(key, value)]);
    }
    if let Some(body) = body {
        request = request.json(body);
    }
    let response = match request.send().await {
        Ok(r) => r,
        Err(e) => return (-1, e.to_string(), json!({})),
    };
    let value: Value = response.json().await.unwrap_or(json!({}));
    let code = value.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
    let msg = value
        .get("msg")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let data = value.get("data").cloned().unwrap_or(json!({}));
    (code, msg, data)
}

fn arg_str(args: &Value, key: &str) -> String {
    args.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string()
}

fn arg_str_or(args: &Value, key: &str, default: &str) -> String {
    let value = arg_str(args, key);
    if value.is_empty() {
        default.to_string()
    } else {
        value
    }
}

/// hermes `page_size` default 100, clamped to 1..=100.
fn clamp_page_size(args: &Value) -> u64 {
    let raw = match args.get("page_size") {
        Some(Value::Number(n)) => n.as_u64().unwrap_or(100),
        Some(Value::String(s)) => s.trim().parse::<u64>().unwrap_or(100),
        _ => 100,
    };
    raw.clamp(1, PAGE_SIZE_MAX)
}

/// `{"success": true}` merged with the API `data` object.
fn success_with_data(data: Value) -> Value {
    let mut out = serde_json::Map::new();
    out.insert("success".to_string(), json!(true));
    match data {
        Value::Object(obj) => {
            for (key, value) in obj {
                out.insert(key, value);
            }
        }
        other => {
            out.insert("data".to_string(), other);
        }
    }
    Value::Object(out)
}

/// hermes `_handle_feishu_doc_read`.
async fn doc_read(creds: &FeishuAppCredentials, args: &Value) -> Result<Value, String> {
    let doc_token = arg_str(args, "doc_token");
    if doc_token.is_empty() {
        return Err("doc_token is required".to_string());
    }
    let (code, msg, data) = feishu_call(
        creds,
        reqwest::Method::GET,
        RAW_CONTENT_URI,
        &[("document_id", doc_token.as_str())],
        &[],
        None,
    )
    .await;
    if code != 0 {
        return Err(format!("Failed to read document: code={code} msg={msg}"));
    }
    let content = data
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Ok(json!({"success": true, "content": content}))
}

/// hermes `_handle_list_comments`.
async fn list_comments(creds: &FeishuAppCredentials, args: &Value) -> Result<Value, String> {
    let file_token = arg_str(args, "file_token");
    if file_token.is_empty() {
        return Err("file_token is required".to_string());
    }
    let file_type = arg_str_or(args, "file_type", "docx");
    let is_whole = args
        .get("is_whole")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let page_size = clamp_page_size(args);
    let page_token = arg_str(args, "page_token");

    let mut queries: Vec<(&str, String)> = vec![
        ("file_type", file_type),
        ("user_id_type", "open_id".to_string()),
        ("page_size", page_size.to_string()),
    ];
    if is_whole {
        queries.push(("is_whole", "true".to_string()));
    }
    if !page_token.is_empty() {
        queries.push(("page_token", page_token));
    }
    let qrefs: Vec<(&str, &str)> = queries.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let (code, msg, data) = feishu_call(
        creds,
        reqwest::Method::GET,
        LIST_COMMENTS_URI,
        &[("file_token", file_token.as_str())],
        &qrefs,
        None,
    )
    .await;
    if code != 0 {
        return Err(format!("List comments failed: code={code} msg={msg}"));
    }
    Ok(success_with_data(data))
}

/// hermes `_handle_list_replies`.
async fn list_replies(creds: &FeishuAppCredentials, args: &Value) -> Result<Value, String> {
    let file_token = arg_str(args, "file_token");
    let comment_id = arg_str(args, "comment_id");
    if file_token.is_empty() || comment_id.is_empty() {
        return Err("file_token and comment_id are required".to_string());
    }
    let file_type = arg_str_or(args, "file_type", "docx");
    let page_size = clamp_page_size(args);
    let page_token = arg_str(args, "page_token");

    let mut queries: Vec<(&str, String)> = vec![
        ("file_type", file_type),
        ("user_id_type", "open_id".to_string()),
        ("page_size", page_size.to_string()),
    ];
    if !page_token.is_empty() {
        queries.push(("page_token", page_token));
    }
    let qrefs: Vec<(&str, &str)> = queries.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let (code, msg, data) = feishu_call(
        creds,
        reqwest::Method::GET,
        LIST_REPLIES_URI,
        &[
            ("file_token", file_token.as_str()),
            ("comment_id", comment_id.as_str()),
        ],
        &qrefs,
        None,
    )
    .await;
    if code != 0 {
        return Err(format!("List replies failed: code={code} msg={msg}"));
    }
    Ok(success_with_data(data))
}

/// hermes `_handle_reply_comment`.
async fn reply_comment(creds: &FeishuAppCredentials, args: &Value) -> Result<Value, String> {
    let file_token = arg_str(args, "file_token");
    let comment_id = arg_str(args, "comment_id");
    let content = arg_str(args, "content");
    if file_token.is_empty() || comment_id.is_empty() || content.is_empty() {
        return Err("file_token, comment_id, and content are required".to_string());
    }
    let file_type = arg_str_or(args, "file_type", "docx");
    let body = json!({
        "content": {
            "elements": [{
                "type": "text_run",
                "text_run": {"text": content},
            }],
        },
    });
    let (code, msg, data) = feishu_call(
        creds,
        reqwest::Method::POST,
        LIST_REPLIES_URI,
        &[
            ("file_token", file_token.as_str()),
            ("comment_id", comment_id.as_str()),
        ],
        &[("file_type", file_type.as_str())],
        Some(&body),
    )
    .await;
    if code != 0 {
        return Err(format!("Reply comment failed: code={code} msg={msg}"));
    }
    Ok(json!({"success": true, "data": data}))
}

/// hermes `_handle_add_comment`.
async fn add_comment(creds: &FeishuAppCredentials, args: &Value) -> Result<Value, String> {
    let file_token = arg_str(args, "file_token");
    let content = arg_str(args, "content");
    if file_token.is_empty() || content.is_empty() {
        return Err("file_token and content are required".to_string());
    }
    let file_type = arg_str_or(args, "file_type", "docx");
    let body = json!({
        "file_type": file_type,
        "reply_elements": [{"type": "text", "text": content}],
    });
    let (code, msg, data) = feishu_call(
        creds,
        reqwest::Method::POST,
        ADD_COMMENT_URI,
        &[("file_token", file_token.as_str())],
        &[],
        Some(&body),
    )
    .await;
    if code != 0 {
        return Err(format!("Add comment failed: code={code} msg={msg}"));
    }
    Ok(json!({"success": true, "data": data}))
}

/// Dispatch one feishu doc/drive tool by name.
pub async fn run_feishu_doc_action(name: &str, args: &Value) -> Result<Value, String> {
    let creds = resolve_credentials().ok_or_else(|| {
        "Feishu client not available (set FEISHU_APP_ID + FEISHU_APP_SECRET or \
         [messaging.feishu] app_id/app_secret)"
            .to_string()
    })?;
    match name {
        "feishu_doc_read" => doc_read(&creds, args).await,
        "feishu_drive_list_comments" => list_comments(&creds, args).await,
        "feishu_drive_list_comment_replies" => list_replies(&creds, args).await,
        "feishu_drive_reply_comment" => reply_comment(&creds, args).await,
        "feishu_drive_add_comment" => add_comment(&creds, args).await,
        _ => Err(format!("Unknown feishu tool: {name}")),
    }
}

fn credentials_availability() -> crate::tools::ToolAvailability {
    if resolve_credentials().is_some() {
        crate::tools::ToolAvailability::available()
    } else {
        crate::tools::ToolAvailability::unavailable(
            "FEISHU_APP_ID/FEISHU_APP_SECRET not configured (env or [messaging.feishu])",
        )
    }
}

pub fn register(registry: &mut crate::tools::ToolRegistry) {
    use crate::tools::tool;

    registry.register(
        tool("feishu_doc_read")
            .description(
                "Read the full content of a Feishu/Lark document as plain text. Useful when \
                 you need more context beyond the quoted text in a comment.",
            )
            .parameters(json!({
                "type": "object",
                "properties": {
                    "doc_token": {
                        "type": "string",
                        "description": "The document token (from the document URL or comment context)."
                    }
                },
                "required": ["doc_token"]
            }))
            .handler(|args, _ctx| async move {
                match run_feishu_doc_action("feishu_doc_read", &args).await {
                    Ok(value) => Ok(value),
                    Err(e) => Ok(json!({"success": false, "error": e})),
                }
            })
            .toolset("feishu_doc")
            .emoji("\u{1f4c4}")
            .check_fn(credentials_availability)
            .build()
            .expect("feishu_doc_read builds"),
    );

    registry.register(
        tool("feishu_drive_list_comments")
            .description(
                "List comments on a Feishu document. Use is_whole=true to list whole-document \
                 comments only.",
            )
            .parameters(json!({
                "type": "object",
                "properties": {
                    "file_token": {"type": "string", "description": "The document file token."},
                    "file_type": {"type": "string", "description": "File type (default: docx).", "default": "docx"},
                    "is_whole": {"type": "boolean", "description": "If true, only return whole-document comments.", "default": false},
                    "page_size": {"type": "integer", "description": "Number of comments per page (max 100).", "default": 100},
                    "page_token": {"type": "string", "description": "Pagination token for next page."}
                },
                "required": ["file_token"]
            }))
            .handler(|args, _ctx| async move {
                match run_feishu_doc_action("feishu_drive_list_comments", &args).await {
                    Ok(value) => Ok(value),
                    Err(e) => Ok(json!({"success": false, "error": e})),
                }
            })
            .toolset("feishu_drive")
            .emoji("\u{1f4ac}")
            .check_fn(credentials_availability)
            .build()
            .expect("feishu_drive_list_comments builds"),
    );

    registry.register(
        tool("feishu_drive_list_comment_replies")
            .description("List replies to a comment thread on a Feishu document.")
            .parameters(json!({
                "type": "object",
                "properties": {
                    "file_token": {"type": "string", "description": "The document file token."},
                    "comment_id": {"type": "string", "description": "The comment ID to list replies for."},
                    "file_type": {"type": "string", "description": "File type (default: docx).", "default": "docx"},
                    "page_size": {"type": "integer", "description": "Number of replies per page (max 100).", "default": 100},
                    "page_token": {"type": "string", "description": "Pagination token for next page."}
                },
                "required": ["file_token", "comment_id"]
            }))
            .handler(|args, _ctx| async move {
                match run_feishu_doc_action("feishu_drive_list_comment_replies", &args).await {
                    Ok(value) => Ok(value),
                    Err(e) => Ok(json!({"success": false, "error": e})),
                }
            })
            .toolset("feishu_drive")
            .emoji("\u{1f4ac}")
            .check_fn(credentials_availability)
            .build()
            .expect("feishu_drive_list_comment_replies builds"),
    );

    registry.register(
        tool("feishu_drive_reply_comment")
            .description(
                "Reply to a local comment thread on a Feishu document. Use this for local \
                 (quoted-text) comments. For whole-document comments, use \
                 feishu_drive_add_comment instead.",
            )
            .parameters(json!({
                "type": "object",
                "properties": {
                    "file_token": {"type": "string", "description": "The document file token."},
                    "comment_id": {"type": "string", "description": "The comment ID to reply to."},
                    "content": {"type": "string", "description": "The reply text content (plain text only, no markdown)."},
                    "file_type": {"type": "string", "description": "File type (default: docx).", "default": "docx"}
                },
                "required": ["file_token", "comment_id", "content"]
            }))
            .handler(|args, _ctx| async move {
                match run_feishu_doc_action("feishu_drive_reply_comment", &args).await {
                    Ok(value) => Ok(value),
                    Err(e) => Ok(json!({"success": false, "error": e})),
                }
            })
            .toolset("feishu_drive")
            .emoji("\u{2709}\u{fe0f}")
            .check_fn(credentials_availability)
            .build()
            .expect("feishu_drive_reply_comment builds"),
    );

    registry.register(
        tool("feishu_drive_add_comment")
            .description(
                "Add a new whole-document comment on a Feishu document. Use this for \
                 whole-document comments or as a fallback when reply_comment fails with \
                 code 1069302.",
            )
            .parameters(json!({
                "type": "object",
                "properties": {
                    "file_token": {"type": "string", "description": "The document file token."},
                    "content": {"type": "string", "description": "The comment text content (plain text only, no markdown)."},
                    "file_type": {"type": "string", "description": "File type (default: docx).", "default": "docx"}
                },
                "required": ["file_token", "content"]
            }))
            .handler(|args, _ctx| async move {
                match run_feishu_doc_action("feishu_drive_add_comment", &args).await {
                    Ok(value) => Ok(value),
                    Err(e) => Ok(json!({"success": false, "error": e})),
                }
            })
            .toolset("feishu_drive")
            .emoji("\u{2709}\u{fe0f}")
            .check_fn(credentials_availability)
            .build()
            .expect("feishu_drive_add_comment builds"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clear_env() -> (Option<String>, Option<String>, Option<String>) {
        let prev_id = std::env::var("FEISHU_APP_ID").ok();
        let prev_secret = std::env::var("FEISHU_APP_SECRET").ok();
        let prev_home = std::env::var("ULNCLAW_HOME").ok();
        std::env::remove_var("FEISHU_APP_ID");
        std::env::remove_var("FEISHU_APP_SECRET");
        (prev_id, prev_secret, prev_home)
    }

    fn restore_env(prev: (Option<String>, Option<String>, Option<String>)) {
        for (name, value) in [
            ("FEISHU_APP_ID", prev.0),
            ("FEISHU_APP_SECRET", prev.1),
            ("ULNCLAW_HOME", prev.2),
        ] {
            match value {
                Some(v) => std::env::set_var(name, v),
                None => std::env::remove_var(name),
            }
        }
    }

    #[test]
    fn resolve_credentials_none_without_config() {
        let _guard = crate::models_dev::test_env_lock();
        let prev = clear_env();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("ULNCLAW_HOME", tmp.path());
        assert!(resolve_credentials().is_none());
        restore_env(prev);
    }

    #[test]
    fn resolve_credentials_from_env() {
        let _guard = crate::models_dev::test_env_lock();
        let prev = clear_env();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("ULNCLAW_HOME", tmp.path());
        std::env::set_var("FEISHU_APP_ID", "cli_env_id");
        std::env::set_var("FEISHU_APP_SECRET", "env_secret");
        let creds = resolve_credentials().expect("env credentials");
        assert_eq!(creds.app_id, "cli_env_id");
        assert_eq!(creds.app_secret, "env_secret");
        restore_env(prev);
    }

    #[test]
    fn resolve_credentials_from_config_file() {
        let _guard = crate::models_dev::test_env_lock();
        let prev = clear_env();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("config.toml"),
            "[messaging.feishu]\napp_id = \"cli_cfg_id\"\napp_secret = \"cfg_secret\"\n",
        )
        .unwrap();
        std::env::set_var("ULNCLAW_HOME", tmp.path());
        let creds = resolve_credentials().expect("config credentials");
        assert_eq!(creds.app_id, "cli_cfg_id");
        assert_eq!(creds.app_secret, "cfg_secret");
        restore_env(prev);
    }

    #[test]
    fn resolve_credentials_env_overrides_config_file() {
        let _guard = crate::models_dev::test_env_lock();
        let prev = clear_env();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("config.toml"),
            "[messaging.feishu]\napp_id = \"cli_cfg_id\"\napp_secret = \"cfg_secret\"\n",
        )
        .unwrap();
        std::env::set_var("ULNCLAW_HOME", tmp.path());
        std::env::set_var("FEISHU_APP_ID", "cli_env_id");
        let creds = resolve_credentials().expect("credentials");
        assert_eq!(creds.app_id, "cli_env_id");
        assert_eq!(creds.app_secret, "cfg_secret");
        restore_env(prev);
    }

    #[test]
    fn page_size_clamped_to_bounds() {
        assert_eq!(clamp_page_size(&json!({})), 100);
        assert_eq!(clamp_page_size(&json!({"page_size": 0})), 1);
        assert_eq!(clamp_page_size(&json!({"page_size": 50})), 50);
        assert_eq!(clamp_page_size(&json!({"page_size": 500})), 100);
        assert_eq!(clamp_page_size(&json!({"page_size": "25"})), 25);
        assert_eq!(clamp_page_size(&json!({"page_size": "junk"})), 100);
    }

    #[tokio::test]
    async fn run_action_requires_credentials() {
        let _guard = crate::models_dev::test_env_lock();
        let prev = clear_env();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("ULNCLAW_HOME", tmp.path());
        let err = run_feishu_doc_action("feishu_doc_read", &json!({"doc_token": "t"}))
            .await
            .unwrap_err();
        assert!(err.contains("Feishu client not available"), "{err}");
        restore_env(prev);
    }

    #[tokio::test]
    async fn doc_read_requires_doc_token() {
        let _guard = crate::models_dev::test_env_lock();
        let prev = clear_env();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("ULNCLAW_HOME", tmp.path());
        std::env::set_var("FEISHU_APP_ID", "cli_id");
        std::env::set_var("FEISHU_APP_SECRET", "secret");
        let err = run_feishu_doc_action("feishu_doc_read", &json!({}))
            .await
            .unwrap_err();
        assert_eq!(err, "doc_token is required");
        restore_env(prev);
    }

    #[tokio::test]
    async fn drive_tools_validate_required_fields_before_network() {
        let _guard = crate::models_dev::test_env_lock();
        let prev = clear_env();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("ULNCLAW_HOME", tmp.path());
        std::env::set_var("FEISHU_APP_ID", "cli_id");
        std::env::set_var("FEISHU_APP_SECRET", "secret");

        let err = run_feishu_doc_action("feishu_drive_list_comments", &json!({}))
            .await
            .unwrap_err();
        assert_eq!(err, "file_token is required");

        let err = run_feishu_doc_action(
            "feishu_drive_list_comment_replies",
            &json!({"file_token": "ft"}),
        )
        .await
        .unwrap_err();
        assert_eq!(err, "file_token and comment_id are required");

        let err = run_feishu_doc_action(
            "feishu_drive_reply_comment",
            &json!({"file_token": "ft", "comment_id": "c1"}),
        )
        .await
        .unwrap_err();
        assert_eq!(err, "file_token, comment_id, and content are required");

        let err = run_feishu_doc_action("feishu_drive_add_comment", &json!({"file_token": "ft"}))
            .await
            .unwrap_err();
        assert_eq!(err, "file_token and content are required");

        restore_env(prev);
    }

    #[tokio::test]
    async fn unknown_action_rejected() {
        let _guard = crate::models_dev::test_env_lock();
        let prev = clear_env();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("ULNCLAW_HOME", tmp.path());
        std::env::set_var("FEISHU_APP_ID", "cli_id");
        std::env::set_var("FEISHU_APP_SECRET", "secret");
        let err = run_feishu_doc_action("feishu_explode", &json!({}))
            .await
            .unwrap_err();
        assert!(err.contains("Unknown feishu tool"), "{err}");
        restore_env(prev);
    }

    #[test]
    fn register_exposes_five_tools_in_two_toolsets() {
        let mut registry = crate::tools::ToolRegistry::new();
        register(&mut registry);
        assert_eq!(
            registry.get("feishu_doc_read").unwrap().toolset,
            "feishu_doc"
        );
        for name in [
            "feishu_drive_list_comments",
            "feishu_drive_list_comment_replies",
            "feishu_drive_reply_comment",
            "feishu_drive_add_comment",
        ] {
            assert_eq!(
                registry.get(name).unwrap().toolset,
                "feishu_drive",
                "{name}"
            );
        }
    }

    #[test]
    fn success_with_data_merges_object_and_wraps_scalars() {
        let merged = success_with_data(json!({"items": [1], "has_more": false}));
        assert_eq!(merged["success"], true);
        assert_eq!(merged["items"], json!([1]));
        assert_eq!(merged["has_more"], false);
        let wrapped = success_with_data(json!("plain"));
        assert_eq!(wrapped["success"], true);
        assert_eq!(wrapped["data"], json!("plain"));
    }
}

//! Feishu/Lark Drive document-comment agent — port of hermes
//! `plugins/platforms/feishu/feishu_comment.py` +
//! `feishu_comment_rules.py` @ v2026.8.3.
//!
//! Pipeline for `drive.notice.comment_add_v1` events:
//!
//! 1. **Parse + filter** — extract file/comment/reply ids, drop
//!    self-authored events, events not addressed to the bot, and
//!    notice types outside {add_comment, add_reply}.
//! 2. **Access rules** — `<home>/feishu_comment_rules.json`, hermes'
//!    3-tier resolution (exact `docType:token` / `wiki:token` key >
//!    wildcard `*` > top-level > defaults), field-by-field fallback
//!    for enabled/policy/allow_from, mtime-cached hot reload, pairing
//!    approvals in `<home>/feishu_comment_pairing.json`.
//! 3. **OK reaction** on the triggering reply while the agent works
//!    (removed after delivery).
//! 4. **Context assembly** — parallel doc-meta + comment batch_query,
//!    then either the whole-document comment timeline or the local
//!    thread replies, with referenced-doc link extraction + wiki node
//!    resolution and hermes' windowed timeline selection.
//! 5. **Agent run** — the assembled prompt is dispatched through the
//!    normal agent under a per-document session
//!    (`comment-doc:<file_type>:<file_token>`).
//! 6. **Delivery** — reply text chunked at 4000 chars into the comment
//!    thread (`.../comments/<id>/replies`), falling back to a new
//!    whole-document comment on error 1069302.

use crate::messaging::Dispatcher;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

const REACTION_URI: &str = "/open-apis/drive/v2/files/{file_token}/comments/reaction";
const BATCH_QUERY_META_URI: &str = "/open-apis/drive/v1/metas/batch_query";
const BATCH_QUERY_COMMENT_URI: &str = "/open-apis/drive/v1/files/{file_token}/comments/batch_query";
const LIST_COMMENTS_URI: &str = "/open-apis/drive/v1/files/{file_token}/comments";
const LIST_REPLIES_URI: &str =
    "/open-apis/drive/v1/files/{file_token}/comments/{comment_id}/replies";
const ADD_COMMENT_URI: &str = "/open-apis/drive/v1/files/{file_token}/new_comments";
const WIKI_GET_NODE_URI: &str = "/open-apis/wiki/v2/spaces/get_node";
const BOT_INFO_URI: &str = "/open-apis/bot/v3/info";

const COMMENT_RETRY_LIMIT: u32 = 6;
const COMMENT_RETRY_DELAY: Duration = Duration::from_secs(1);
/// hermes `_REPLY_CHUNK_SIZE`.
const REPLY_CHUNK_SIZE: usize = 4000;
/// hermes `_PROMPT_TEXT_LIMIT` / timeline windows.
const PROMPT_TEXT_LIMIT: usize = 220;
const LOCAL_TIMELINE_LIMIT: usize = 20;
const WHOLE_TIMELINE_LIMIT: usize = 12;
/// hermes `_NO_REPLY_SENTINEL` / `_ALLOWED_NOTICE_TYPES`.
const NO_REPLY_SENTINEL: &str = "NO_REPLY";
const ALLOWED_NOTICE_TYPES: [&str; 2] = ["add_comment", "add_reply"];
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// hermes `_COMMON_INSTRUCTIONS`.
const COMMON_INSTRUCTIONS: &str = "This is a Feishu document comment thread, not an IM chat.\nDo NOT call feishu_drive_add_comment or feishu_drive_reply_comment yourself.\nYour reply will be posted automatically. Just output the reply text.\nUse the thread timeline above as the main context.\nIf the quoted content is not enough, use feishu_doc_read to read nearby context.\nThe quoted content is your primary anchor — insert/summarize/explain requests are about it.\nDo not guess document content you haven't read.\nReply in the same language as the user's comment unless they request otherwise.\nUse plain text only. Do not use Markdown, headings, bullet lists, tables, or code blocks.\nDo not show your reasoning process. Do not start with \"I will\", \"Let me\", or \"I'll first\".\nOutput only the final user-facing reply.\nIf no reply is needed, output exactly NO_REPLY.";

// ---------------------------------------------------------------------------
// Access rules (hermes feishu_comment_rules.py)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct CommentDocumentRule {
    pub enabled: Option<bool>,
    pub policy: Option<String>,
    pub allow_from: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
pub struct CommentsConfig {
    pub enabled: bool,
    pub policy: String,
    pub allow_from: Vec<String>,
    pub documents: HashMap<String, CommentDocumentRule>,
}

impl Default for CommentsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            policy: "pairing".into(),
            allow_from: Vec::new(),
            documents: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedCommentRule {
    pub enabled: bool,
    pub policy: String,
    pub allow_from: Vec<String>,
    pub match_source: String,
}

fn rules_file() -> std::path::PathBuf {
    crate::config::ulnclaw_home().join("feishu_comment_rules.json")
}

fn pairing_file() -> std::path::PathBuf {
    crate::config::ulnclaw_home().join("feishu_comment_pairing.json")
}

fn read_json_file(path: &std::path::Path) -> Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or(Value::Null)
}

fn parse_string_list(raw: &Value) -> Option<Vec<String>> {
    raw.as_array().map(|arr| {
        arr.iter()
            .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
            .filter(|s| !s.is_empty())
            .collect()
    })
}

fn parse_document_rule(raw: &Value) -> CommentDocumentRule {
    let policy = raw
        .get("policy")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_lowercase())
        .filter(|p| p == "allowlist" || p == "pairing");
    CommentDocumentRule {
        enabled: raw.get("enabled").and_then(|v| v.as_bool()),
        policy,
        allow_from: raw.get("allow_from").and_then(parse_string_list),
    }
}

/// hermes `load_config` — `<home>/feishu_comment_rules.json` (re-read
/// on every event, hermes mtime-caches; the file is tiny).
pub fn load_comments_config() -> CommentsConfig {
    let raw = read_json_file(&rules_file());
    let Some(raw) = raw.as_object() else {
        return CommentsConfig::default();
    };
    let mut documents = HashMap::new();
    if let Some(docs) = raw.get("documents").and_then(|v| v.as_object()) {
        for (key, rule) in docs {
            documents.insert(key.clone(), parse_document_rule(rule));
        }
    }
    let policy = raw
        .get("policy")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_lowercase())
        .filter(|p| p == "allowlist" || p == "pairing")
        .unwrap_or_else(|| "pairing".into());
    CommentsConfig {
        enabled: raw.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true),
        policy,
        allow_from: raw
            .get("allow_from")
            .and_then(parse_string_list)
            .unwrap_or_default(),
        documents,
    }
}

/// hermes `has_wiki_keys`.
pub fn has_wiki_keys(cfg: &CommentsConfig) -> bool {
    cfg.documents.keys().any(|k| k.starts_with("wiki:"))
}

/// hermes `resolve_rule` — exact doc > wiki key > wildcard `*` >
/// top-level > code defaults, field-by-field fallback.
pub fn resolve_rule(
    cfg: &CommentsConfig,
    file_type: &str,
    file_token: &str,
    wiki_token: &str,
) -> ResolvedCommentRule {
    let exact_key = format!("{file_type}:{file_token}");
    let mut layers: Vec<(&CommentDocumentRule, String)> = Vec::new();
    if let Some(rule) = cfg.documents.get(&exact_key) {
        layers.push((rule, format!("exact:{exact_key}")));
    } else if !wiki_token.is_empty() {
        let wiki_key = format!("wiki:{wiki_token}");
        if let Some(rule) = cfg.documents.get(&wiki_key) {
            layers.push((rule, format!("exact:{wiki_key}")));
        }
    }
    if let Some(rule) = cfg.documents.get("*") {
        layers.push((rule, "wildcard".into()));
    }

    let mut enabled = cfg.enabled;
    let mut enabled_src = "top";
    let mut policy = cfg.policy.clone();
    let mut policy_src = "top";
    let mut allow_from = cfg.allow_from.clone();
    for (layer, _source) in &layers {
        if layer.enabled.is_some() && enabled_src == "top" {
            enabled = layer.enabled.unwrap();
            enabled_src = "exact";
        }
        if layer.policy.is_some() && policy_src == "top" {
            policy = layer.policy.clone().unwrap();
            policy_src = "exact";
        }
        if layer.allow_from.is_some() {
            allow_from = layer.allow_from.clone().unwrap_or_default();
        }
    }
    let match_source = if enabled_src != "top" {
        enabled_src.to_string()
    } else if policy_src != "top" {
        policy_src.to_string()
    } else {
        "top".to_string()
    };
    ResolvedCommentRule {
        enabled,
        policy,
        allow_from,
        match_source,
    }
}

fn load_pairing_approved() -> HashSet<String> {
    let raw = read_json_file(&pairing_file());
    let mut out = HashSet::new();
    match raw.get("approved") {
        Some(Value::Object(map)) => {
            for key in map.keys() {
                out.insert(key.clone());
            }
        }
        Some(Value::Array(arr)) => {
            for v in arr {
                if let Some(s) = v.as_str() {
                    if !s.is_empty() {
                        out.insert(s.to_string());
                    }
                }
            }
        }
        _ => {}
    }
    out
}

/// hermes `is_user_allowed`.
pub fn is_user_allowed(rule: &ResolvedCommentRule, user_open_id: &str) -> bool {
    if rule
        .allow_from
        .iter()
        .any(|u| u == user_open_id || u == "*")
    {
        return true;
    }
    if rule.policy == "pairing" {
        return load_pairing_approved().contains(user_open_id);
    }
    false
}

// ---------------------------------------------------------------------------
// Event parsing (hermes parse_drive_comment_event)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct DriveCommentEvent {
    pub event_id: String,
    pub comment_id: String,
    pub reply_id: String,
    pub is_mentioned: bool,
    pub file_token: String,
    pub file_type: String,
    pub notice_type: String,
    pub from_open_id: String,
    pub to_open_id: String,
}

pub fn parse_drive_comment_event(envelope: &Value) -> Option<DriveCommentEvent> {
    let event = envelope.get("event")?;
    let notice_meta = event.get("notice_meta").cloned().unwrap_or(json!({}));
    let from_user = notice_meta
        .get("from_user_id")
        .cloned()
        .unwrap_or(json!({}));
    let to_user = notice_meta.get("to_user_id").cloned().unwrap_or(json!({}));
    Some(DriveCommentEvent {
        event_id: event
            .get("event_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        comment_id: event
            .get("comment_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        reply_id: event
            .get("reply_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        is_mentioned: event
            .get("is_mentioned")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        file_token: notice_meta
            .get("file_token")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        file_type: notice_meta
            .get("file_type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        notice_type: notice_meta
            .get("notice_type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        from_open_id: from_user
            .get("open_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        to_open_id: to_user
            .get("open_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    })
}

// ---------------------------------------------------------------------------
// Open API helpers (tenant-token bearer calls)
// ---------------------------------------------------------------------------

struct CommentApi {
    client: reqwest::Client,
    api: Arc<crate::feishu::FeishuApi>,
}

impl CommentApi {
    fn new(cfg: &crate::feishu::FeishuConfig) -> Self {
        Self {
            client: reqwest::Client::new(),
            api: crate::feishu::feishu_api(cfg),
        }
    }

    async fn call(
        &self,
        method: reqwest::Method,
        uri: &str,
        paths: &[(&str, &str)],
        queries: &[(&str, &str)],
        body: Option<&Value>,
    ) -> (i64, String, Value) {
        let token = match self.api.api_token().await {
            Ok(t) => t,
            Err(e) => return (-1, e, json!({})),
        };
        let mut url = uri.to_string();
        for (key, value) in paths {
            url = url.replace(&format!("{{{key}}}"), value);
        }
        let mut request = self
            .client
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

    /// hermes `add_comment_reaction` / `delete_comment_reaction`.
    async fn comment_reaction(
        &self,
        file_token: &str,
        file_type: &str,
        reply_id: &str,
        action: &str,
    ) -> bool {
        let body = json!({
            "action": action,
            "reply_id": reply_id,
            "reaction_type": "OK",
        });
        let (code, msg, _) = self
            .call(
                reqwest::Method::POST,
                REACTION_URI,
                &[("file_token", file_token)],
                &[("file_type", file_type)],
                Some(&body),
            )
            .await;
        if code != 0 {
            eprintln!("[feishu-comment] reaction {action} failed: code={code} msg={msg}");
        }
        code == 0
    }

    /// hermes `query_document_meta`.
    async fn query_document_meta(&self, file_token: &str, file_type: &str) -> Value {
        let body = json!({
            "request_docs": [{"doc_token": file_token, "doc_type": file_type}],
            "with_url": true,
        });
        let (code, msg, data) = self
            .call(
                reqwest::Method::POST,
                BATCH_QUERY_META_URI,
                &[],
                &[],
                Some(&body),
            )
            .await;
        if code != 0 {
            eprintln!("[feishu-comment] meta batch_query failed: code={code} msg={msg}");
            return json!({});
        }
        let meta = data
            .get("metas")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first().cloned())
            .unwrap_or(json!({}));
        json!({
            "title": meta.get("title").and_then(|v| v.as_str()).unwrap_or(""),
            "url": meta.get("url").and_then(|v| v.as_str()).unwrap_or(""),
        })
    }

    /// hermes `batch_query_comment` (6 retries — eventual consistency).
    async fn batch_query_comment(
        &self,
        file_token: &str,
        file_type: &str,
        comment_id: &str,
    ) -> Value {
        for attempt in 0..COMMENT_RETRY_LIMIT {
            let body = json!({ "comment_ids": [comment_id] });
            let (code, msg, data) = self
                .call(
                    reqwest::Method::POST,
                    BATCH_QUERY_COMMENT_URI,
                    &[("file_token", file_token)],
                    &[("file_type", file_type), ("user_id_type", "open_id")],
                    Some(&body),
                )
                .await;
            if code == 0 {
                return data
                    .get("items")
                    .and_then(|v| v.as_array())
                    .and_then(|arr| arr.first().cloned())
                    .unwrap_or(json!({}));
            }
            if attempt < COMMENT_RETRY_LIMIT - 1 {
                tokio::time::sleep(COMMENT_RETRY_DELAY).await;
            } else {
                eprintln!(
                    "[feishu-comment] batch_query_comment failed after {COMMENT_RETRY_LIMIT} attempts: code={code} msg={msg}"
                );
            }
        }
        json!({})
    }

    async fn list_comments_page(
        &self,
        file_token: &str,
        file_type: &str,
        is_whole: bool,
        comment_id: &str,
        page_token: &str,
    ) -> (i64, Vec<Value>, bool, String) {
        let uri = if comment_id.is_empty() {
            LIST_COMMENTS_URI.to_string()
        } else {
            LIST_REPLIES_URI.replace("{comment_id}", comment_id)
        };
        let mut queries: Vec<(&str, &str)> = vec![
            ("file_type", file_type),
            ("page_size", "100"),
            ("user_id_type", "open_id"),
        ];
        let whole_flag;
        if is_whole {
            whole_flag = "true".to_string();
            queries.push(("is_whole", &whole_flag));
        }
        if !page_token.is_empty() {
            queries.push(("page_token", page_token));
        }
        let (code, msg, data) = self
            .call(
                reqwest::Method::GET,
                &uri,
                &[("file_token", file_token)],
                &queries,
                None,
            )
            .await;
        if code != 0 {
            eprintln!("[feishu-comment] list comments failed: code={code} msg={msg}");
            return (code, Vec::new(), false, String::new());
        }
        let items = data
            .get("items")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let has_more = data
            .get("has_more")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let next = data
            .get("page_token")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        (0, items, has_more, next)
    }

    /// hermes `list_whole_comments` (paginated, max 5 pages).
    async fn list_whole_comments(&self, file_token: &str, file_type: &str) -> Vec<Value> {
        let mut all = Vec::new();
        let mut page_token = String::new();
        for _ in 0..5 {
            let (code, items, has_more, next) = self
                .list_comments_page(file_token, file_type, true, "", &page_token)
                .await;
            if code != 0 {
                break;
            }
            all.extend(items);
            if !has_more || next.is_empty() {
                break;
            }
            page_token = next;
        }
        all
    }

    /// hermes `list_comment_replies` (paginated; retries until the
    /// expected reply_id shows up — eventual consistency).
    async fn list_comment_replies(
        &self,
        file_token: &str,
        file_type: &str,
        comment_id: &str,
        expect_reply_id: &str,
    ) -> Vec<Value> {
        let mut all = Vec::new();
        for attempt in 0..COMMENT_RETRY_LIMIT {
            let mut replies = Vec::new();
            let mut page_token = String::new();
            let mut fetch_ok = true;
            for _ in 0..5 {
                let (code, items, has_more, next) = self
                    .list_comments_page(file_token, file_type, false, comment_id, &page_token)
                    .await;
                if code != 0 {
                    fetch_ok = false;
                    break;
                }
                replies.extend(items);
                if !has_more || next.is_empty() {
                    break;
                }
                page_token = next;
            }
            all = replies;
            if expect_reply_id.is_empty()
                || !fetch_ok
                || all
                    .iter()
                    .any(|r| r.get("reply_id").and_then(|v| v.as_str()) == Some(expect_reply_id))
            {
                break;
            }
            if attempt < COMMENT_RETRY_LIMIT - 1 {
                tokio::time::sleep(COMMENT_RETRY_DELAY).await;
            }
        }
        all
    }

    /// hermes `_reverse_lookup_wiki_token` / `_resolve_wiki_nodes`.
    async fn wiki_get_node(&self, token: &str, obj_type: &str) -> Value {
        let mut queries: Vec<(&str, &str)> = vec![("token", token)];
        if !obj_type.is_empty() {
            queries.push(("obj_type", obj_type));
        }
        let (code, _, data) = self
            .call(reqwest::Method::GET, WIKI_GET_NODE_URI, &[], &queries, None)
            .await;
        if code != 0 {
            return json!({});
        }
        data.get("node").cloned().unwrap_or(json!({}))
    }

    /// hermes `reply_to_comment` — returns (ok, code).
    async fn reply_to_comment(
        &self,
        file_token: &str,
        file_type: &str,
        comment_id: &str,
        text: &str,
    ) -> (bool, i64) {
        let body = json!({
            "content": {
                "elements": [{
                    "type": "text_run",
                    "text_run": { "text": sanitize_comment_text(text) },
                }],
            },
        });
        let (code, msg, _) = self
            .call(
                reqwest::Method::POST,
                LIST_REPLIES_URI,
                &[("file_token", file_token), ("comment_id", comment_id)],
                &[("file_type", file_type)],
                Some(&body),
            )
            .await;
        if code != 0 {
            eprintln!("[feishu-comment] reply_to_comment failed: code={code} msg={msg}");
        }
        (code == 0, code)
    }

    /// hermes `add_whole_comment`.
    async fn add_whole_comment(&self, file_token: &str, file_type: &str, text: &str) -> bool {
        let body = json!({
            "file_type": file_type,
            "reply_elements": [{ "type": "text", "text": sanitize_comment_text(text) }],
        });
        let (code, msg, _) = self
            .call(
                reqwest::Method::POST,
                ADD_COMMENT_URI,
                &[("file_token", file_token)],
                &[],
                Some(&body),
            )
            .await;
        if code != 0 {
            eprintln!("[feishu-comment] add_whole_comment failed: code={code} msg={msg}");
        }
        code == 0
    }

    async fn bot_open_id(&self) -> String {
        static CACHED: std::sync::OnceLock<tokio::sync::Mutex<Option<String>>> =
            std::sync::OnceLock::new();
        let slot = CACHED.get_or_init(|| tokio::sync::Mutex::new(None));
        if let Some(cached) = slot.lock().await.clone() {
            return cached;
        }
        let (code, _, data) = self
            .call(reqwest::Method::GET, BOT_INFO_URI, &[], &[], None)
            .await;
        let open_id = if code == 0 {
            data.pointer("/bot/open_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        } else {
            String::new()
        };
        if !open_id.is_empty() {
            *slot.lock().await = Some(open_id.clone());
        }
        open_id
    }
}

/// hermes `_sanitize_comment_text`.
pub fn sanitize_comment_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// hermes `deliver_comment_reply` — chunked, whole-comment fallback on
/// error 1069302.
async fn deliver_comment_reply(
    api: &CommentApi,
    file_token: &str,
    file_type: &str,
    comment_id: &str,
    text: &str,
    mut is_whole: bool,
) -> bool {
    let chunks = chunk_text(text, REPLY_CHUNK_SIZE);
    let mut all_ok = true;
    for chunk in chunks {
        let ok = if is_whole {
            api.add_whole_comment(file_token, file_type, &chunk).await
        } else {
            let (success, code) = api
                .reply_to_comment(file_token, file_type, comment_id, &chunk)
                .await;
            if success {
                true
            } else if code == 1069302 {
                eprintln!(
                    "[feishu-comment] reply not allowed (1069302), falling back to whole comment"
                );
                is_whole = true;
                api.add_whole_comment(file_token, file_type, &chunk).await
            } else {
                false
            }
        };
        if !ok {
            all_ok = false;
            break;
        }
    }
    all_ok
}

/// hermes `_chunk_text` — line-break preferring chunks.
pub fn chunk_text(text: &str, limit: usize) -> Vec<String> {
    if text.len() <= limit {
        return vec![text.to_string()];
    }
    let mut chunks = Vec::new();
    let mut remaining = text.to_string();
    while !remaining.is_empty() {
        if remaining.len() <= limit {
            chunks.push(remaining);
            break;
        }
        let window = &remaining[..limit];
        let cut = window.rfind('\n').filter(|c| *c > 0).unwrap_or(limit);
        let head: String = remaining[..cut].to_string();
        chunks.push(head);
        remaining = remaining[cut..].trim_start_matches('\n').to_string();
    }
    chunks
}

// ---------------------------------------------------------------------------
// Content extraction (hermes helpers)
// ---------------------------------------------------------------------------

fn reply_content(reply: &Value) -> Value {
    match reply.get("content") {
        Some(Value::String(s)) => serde_json::from_str(s).unwrap_or(json!({})),
        Some(v) => v.clone(),
        None => json!({}),
    }
}

/// hermes `_extract_reply_text`.
pub fn extract_reply_text(reply: &Value) -> String {
    let content = reply_content(reply);
    let mut parts = Vec::new();
    for elem in content
        .get("elements")
        .and_then(|v| v.as_array())
        .unwrap_or(&vec![])
    {
        match elem.get("type").and_then(|v| v.as_str()).unwrap_or("") {
            "text_run" => {
                if let Some(text) = elem.pointer("/text_run/text").and_then(|v| v.as_str()) {
                    parts.push(text.to_string());
                }
            }
            "docs_link" => {
                if let Some(url) = elem.pointer("/docs_link/url").and_then(|v| v.as_str()) {
                    parts.push(url.to_string());
                }
            }
            "person" => {
                let uid = elem
                    .pointer("/person/user_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                parts.push(format!("@{uid}"));
            }
            _ => {}
        }
    }
    parts.join("")
}

/// hermes `_get_reply_user_id`.
pub fn get_reply_user_id(reply: &Value) -> String {
    match reply.get("user_id") {
        Some(Value::Object(_)) => reply
            .pointer("/user_id/open_id")
            .and_then(|v| v.as_str())
            .or_else(|| reply.pointer("/user_id/user_id").and_then(|v| v.as_str()))
            .unwrap_or("")
            .to_string(),
        Some(v) => v.as_str().unwrap_or("").to_string(),
        None => String::new(),
    }
}

/// hermes `_extract_semantic_text` — strips self @mentions, collapses
/// whitespace.
pub fn extract_semantic_text(reply: &Value, self_open_id: &str) -> String {
    let content = reply_content(reply);
    let mut parts = Vec::new();
    for elem in content
        .get("elements")
        .and_then(|v| v.as_array())
        .unwrap_or(&vec![])
    {
        match elem.get("type").and_then(|v| v.as_str()).unwrap_or("") {
            "person" => {
                let uid = elem
                    .pointer("/person/user_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if !self_open_id.is_empty() && uid == self_open_id {
                    continue;
                }
                parts.push(format!("@{uid}"));
            }
            "text_run" => {
                if let Some(text) = elem.pointer("/text_run/text").and_then(|v| v.as_str()) {
                    parts.push(text.to_string());
                }
            }
            "docs_link" => {
                if let Some(url) = elem.pointer("/docs_link/url").and_then(|v| v.as_str()) {
                    parts.push(url.to_string());
                }
            }
            _ => {}
        }
    }
    parts
        .join("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// hermes `_FEISHU_DOC_URL_RE`.
fn doc_url_parts(url: &str) -> Option<(String, String)> {
    let re = regex::Regex::new(
        r"(?:feishu\.cn|larkoffice\.com|larksuite\.com|lark\.suite\.com)/(wiki|doc|docx|sheet|sheets|slides|mindnote|bitable|base|file)/([A-Za-z0-9_-]{10,40})",
    )
    .unwrap();
    re.captures(url).map(|caps| {
        (
            caps.get(1).unwrap().as_str().to_string(),
            caps.get(2).unwrap().as_str().to_string(),
        )
    })
}

/// hermes `_extract_docs_links`.
pub fn extract_docs_links(replies: &[Value]) -> Vec<Value> {
    let mut seen = HashSet::new();
    let mut links = Vec::new();
    for reply in replies {
        let content = reply_content(reply);
        for elem in content
            .get("elements")
            .and_then(|v| v.as_array())
            .unwrap_or(&vec![])
        {
            let elem_type = elem.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if elem_type != "docs_link" && elem_type != "link" {
                continue;
            }
            let url = elem
                .pointer("/docs_link/url")
                .or_else(|| elem.pointer("/link/url"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if url.is_empty() {
                continue;
            }
            if let Some((doc_type, token)) = doc_url_parts(url) {
                if seen.insert(token.clone()) {
                    links.push(json!({ "url": url, "doc_type": doc_type, "token": token }));
                }
            }
        }
    }
    links
}

/// hermes `_format_referenced_docs`.
pub fn format_referenced_docs(links: &[Value], current_file_token: &str) -> String {
    if links.is_empty() {
        return String::new();
    }
    let mut lines = vec![
        "".to_string(),
        "Referenced documents in comments:".to_string(),
    ];
    for link in links {
        let rtype = link
            .get("resolved_type")
            .and_then(|v| v.as_str())
            .or_else(|| link.get("doc_type").and_then(|v| v.as_str()))
            .unwrap_or("");
        let rtoken = link
            .get("resolved_token")
            .and_then(|v| v.as_str())
            .or_else(|| link.get("token").and_then(|v| v.as_str()))
            .unwrap_or("");
        let url = link.get("url").and_then(|v| v.as_str()).unwrap_or("");
        let suffix = if rtoken == current_file_token {
            " (same as current document)"
        } else {
            ""
        };
        let short_url: String = url.chars().take(80).collect();
        lines.push(format!("- {rtype}:{rtoken}{suffix} ({short_url})"));
    }
    lines.join("\n")
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        text.to_string()
    } else {
        text.chars().take(limit).collect::<String>() + "..."
    }
}

type TimelineEntry = (String, String, bool);

/// hermes `_select_local_timeline` — window around target, always keep
/// first/target/last.
pub fn select_local_timeline(
    timeline: &[TimelineEntry],
    target_index: isize,
) -> Vec<TimelineEntry> {
    let n = timeline.len();
    if n <= LOCAL_TIMELINE_LIMIT {
        return timeline.to_vec();
    }
    let mut selected = std::collections::BTreeSet::new();
    selected.insert(0);
    selected.insert(n - 1);
    if target_index >= 0 && (target_index as usize) < n {
        selected.insert(target_index as usize);
    }
    let mut budget = LOCAL_TIMELINE_LIMIT.saturating_sub(selected.len());
    let target = target_index.max(0) as usize;
    let (mut lo, mut hi) = (target.saturating_sub(1) as isize, target + 1);
    while budget > 0 && (lo >= 0 || (hi as usize) < n) {
        if lo >= 0 && !selected.contains(&(lo as usize)) {
            selected.insert(lo as usize);
            budget -= 1;
        }
        lo -= 1;
        if budget > 0 && (hi as usize) < n && !selected.contains(&(hi as usize)) {
            selected.insert(hi as usize);
            budget -= 1;
        }
        hi += 1;
    }
    selected.iter().map(|i| timeline[*i].clone()).collect()
}

/// hermes `_select_whole_timeline` — prioritize current + nearest self.
pub fn select_whole_timeline(
    timeline: &[TimelineEntry],
    current_index: isize,
    nearest_self_index: isize,
) -> Vec<TimelineEntry> {
    let n = timeline.len();
    if n <= WHOLE_TIMELINE_LIMIT {
        return timeline.to_vec();
    }
    let mut selected = std::collections::BTreeSet::new();
    if current_index >= 0 && (current_index as usize) < n {
        selected.insert(current_index as usize);
    }
    if nearest_self_index >= 0 && (nearest_self_index as usize) < n {
        selected.insert(nearest_self_index as usize);
    }
    let mut budget = WHOLE_TIMELINE_LIMIT.saturating_sub(selected.len());
    let current = current_index.max(0) as usize;
    let (mut lo, mut hi) = (current.saturating_sub(1) as isize, current + 1);
    while budget > 0 && (lo >= 0 || (hi as usize) < n) {
        if lo >= 0 && !selected.contains(&(lo as usize)) {
            selected.insert(lo as usize);
            budget -= 1;
        }
        lo -= 1;
        if budget > 0 && (hi as usize) < n && !selected.contains(&(hi as usize)) {
            selected.insert(hi as usize);
            budget -= 1;
        }
        hi += 1;
    }
    if selected.is_empty() {
        return timeline[n - WHOLE_TIMELINE_LIMIT..].to_vec();
    }
    selected.iter().map(|i| timeline[*i].clone()).collect()
}

// ---------------------------------------------------------------------------
// Prompt builders (hermes build_*_comment_prompt)
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub fn build_local_comment_prompt(
    doc_title: &str,
    doc_url: &str,
    file_token: &str,
    file_type: &str,
    comment_id: &str,
    quote_text: &str,
    root_comment_text: &str,
    target_reply_text: &str,
    timeline: &[TimelineEntry],
    target_index: isize,
    referenced_docs: &str,
) -> String {
    let selected = select_local_timeline(timeline, target_index);
    let mut lines = vec![
        format!("The user added a reply in \"{doc_title}\"."),
        format!(
            "Current user comment text: \"{}\"",
            truncate(target_reply_text, PROMPT_TEXT_LIMIT)
        ),
        format!(
            "Original comment text: \"{}\"",
            truncate(root_comment_text, PROMPT_TEXT_LIMIT)
        ),
        format!("Quoted content: \"{}\"", truncate(quote_text, 500)),
        "This comment mentioned you (@mention is for routing, not task content).".to_string(),
        format!("Document link: {doc_url}"),
        "Current commented document:".to_string(),
        format!("- file_type={file_type}"),
        format!("- file_token={file_token}"),
        format!("- comment_id={comment_id}"),
        String::new(),
        format!(
            "Current comment card timeline ({}/{} entries):",
            selected.len(),
            timeline.len()
        ),
    ];
    for (user_id, text, is_self) in &selected {
        let marker = if *is_self { " <-- YOU" } else { "" };
        lines.push(format!(
            "[{user_id}] {}{marker}",
            truncate(text, PROMPT_TEXT_LIMIT)
        ));
    }
    if !referenced_docs.is_empty() {
        lines.push(referenced_docs.to_string());
    }
    lines.push(String::new());
    lines.push(COMMON_INSTRUCTIONS.to_string());
    lines.join("\n")
}

#[allow(clippy::too_many_arguments)]
pub fn build_whole_comment_prompt(
    doc_title: &str,
    doc_url: &str,
    file_token: &str,
    file_type: &str,
    comment_text: &str,
    timeline: &[TimelineEntry],
    current_index: isize,
    nearest_self_index: isize,
    referenced_docs: &str,
) -> String {
    let selected = select_whole_timeline(timeline, current_index, nearest_self_index);
    let mut lines = vec![
        format!("The user added a comment in \"{doc_title}\"."),
        format!(
            "Current user comment text: \"{}\"",
            truncate(comment_text, PROMPT_TEXT_LIMIT)
        ),
        "This is a whole-document comment.".to_string(),
        "This comment mentioned you (@mention is for routing, not task content).".to_string(),
        format!("Document link: {doc_url}"),
        "Current commented document:".to_string(),
        format!("- file_type={file_type}"),
        format!("- file_token={file_token}"),
        String::new(),
        format!(
            "Whole-document comment timeline ({}/{} entries):",
            selected.len(),
            timeline.len()
        ),
    ];
    for (user_id, text, is_self) in &selected {
        let marker = if *is_self { " <-- YOU" } else { "" };
        lines.push(format!(
            "[{user_id}] {}{marker}",
            truncate(text, PROMPT_TEXT_LIMIT)
        ));
    }
    if !referenced_docs.is_empty() {
        lines.push(referenced_docs.to_string());
    }
    lines.push(String::new());
    lines.push(COMMON_INSTRUCTIONS.to_string());
    lines.join("\n")
}

// ---------------------------------------------------------------------------
// Orchestration (hermes handle_drive_comment_event)
// ---------------------------------------------------------------------------

/// Full drive-comment pipeline; spawned by the webhook/WS dispatch for
/// `drive.notice.comment_add_v1`.
pub async fn handle_drive_comment_event(
    cfg: &crate::feishu::FeishuConfig,
    dispatcher: &Arc<Dispatcher>,
    envelope: &Value,
) {
    let Some(parsed) = parse_drive_comment_event(envelope) else {
        eprintln!("[feishu-comment] dropping malformed drive comment event");
        return;
    };
    let api = CommentApi::new(cfg);
    let self_open_id = api.bot_open_id().await;

    // Filters: self-reply, receiver check, notice_type.
    if !parsed.from_open_id.is_empty()
        && !self_open_id.is_empty()
        && parsed.from_open_id == self_open_id
    {
        return;
    }
    if parsed.to_open_id.is_empty()
        || (!self_open_id.is_empty() && parsed.to_open_id != self_open_id)
    {
        return;
    }
    if !parsed.notice_type.is_empty()
        && !ALLOWED_NOTICE_TYPES.contains(&parsed.notice_type.as_str())
    {
        return;
    }
    if parsed.file_token.is_empty() || parsed.file_type.is_empty() || parsed.comment_id.is_empty() {
        eprintln!("[feishu-comment] missing required fields, skipping");
        return;
    }

    // Access rules (hermes feishu_comment_rules).
    let comments_cfg = load_comments_config();
    let mut rule = resolve_rule(&comments_cfg, &parsed.file_type, &parsed.file_token, "");
    if matches!(rule.match_source.as_str(), "wildcard" | "top") && has_wiki_keys(&comments_cfg) {
        let node = api
            .wiki_get_node(&parsed.file_token, &parsed.file_type)
            .await;
        if let Some(wiki_token) = node.get("node_token").and_then(|v| v.as_str()) {
            if !wiki_token.is_empty() {
                rule = resolve_rule(
                    &comments_cfg,
                    &parsed.file_type,
                    &parsed.file_token,
                    wiki_token,
                );
            }
        }
    }
    if !rule.enabled {
        eprintln!(
            "[feishu-comment] comments disabled for {}:{}, skipping",
            parsed.file_type, parsed.file_token
        );
        return;
    }
    if !is_user_allowed(&rule, &parsed.from_open_id) {
        eprintln!(
            "[feishu-comment] user {} denied (policy={}, rule={})",
            parsed.from_open_id, rule.policy, rule.match_source
        );
        return;
    }

    // OK reaction while the agent works.
    if !parsed.reply_id.is_empty() {
        api.comment_reaction(
            &parsed.file_token,
            &parsed.file_type,
            &parsed.reply_id,
            "add",
        )
        .await;
    }

    // Parallel fetch: doc meta + comment detail.
    let meta_fut = api.query_document_meta(&parsed.file_token, &parsed.file_type);
    let comment_fut =
        api.batch_query_comment(&parsed.file_token, &parsed.file_type, &parsed.comment_id);
    let (doc_meta, comment_detail) = tokio::join!(meta_fut, comment_fut);
    let doc_title = doc_meta
        .get("title")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("Untitled")
        .to_string();
    let doc_url = doc_meta
        .get("url")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let is_whole = comment_detail
        .get("is_whole")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let prompt = if is_whole {
        let whole_comments = api
            .list_whole_comments(&parsed.file_token, &parsed.file_type)
            .await;
        let mut timeline: Vec<TimelineEntry> = Vec::new();
        let mut current_text = String::new();
        let mut current_index: isize = -1;
        let mut nearest_self_index: isize = -1;
        for wc in &whole_comments {
            let reply_list = match wc.get("reply_list") {
                Some(Value::String(s)) => serde_json::from_str(s).unwrap_or(json!({})),
                Some(v) => v.clone(),
                None => json!({}),
            };
            for r in reply_list
                .get("replies")
                .and_then(|v| v.as_array())
                .unwrap_or(&vec![])
            {
                let uid = get_reply_user_id(r);
                let text = extract_reply_text(r);
                let is_self = !self_open_id.is_empty() && uid == self_open_id;
                let idx = timeline.len() as isize;
                timeline.push((uid.clone(), text, is_self));
                if uid == parsed.from_open_id {
                    current_text = extract_semantic_text(r, &self_open_id);
                    current_index = idx;
                }
                if is_self {
                    nearest_self_index = idx;
                }
            }
        }
        if current_text.is_empty() {
            for (i, (uid, text, is_self)) in timeline.iter().enumerate().rev() {
                if !is_self {
                    current_text = text.clone();
                    current_index = i as isize;
                    let _ = uid;
                    break;
                }
            }
        }
        let mut all_replies: Vec<Value> = Vec::new();
        for wc in &whole_comments {
            let reply_list = match wc.get("reply_list") {
                Some(Value::String(s)) => serde_json::from_str(s).unwrap_or(json!({})),
                Some(v) => v.clone(),
                None => json!({}),
            };
            if let Some(replies) = reply_list.get("replies").and_then(|v| v.as_array()) {
                all_replies.extend(replies.iter().cloned());
            }
        }
        let mut doc_links = extract_docs_links(&all_replies);
        resolve_wiki_links(&api, &mut doc_links).await;
        let ref_docs = format_referenced_docs(&doc_links, &parsed.file_token);
        build_whole_comment_prompt(
            &doc_title,
            &doc_url,
            &parsed.file_token,
            &parsed.file_type,
            &current_text,
            &timeline,
            current_index,
            nearest_self_index,
            &ref_docs,
        )
    } else {
        let replies = api
            .list_comment_replies(
                &parsed.file_token,
                &parsed.file_type,
                &parsed.comment_id,
                &parsed.reply_id,
            )
            .await;
        let quote_text = comment_detail
            .get("quote")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let mut timeline: Vec<TimelineEntry> = Vec::new();
        let mut root_text = String::new();
        let mut target_text = String::new();
        let mut target_index: isize = -1;
        for (i, r) in replies.iter().enumerate() {
            let uid = get_reply_user_id(r);
            let text = extract_reply_text(r);
            let is_self = !self_open_id.is_empty() && uid == self_open_id;
            timeline.push((uid.clone(), text, is_self));
            if i == 0 {
                root_text = extract_semantic_text(r, &self_open_id);
            }
            let rid = r.get("reply_id").and_then(|v| v.as_str()).unwrap_or("");
            if !rid.is_empty() && rid == parsed.reply_id {
                target_text = extract_semantic_text(r, &self_open_id);
                target_index = i as isize;
            }
        }
        if target_text.is_empty() {
            for (i, (uid, text, _)) in timeline.iter().enumerate().rev() {
                if *uid == parsed.from_open_id {
                    target_text = text.clone();
                    target_index = i as isize;
                    break;
                }
            }
        }
        let mut doc_links = extract_docs_links(&replies);
        resolve_wiki_links(&api, &mut doc_links).await;
        let ref_docs = format_referenced_docs(&doc_links, &parsed.file_token);
        build_local_comment_prompt(
            &doc_title,
            &doc_url,
            &parsed.file_token,
            &parsed.file_type,
            &parsed.comment_id,
            &quote_text,
            &root_text,
            &target_text,
            &timeline,
            target_index,
            &ref_docs,
        )
    };

    // Agent run — per-document session (hermes `_session_key`).
    let chat_id = format!("comment-doc:{}:{}", parsed.file_type, parsed.file_token);
    let event = crate::messaging::MessageEvent {
        platform: "feishu".into(),
        chat_id,
        sender_name: parsed.from_open_id.clone(),
        sender_id: parsed.from_open_id.clone(),
        text: prompt,
        message_id: parsed.event_id.clone(),
        attachments: Vec::new(),
    };
    let response = match dispatcher.handle_event(event).await {
        Ok(outcome) => outcome.reply.trim().to_string(),
        Err(e) => {
            eprintln!("[feishu-comment] agent failed: {e}");
            String::new()
        }
    };

    if response.is_empty() || response.contains(NO_REPLY_SENTINEL) {
        eprintln!("[feishu-comment] agent returned NO_REPLY, skipping delivery");
    } else {
        let ok = deliver_comment_reply(
            &api,
            &parsed.file_token,
            &parsed.file_type,
            &parsed.comment_id,
            &response,
            is_whole,
        )
        .await;
        if ok {
            eprintln!(
                "[feishu-comment] reply delivered ({} chars)",
                response.len()
            );
        } else {
            eprintln!("[feishu-comment] failed to deliver reply");
        }
    }

    // Cleanup: remove the OK reaction (best-effort).
    if !parsed.reply_id.is_empty() {
        api.comment_reaction(
            &parsed.file_token,
            &parsed.file_type,
            &parsed.reply_id,
            "delete",
        )
        .await;
    }
}

async fn resolve_wiki_links(api: &CommentApi, links: &mut Vec<Value>) {
    for link in links.iter_mut() {
        if link.get("doc_type").and_then(|v| v.as_str()) != Some("wiki") {
            continue;
        }
        let token = link
            .get("token")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let node = api.wiki_get_node(&token, "").await;
        let resolved_type = node.get("obj_type").and_then(|v| v.as_str()).unwrap_or("");
        let resolved_token = node.get("obj_token").and_then(|v| v.as_str()).unwrap_or("");
        if !resolved_type.is_empty() && !resolved_token.is_empty() {
            link["resolved_type"] = json!(resolved_type);
            link["resolved_token"] = json!(resolved_token);
        }
    }
}

/// Aux-event router shared by the webhook and WS transports.
pub async fn dispatch_aux_event(
    cfg: &crate::feishu::FeishuConfig,
    dispatcher: &Arc<Dispatcher>,
    event_type: &str,
    envelope: &Value,
) {
    match event_type {
        "drive.notice.comment_add_v1" => {
            handle_drive_comment_event(cfg, dispatcher, envelope).await
        }
        "vc.bot.meeting_invited_v1" => {
            crate::feishu_meeting::handle_meeting_invited_event(cfg, dispatcher, envelope).await
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_comment_envelope() -> Value {
        json!({
            "header": { "event_id": "evt-c1", "event_type": "drive.notice.comment_add_v1" },
            "event": {
                "comment_id": "cmt_1",
                "reply_id": "rep_1",
                "is_mentioned": true,
                "notice_meta": {
                    "file_token": "doxcnabc123",
                    "file_type": "docx",
                    "notice_type": "add_reply",
                    "from_user_id": { "open_id": "ou_user" },
                    "to_user_id": { "open_id": "ou_bot" },
                },
            },
        })
    }

    #[test]
    fn parse_event_fields() {
        let parsed = parse_drive_comment_event(&sample_comment_envelope()).expect("parses");
        assert_eq!(parsed.comment_id, "cmt_1");
        assert_eq!(parsed.reply_id, "rep_1");
        assert!(parsed.is_mentioned);
        assert_eq!(parsed.file_token, "doxcnabc123");
        assert_eq!(parsed.file_type, "docx");
        assert_eq!(parsed.notice_type, "add_reply");
        assert_eq!(parsed.from_open_id, "ou_user");
        assert_eq!(parsed.to_open_id, "ou_bot");
        assert!(parse_drive_comment_event(&json!({})).is_none());
    }

    #[test]
    fn rule_resolution_fallback() {
        let mut cfg = CommentsConfig::default();
        cfg.policy = "allowlist".into();
        cfg.allow_from = vec!["ou_top".into()];
        cfg.documents.insert(
            "*".into(),
            CommentDocumentRule {
                policy: Some("pairing".into()),
                ..Default::default()
            },
        );
        cfg.documents.insert(
            "docx:exact1".into(),
            CommentDocumentRule {
                enabled: Some(false),
                allow_from: Some(vec!["ou_exact".into()]),
                ..Default::default()
            },
        );
        // Exact match: enabled=false from exact, policy from wildcard,
        // allow_from from exact.
        let rule = resolve_rule(&cfg, "docx", "exact1", "");
        assert!(!rule.enabled);
        assert_eq!(rule.policy, "pairing");
        assert_eq!(rule.allow_from, vec!["ou_exact".to_string()]);
        // Wildcard tier.
        let rule = resolve_rule(&cfg, "docx", "other", "");
        assert!(rule.enabled);
        assert_eq!(rule.policy, "pairing");
        assert_eq!(rule.allow_from, vec!["ou_top".to_string()]);
        // Wiki key lookup.
        cfg.documents.insert(
            "wiki:wk1".into(),
            CommentDocumentRule {
                enabled: Some(false),
                ..Default::default()
            },
        );
        let rule = resolve_rule(&cfg, "docx", "other", "wk1");
        assert!(!rule.enabled);
    }

    #[test]
    fn reply_text_extraction() {
        let reply = json!({
            "user_id": { "open_id": "ou_a" },
            "content": {
                "elements": [
                    { "type": "person", "person": { "user_id": "ou_bot" } },
                    { "type": "text_run", "text_run": { "text": "  please review " } },
                    { "type": "docs_link", "docs_link": { "url": "https://x.feishu.cn/docx/abcdefghij" } },
                ],
            },
        });
        assert_eq!(
            extract_reply_text(&reply),
            "@ou_bot  please review https://x.feishu.cn/docx/abcdefghij"
        );
        assert_eq!(get_reply_user_id(&reply), "ou_a");
        // Semantic text strips self mention + collapses whitespace.
        assert_eq!(
            extract_semantic_text(&reply, "ou_bot"),
            "please review https://x.feishu.cn/docx/abcdefghij"
        );
        // String-encoded content.
        let reply2 = json!({ "content": "{\"elements\":[{\"type\":\"text_run\",\"text_run\":{\"text\":\"hi\"}}]}" });
        assert_eq!(extract_reply_text(&reply2), "hi");
    }

    #[test]
    fn docs_link_extraction_and_format() {
        let replies = vec![
            json!({
                "content": { "elements": [
                    { "type": "docs_link", "docs_link": { "url": "https://abc.feishu.cn/wiki/wikntoken123" } },
                    { "type": "docs_link", "docs_link": { "url": "https://abc.feishu.cn/wiki/wikntoken123" } },
                ]},
            }),
            json!({
                "content": { "elements": [
                    { "type": "link", "link": { "url": "https://abc.feishu.cn/docx/docxtoken456" } },
                    { "type": "link", "link": { "url": "https://example.com/nope" } },
                ]},
            }),
        ];
        let links = extract_docs_links(&replies);
        assert_eq!(links.len(), 2);
        assert_eq!(links[0]["doc_type"], "wiki");
        assert_eq!(links[1]["token"], "docxtoken456");
        let formatted = format_referenced_docs(&links, "docxtoken456");
        assert!(formatted.contains("wiki:wikntoken123"));
        assert!(formatted.contains("(same as current document)"));
    }

    #[test]
    fn timeline_selection_windows() {
        let timeline: Vec<TimelineEntry> = (0..50)
            .map(|i| (format!("u{i}"), format!("msg {i}"), i == 25))
            .collect();
        let selected = select_local_timeline(&timeline, 25);
        assert_eq!(selected.len(), LOCAL_TIMELINE_LIMIT);
        assert_eq!(selected.first().unwrap().0, "u0");
        assert_eq!(selected.last().unwrap().0, "u49");
        assert!(selected.iter().any(|(u, _, _)| u == "u25"));
        let selected = select_whole_timeline(&timeline, 25, 30);
        assert_eq!(selected.len(), WHOLE_TIMELINE_LIMIT);
        assert!(selected.iter().any(|(u, _, _)| u == "u25"));
        // Small timelines pass through.
        let small: Vec<TimelineEntry> = vec![("a".into(), "b".into(), false)];
        assert_eq!(select_local_timeline(&small, 0).len(), 1);
    }

    #[test]
    fn chunk_text_prefers_line_breaks() {
        let text = format!("{}\n{}", "a".repeat(3999), "b".repeat(10));
        let chunks = chunk_text(&text, REPLY_CHUNK_SIZE);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], "a".repeat(3999));
        assert_eq!(chunks[1], "b".repeat(10));
        assert_eq!(chunk_text("short", REPLY_CHUNK_SIZE), vec!["short"]);
    }

    #[test]
    fn sanitize_escapes_markup() {
        assert_eq!(
            sanitize_comment_text("a < b & c > d"),
            "a &lt; b &amp; c &gt; d"
        );
    }

    #[test]
    fn prompt_builders_include_context() {
        let timeline = vec![
            ("ou_a".into(), "first".into(), false),
            ("ou_bot".into(), "my reply".into(), true),
        ];
        let prompt = build_local_comment_prompt(
            "My Doc",
            "https://doc.example",
            "tok1",
            "docx",
            "cmt1",
            "quoted",
            "root",
            "target text",
            &timeline,
            1,
            "",
        );
        assert!(prompt.contains("The user added a reply in \"My Doc\"."));
        assert!(prompt.contains("[ou_bot] my reply <-- YOU"));
        assert!(prompt.contains("NO_REPLY"));
        let prompt = build_whole_comment_prompt(
            "My Doc",
            "https://doc.example",
            "tok1",
            "docx",
            "whole text",
            &timeline,
            0,
            -1,
            "",
        );
        assert!(prompt.contains("whole-document comment"));
        assert!(prompt.contains("whole text"));
    }
}

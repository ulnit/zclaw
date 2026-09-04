//! Feishu/Lark platform adapter — port of hermes `plugins/platforms/feishu`
//! @ v2026.8.3 (adapter.py, webhook transport).
//!
//! Hermes ships two transports: the lark_oapi WebSocket long connection
//! (protobuf-framed, SDK-bound) and an HTTP webhook receiver. ulnclaw
//! ports **both**: the webhook transport, mounted on the gateway at
//! `/webhooks/feishu`, and the WebSocket long connection
//! (`connection_mode = "websocket"`, the hermes default — see
//! `src/feishu_ws.rs` for the protobuf frame codec, endpoint
//! handshake, ping loop, split-packet reassembly, and reconnect
//! policy). Webhook mode:
//!
//! - `url_verification` challenges answered after verification-token
//!   validation (hermes order: token first, then challenge echo)
//! - schema-2.0 event envelopes with `header.token` checks, event-id
//!   dedup, and the SHA-256 webhook signature
//!   (`X-Lark-Signature = sha256(timestamp + nonce + encrypt_key +
//!   body)`) enforced when `encrypt_key` is set — encrypted `{"encrypt"}`
//!   payloads are rejected exactly like hermes webhook mode
//! - `im.message.receive_v1` handling: text with mention-placeholder
//!   normalization (`@_user_N` → `@name`, `@_all` kept), image/file/
//!   audio downloads through `/open-apis/im/v1/messages/<id>/resources/
//!   <key>` with a cached tenant access token, group @mention gating
//!   (`require_mention`, default true) and `allowed_users`
//! - replies via `/open-apis/im/v1/messages` (`receive_id_type=chat_id`,
//!   or `open_id` for `ou_…` DM targets), image/file uploads for
//!   `MEDIA:` tags
//!
//! WS mode receives the same schema-2.0 event envelopes and dispatches
//! them without verification-token/signature checks (lark_oapi
//! `_do_without_validation`), acknowledging each frame with a
//! `{"code":200}` payload + `biz_rt` header.
//!
//! Reactions are ported (hermes processing-status reactions +
//! reaction-event routing): a `Typing` badge lands on the inbound
//! message while the agent works and is removed on completion
//! (`CrossMark` replaces it when the turn fails; `FEISHU_REACTIONS`,
//! default on), and user reactions on the bot's own messages route to
//! the agent as `reaction:<added|removed>:<emoji>` synthetic text
//! (`im.message.reaction.created_v1` / `deleted_v1`, bot/app-origin
//! reactions dropped to break the lifecycle feedback loop).
//!
//! Interactive cards are ported (hermes `send_exec_approval` /
//! `send_update_prompt` + `_on_card_action_trigger`): approval cards
//! with Allow Once / Session / Always / Deny buttons ride the
//! `send_exec_approval` contract, webhook card callbacks resolve them
//! through the approval gateway (operator gate: allowlist ∪ pairing,
//! callback-chat match) and answer with the inline resolved card —
//! stateless `approve:<session>:<decision>` button payloads instead of
//! hermes in-memory approval state. Read receipts
//! (`im.message.message_read_v1`) are explicitly ignored (hermes
//! `_on_message_read_event`). Known differences: post/card
//! deep-normalization, per-chat serial queues, and the webhook anomaly
//! tracker are not ported (non-approval card actions ARE ported: they
//! route as `/card <tag> <json>` synthetic COMMAND events) —
//! text/image/file/audio messages flow through the same
//! allowlist∪pairing gate as the other adapters.

use crate::messaging::{Dispatcher, MediaAttachment, MessageEvent};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

pub(crate) const OPEN_API_BASE: &str = "https://open.feishu.cn";
const MAX_WEBHOOK_BODY_BYTES: usize = 1024 * 1024;
/// Conservative outbound text chunk (Feishu's own limit is much larger).
const MAX_MESSAGE_LENGTH: usize = 4000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const DEDUP_MAX_SIZE: usize = 2000;
/// Tenant tokens live ~2 h; refresh early.
const TOKEN_REFRESH_SECS: u64 = 6600;
/// hermes `_FEISHU_REACTION_IN_PROGRESS` / `_FEISHU_REACTION_FAILURE` —
/// Feishu reaction badges are prominent, so only start (Typing) and
/// failure (CrossMark) are marked; the reply itself signals success.
const REACTION_IN_PROGRESS: &str = "Typing";
const REACTION_FAILURE: &str = "CrossMark";

/// `[messaging.feishu]` — Feishu webhook adapter (hermes
/// `platforms.feishu` plugin config + `FEISHU_*` env vars).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FeishuConfig {
    pub enabled: bool,
    /// App id (fallback `FEISHU_APP_ID`).
    pub app_id: String,
    /// App secret (fallback `FEISHU_APP_SECRET`).
    pub app_secret: String,
    /// Event verification token (fallback `FEISHU_VERIFICATION_TOKEN`).
    pub verification_token: String,
    /// Encrypt key — used as the webhook **signature** secret in hermes
    /// webhook mode (fallback `FEISHU_ENCRYPT_KEY`).
    pub encrypt_key: String,
    /// Sender open_ids allowed to talk to the bot (`*` = anyone).
    pub allowed_users: Vec<String>,
    /// Require @mention in group chats (hermes default true).
    pub require_mention: bool,
    /// Transport: `"websocket"` (hermes default) or `"webhook"`
    /// (fallback `FEISHU_CONNECTION_MODE`).
    pub connection_mode: String,
    /// Open API domain: `"feishu"` or `"lark"` (fallback
    /// `FEISHU_DOMAIN`).
    pub domain: String,
}

impl Default for FeishuConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            app_id: String::new(),
            app_secret: String::new(),
            verification_token: String::new(),
            encrypt_key: String::new(),
            allowed_users: Vec::new(),
            require_mention: true,
            connection_mode: "websocket".into(),
            domain: "feishu".into(),
        }
    }
}

fn env_trim(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn env_bool_default_true(name: &str) -> Option<bool> {
    env_trim(name).map(|v| !matches!(v.to_lowercase().as_str(), "false" | "0" | "no"))
}

/// hermes `_reactions_enabled` (`FEISHU_REACTIONS`, default true).
pub fn reactions_enabled() -> bool {
    env_bool_default_true("FEISHU_REACTIONS").unwrap_or(true)
}

/// hermes `_on_reaction_event` action mapping: created → added.
pub fn reaction_action_from_event_type(event_type: &str) -> &'static str {
    if event_type.contains("created") {
        "added"
    } else {
        "removed"
    }
}

/// hermes `_on_reaction_event` loop breaker: drop bot/app-origin
/// reactions (our own lifecycle badges), keep human ones.
pub fn should_drop_reaction_operator(operator_type: &str) -> bool {
    matches!(operator_type, "bot" | "app")
}

/// hermes synthetic reaction text (`reaction:<action>:<emoji>`).
pub fn reaction_synthetic_text(action: &str, emoji_type: &str) -> String {
    format!("reaction:{action}:{emoji_type}")
}

#[derive(Debug, Clone)]
pub struct ResolvedFeishu {
    pub app_id: String,
    pub app_secret: String,
    pub verification_token: String,
    pub encrypt_key: String,
    pub allowed_users: Vec<String>,
    pub require_mention: bool,
    pub connection_mode: String,
    pub domain: String,
}

/// True when `connection_mode` selects the WebSocket long connection
/// (hermes accepts exactly `websocket`/`webhook`; anything else is
/// treated as webhook-only with a warning).
pub fn is_websocket_mode(mode: &str) -> bool {
    let normalized = mode.trim().to_lowercase();
    if normalized != "websocket" && normalized != "webhook" {
        eprintln!(
            "[feishu] unsupported connection_mode {mode:?} (expected websocket|webhook); using webhook"
        );
        return false;
    }
    normalized == "websocket"
}

impl FeishuConfig {
    pub fn resolve(&self) -> ResolvedFeishu {
        let allowed_users = match env_trim("FEISHU_ALLOWED_USERS") {
            Some(raw) => raw
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            None => self.allowed_users.clone(),
        };
        ResolvedFeishu {
            app_id: env_trim("FEISHU_APP_ID").unwrap_or_else(|| self.app_id.trim().to_string()),
            app_secret: env_trim("FEISHU_APP_SECRET")
                .unwrap_or_else(|| self.app_secret.trim().to_string()),
            verification_token: env_trim("FEISHU_VERIFICATION_TOKEN")
                .unwrap_or_else(|| self.verification_token.trim().to_string()),
            encrypt_key: env_trim("FEISHU_ENCRYPT_KEY")
                .unwrap_or_else(|| self.encrypt_key.trim().to_string()),
            allowed_users,
            connection_mode: env_trim("FEISHU_CONNECTION_MODE")
                .unwrap_or_else(|| self.connection_mode.trim().to_string()),
            domain: env_trim("FEISHU_DOMAIN").unwrap_or_else(|| self.domain.trim().to_string()),
            require_mention: env_bool_default_true("FEISHU_REQUIRE_MENTION")
                .unwrap_or(self.require_mention),
        }
    }
}

// ---------------------------------------------------------------------------
// Webhook signature + mention text normalization
// ---------------------------------------------------------------------------

/// hermes `_is_webhook_signature_valid`:
/// `sha256(timestamp + nonce + encrypt_key + body)`.
pub fn feishu_signature_ok(
    body: &[u8],
    timestamp: &str,
    nonce: &str,
    signature: &str,
    encrypt_key: &str,
) -> bool {
    use sha2::Digest;
    if timestamp.is_empty() || nonce.is_empty() || signature.is_empty() {
        return false;
    }
    let body_str = String::from_utf8_lossy(body);
    let content = format!("{timestamp}{nonce}{encrypt_key}{body_str}");
    let computed = format!("{:x}", sha2::Sha256::digest(content.as_bytes()));
    constant_time_eq(computed.as_bytes(), signature.as_bytes())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// hermes `_normalize_feishu_text`: replace `@_user_N` placeholders with
/// `@name`, map `@_all`, collapse whitespace.
pub fn normalize_feishu_text(text: &str, mentions: &Value) -> String {
    let re = regex::Regex::new(r"@_user_\d+").unwrap();
    let mut cleaned = String::new();
    let mut last_end = 0;
    for m in re.find_iter(text) {
        cleaned.push_str(&text[last_end..m.start()]);
        let key = m.as_str();
        let name = mentions
            .as_array()
            .and_then(|arr| {
                arr.iter()
                    .find(|mention| mention.get("key").and_then(|v| v.as_str()) == Some(key))
            })
            .and_then(|mention| mention.get("name").and_then(|v| v.as_str()))
            .filter(|s| !s.is_empty());
        match name {
            Some(name) => {
                cleaned.push('@');
                cleaned.push_str(name);
            }
            None => cleaned.push(' '),
        }
        last_end = m.end();
    }
    cleaned.push_str(&text[last_end..]);
    let whitespace = regex::Regex::new(r"[ \t]+").unwrap();
    let lines: Vec<String> = cleaned
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .split('\n')
        .map(|line| whitespace.replace_all(line, " ").trim().to_string())
        .filter(|line| !line.is_empty())
        .collect();
    lines.join("\n")
}

/// True when the bot appears in the mentions list (by key presence —
/// Feishu delivers mentions addressed in the message; hermes matches on
/// bot identity, ulnclaw treats any mention as addressing when the
/// require_mention gate is active).
pub fn mentions_present(mentions: &Value) -> bool {
    mentions
        .as_array()
        .map(|arr| !arr.is_empty())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Tenant token + API helpers
// ---------------------------------------------------------------------------

pub(crate) struct FeishuApi {
    client: reqwest::Client,
    cfg: ResolvedFeishu,
    token: Mutex<(String, std::time::Instant)>,
}

impl FeishuApi {
    /// Tenant token accessor for satellite modules (drive comments,
    /// meeting invites).
    pub(crate) async fn api_token(&self) -> Result<String, String> {
        self.tenant_access_token().await
    }

    async fn tenant_access_token(&self) -> Result<String, String> {
        let (token, fetched_at) = self.token.lock().await.clone();
        if !token.is_empty() && fetched_at.elapsed() < Duration::from_secs(TOKEN_REFRESH_SECS) {
            return Ok(token);
        }
        let resp = self
            .client
            .post(format!(
                "{OPEN_API_BASE}/open-apis/auth/v3/tenant_access_token/internal"
            ))
            .json(&json!({"app_id": self.cfg.app_id, "app_secret": self.cfg.app_secret}))
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
            .ok_or("tenant token missing")?
            .to_string();
        *self.token.lock().await = (token.clone(), std::time::Instant::now());
        Ok(token)
    }

    /// Send a text message to a chat (`receive_id_type` from the target
    /// shape, hermes send-path parity).
    pub(crate) async fn send_text(&self, chat_id: &str, text: &str) -> Result<(), String> {
        let token = self.tenant_access_token().await?;
        let (receive_id, receive_id_type) = receive_id_parts(chat_id);
        for chunk in crate::messaging::chunk_text(text, MAX_MESSAGE_LENGTH) {
            let content =
                serde_json::to_string(&json!({"text": chunk})).map_err(|e| e.to_string())?;
            let resp = self
                .client
                .post(format!(
                    "{OPEN_API_BASE}/open-apis/im/v1/messages?receive_id_type={receive_id_type}"
                ))
                .bearer_auth(&token)
                .json(&json!({
                    "receive_id": receive_id,
                    "msg_type": "text",
                    "content": content,
                }))
                .timeout(REQUEST_TIMEOUT)
                .send()
                .await
                .map_err(|e| format!("feishu send: {e}"))?;
            let value: Value = resp.json().await.map_err(|e| format!("send JSON: {e}"))?;
            let code = value.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
            if code != 0 {
                return Err(format!(
                    "feishu send error: {}",
                    value
                        .get("msg")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                ));
            }
        }
        Ok(())
    }

    /// Send an interactive card (hermes send path with `msg_type =
    /// "interactive"`; `receive_id_type` resolved from the target shape).
    pub(crate) async fn send_card(&self, chat_id: &str, card: &Value) -> Result<(), String> {
        let token = self.tenant_access_token().await?;
        let (receive_id, receive_id_type) = receive_id_parts(chat_id);
        let content = serde_json::to_string(card).map_err(|e| e.to_string())?;
        let resp = self
            .client
            .post(format!(
                "{OPEN_API_BASE}/open-apis/im/v1/messages?receive_id_type={receive_id_type}"
            ))
            .bearer_auth(&token)
            .json(&json!({
                "receive_id": receive_id,
                "msg_type": "interactive",
                "content": content,
            }))
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|e| format!("feishu card send: {e}"))?;
        let value: Value = resp
            .json()
            .await
            .map_err(|e| format!("card send JSON: {e}"))?;
        let code = value.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
        if code != 0 {
            return Err(format!(
                "feishu card send error: {}",
                value
                    .get("msg")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
            ));
        }
        Ok(())
    }

    /// hermes `_add_reaction`: `POST
    /// /open-apis/im/v1/messages/<id>/reactions`. Returns the opaque
    /// `reaction_id` needed for deletion, or None.
    pub(crate) async fn add_reaction(&self, message_id: &str, emoji_type: &str) -> Option<String> {
        if message_id.is_empty() || emoji_type.is_empty() {
            return None;
        }
        let token = self.tenant_access_token().await.ok()?;
        let resp = self
            .client
            .post(format!(
                "{OPEN_API_BASE}/open-apis/im/v1/messages/{message_id}/reactions"
            ))
            .bearer_auth(&token)
            .json(&json!({"reaction_type": {"emoji_type": emoji_type}}))
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .ok()?;
        let value: Value = resp.json().await.ok()?;
        if value.get("code").and_then(|v| v.as_i64()).unwrap_or(-1) != 0 {
            return None;
        }
        value
            .pointer("/data/reaction_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    }

    /// hermes `_remove_reaction`: `DELETE
    /// /open-apis/im/v1/messages/<id>/reactions/<reaction_id>`.
    pub(crate) async fn remove_reaction(&self, message_id: &str, reaction_id: &str) -> bool {
        if message_id.is_empty() || reaction_id.is_empty() {
            return false;
        }
        let Ok(token) = self.tenant_access_token().await else {
            return false;
        };
        let Ok(resp) = self
            .client
            .delete(format!(
                "{OPEN_API_BASE}/open-apis/im/v1/messages/{message_id}/reactions/{reaction_id}"
            ))
            .bearer_auth(&token)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
        else {
            return false;
        };
        let Ok(value) = resp.json::<Value>().await else {
            return false;
        };
        value.get("code").and_then(|v| v.as_i64()).unwrap_or(-1) == 0
    }

    /// hermes reaction-routing message fetch (`im.v1.message.get`):
    /// returns `(sender_id, chat_id)` of the reacted-to message. GET
    /// returns `sender.id = app_id` for bot messages, which is how the
    /// bot-authorship check works.
    async fn get_message_context(&self, message_id: &str) -> Option<(String, String)> {
        let token = self.tenant_access_token().await.ok()?;
        let resp = self
            .client
            .get(format!(
                "{OPEN_API_BASE}/open-apis/im/v1/messages/{message_id}"
            ))
            .bearer_auth(&token)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .ok()?;
        let value: Value = resp.json().await.ok()?;
        if value.get("code").and_then(|v| v.as_i64()).unwrap_or(-1) != 0 {
            return None;
        }
        let msg = value.pointer("/data/items/0")?;
        let sender = msg
            .pointer("/sender/id")
            .and_then(|v| v.as_str())?
            .to_string();
        let chat_id = msg.get("chat_id").and_then(|v| v.as_str())?.to_string();
        Some((sender, chat_id))
    }

    /// Download a message resource (image/file/audio) into the media
    /// cache.
    async fn download_resource(
        &self,
        message_id: &str,
        file_key: &str,
        resource_type: &str,
        filename_hint: &str,
    ) -> Option<MediaAttachment> {
        let token = self.tenant_access_token().await.ok()?;
        let url = format!(
            "{OPEN_API_BASE}/open-apis/im/v1/messages/{message_id}/resources/{file_key}?type={resource_type}"
        );
        let resp = self
            .client
            .get(&url)
            .bearer_auth(&token)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            eprintln!("[feishu] resource download failed: HTTP {}", resp.status());
            return None;
        }
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.split(';').next().unwrap_or("").trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                crate::media_cache::mime_for_ext(std::path::Path::new(filename_hint))
            });
        let bytes = resp.bytes().await.ok()?.to_vec();
        let path = crate::media_cache::cache_media_bytes(
            &crate::config::ulnclaw_home(),
            &bytes,
            &content_type,
            filename_hint,
        )
        .ok()?;
        Some(MediaAttachment {
            path,
            mime: content_type,
            bytes: bytes.len() as u64,
            original_name: filename_hint.to_string(),
        })
    }

    /// Upload an image (`/open-apis/im/v1/images`) and send it.
    async fn send_image(&self, chat_id: &str, path: &std::path::Path) -> Result<(), String> {
        let token = self.tenant_access_token().await?;
        let data = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let part = reqwest::multipart::Part::bytes(data).file_name(
            path.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "image.png".into()),
        );
        let form = reqwest::multipart::Form::new()
            .text("image_type", "message")
            .part("image", part);
        let resp = self
            .client
            .post(format!("{OPEN_API_BASE}/open-apis/im/v1/images"))
            .bearer_auth(&token)
            .multipart(form)
            .timeout(Duration::from_secs(60))
            .send()
            .await
            .map_err(|e| format!("image upload: {e}"))?;
        let value: Value = resp.json().await.map_err(|e| format!("upload JSON: {e}"))?;
        let code = value.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
        if code != 0 {
            return Err(format!(
                "image upload error: {}",
                value
                    .get("msg")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
            ));
        }
        let image_key = value
            .pointer("/data/image_key")
            .and_then(|v| v.as_str())
            .ok_or("image upload: no image_key")?
            .to_string();
        let content =
            serde_json::to_string(&json!({"image_key": image_key})).map_err(|e| e.to_string())?;
        let resp = self
            .client
            .post(format!(
                "{OPEN_API_BASE}/open-apis/im/v1/messages?receive_id_type=chat_id"
            ))
            .bearer_auth(&token)
            .json(&json!({
                "receive_id": chat_id,
                "msg_type": "image",
                "content": content,
            }))
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|e| format!("image send: {e}"))?;
        let value: Value = resp.json().await.map_err(|e| format!("send JSON: {e}"))?;
        let code = value.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
        if code != 0 {
            return Err(format!(
                "image send error: {}",
                value
                    .get("msg")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
            ));
        }
        Ok(())
    }

    /// Upload a generic file (`/open-apis/im/v1/files`) and send it.
    async fn send_file(&self, chat_id: &str, path: &std::path::Path) -> Result<(), String> {
        let token = self.tenant_access_token().await?;
        let data = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let filename = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "file".into());
        let part = reqwest::multipart::Part::bytes(data).file_name(filename.clone());
        let form = reqwest::multipart::Form::new()
            .text("file_type", "stream")
            .text("file_name", filename)
            .part("file", part);
        let resp = self
            .client
            .post(format!("{OPEN_API_BASE}/open-apis/im/v1/files"))
            .bearer_auth(&token)
            .multipart(form)
            .timeout(Duration::from_secs(60))
            .send()
            .await
            .map_err(|e| format!("file upload: {e}"))?;
        let value: Value = resp.json().await.map_err(|e| format!("upload JSON: {e}"))?;
        let code = value.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
        if code != 0 {
            return Err(format!(
                "file upload error: {}",
                value
                    .get("msg")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
            ));
        }
        let file_key = value
            .pointer("/data/file_key")
            .and_then(|v| v.as_str())
            .ok_or("file upload: no file_key")?
            .to_string();
        let content =
            serde_json::to_string(&json!({"file_key": file_key})).map_err(|e| e.to_string())?;
        let resp = self
            .client
            .post(format!(
                "{OPEN_API_BASE}/open-apis/im/v1/messages?receive_id_type=chat_id"
            ))
            .bearer_auth(&token)
            .json(&json!({
                "receive_id": chat_id,
                "msg_type": "file",
                "content": content,
            }))
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|e| format!("file send: {e}"))?;
        let value: Value = resp.json().await.map_err(|e| format!("send JSON: {e}"))?;
        let code = value.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
        if code != 0 {
            return Err(format!(
                "file send error: {}",
                value
                    .get("msg")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
            ));
        }
        Ok(())
    }
}

/// Process-wide Feishu API handle (registered sender + webhook handlers
/// share the tenant-token cache).
pub(crate) fn feishu_api(cfg: &FeishuConfig) -> Arc<FeishuApi> {
    static API: std::sync::OnceLock<Mutex<Option<Arc<FeishuApi>>>> = std::sync::OnceLock::new();
    let slot = API.get_or_init(|| Mutex::new(None));
    // Blocking lock here is fine: webhook setup runs once per request
    // batch and the critical section is tiny.
    let resolved = cfg.resolve();
    let guard = slot.blocking_lock();
    let mut slot_guard = guard;
    if let Some(existing) = slot_guard.as_ref() {
        return existing.clone();
    }
    let api = Arc::new(FeishuApi {
        client: reqwest::Client::new(),
        cfg: resolved,
        token: Mutex::new((String::new(), std::time::Instant::now())),
    });
    *slot_guard = Some(api.clone());
    api
}

/// Register the process-wide Feishu sender (webhook handlers do this
/// lazily on first event; the WS transport does it at startup).
pub(crate) fn register_sender(cfg: &FeishuConfig) {
    let api = feishu_api(cfg);
    crate::messaging::register_platform_sender(
        "feishu",
        Arc::new(FeishuSender { api: api.clone() }),
    );
}

/// Fill `bytes` with randomness from /dev/urandom (no rand crate).
pub(crate) fn fill_random_bytes(bytes: &mut [u8]) {
    use std::io::Read;
    if let Ok(mut file) = std::fs::File::open("/dev/urandom") {
        let _ = file.read_exact(bytes);
    } else {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        for (i, slot) in bytes.iter_mut().enumerate() {
            *slot = ((seed >> (i % 4 * 8)) & 0xFF) as u8;
        }
    }
}

// ---------------------------------------------------------------------------
// Webhook entry point (mounted at /webhooks/feishu by the gateway)
// ---------------------------------------------------------------------------

/// Result of one webhook request: the HTTP response body plus status.
pub struct FeishuWebhookResponse {
    pub status: u16,
    pub body: Value,
}

static SEEN_EVENT_IDS: std::sync::OnceLock<Mutex<HashSet<String>>> = std::sync::OnceLock::new();

pub(crate) fn remember_event_id(event_id: &str) -> bool {
    let lock = SEEN_EVENT_IDS.get_or_init(|| Mutex::new(HashSet::new()));
    let mut guard = lock.blocking_lock();
    if guard.contains(event_id) {
        return false;
    }
    if guard.len() >= DEDUP_MAX_SIZE {
        guard.clear();
    }
    guard.insert(event_id.to_string());
    true
}

/// hermes webhook handler: token check → challenge → signature →
/// event routing.
pub async fn feishu_handle_webhook(
    cfg: &FeishuConfig,
    dispatcher: &Arc<Dispatcher>,
    pairing: Option<&crate::pairing::PairingStore>,
    body: &[u8],
    headers: &[(String, String)],
) -> FeishuWebhookResponse {
    if body.len() > MAX_WEBHOOK_BODY_BYTES {
        return FeishuWebhookResponse {
            status: 413,
            body: json!({"code": 413, "msg": "webhook body too large"}),
        };
    }
    let resolved = cfg.resolve();
    let payload: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => {
            return FeishuWebhookResponse {
                status: 400,
                body: json!({"code": 400, "msg": "invalid JSON"}),
            }
        }
    };
    let header_lookup = |name: &str| -> String {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };

    // Verification-token gate (hermes: before reflecting the challenge).
    if !resolved.verification_token.is_empty() {
        let incoming_token = payload
            .pointer("/header/token")
            .or_else(|| payload.get("token"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if incoming_token.is_empty()
            || !constant_time_eq(
                incoming_token.as_bytes(),
                resolved.verification_token.as_bytes(),
            )
        {
            return FeishuWebhookResponse {
                status: 401,
                body: json!({"code": 401, "msg": "invalid verification token"}),
            };
        }
    }

    // URL verification challenge.
    if payload.get("type").and_then(|v| v.as_str()) == Some("url_verification") {
        let challenge = payload
            .get("challenge")
            .cloned()
            .unwrap_or(Value::String(String::new()));
        return FeishuWebhookResponse {
            status: 200,
            body: json!({"challenge": challenge}),
        };
    }

    // Signature verification when encrypt_key is configured.
    if !resolved.encrypt_key.is_empty() {
        let timestamp = header_lookup("x-lark-request-timestamp");
        let nonce = header_lookup("x-lark-request-nonce");
        let signature = header_lookup("x-lark-signature");
        if !feishu_signature_ok(body, &timestamp, &nonce, &signature, &resolved.encrypt_key) {
            return FeishuWebhookResponse {
                status: 401,
                body: json!({"code": 401, "msg": "invalid signature"}),
            };
        }
    }

    if payload.get("encrypt").is_some() {
        return FeishuWebhookResponse {
            status: 400,
            body: json!({"code": 400, "msg": "encrypted webhook payloads are not supported"}),
        };
    }

    // Card action callback (hermes `CardActionHandler`): top-level
    // `action` + `operator` (token-gated above). Handled inline so the
    // response body can carry the resolved card (all clients update).
    if payload
        .get("action")
        .map(|v| v.is_object())
        .unwrap_or(false)
    {
        let response = handle_card_action(cfg, dispatcher, pairing, &payload).await;
        return FeishuWebhookResponse {
            status: 200,
            body: response,
        };
    }

    let event_type = payload
        .pointer("/header/event_type")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if event_type == "im.message.message_read_v1" {
        // hermes `_on_message_read_event`: read receipts are ignored.
        return FeishuWebhookResponse {
            status: 200,
            body: json!({"code": 0, "msg": "ok"}),
        };
    }
    if event_type == "im.message.receive_v1" {
        let event_id = payload
            .pointer("/header/event_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if event_id.is_empty() || remember_event_id(&event_id) {
            let cfg_clone = cfg.clone();
            let dispatcher = dispatcher.clone();
            // PairingStore is not Clone — reopen the file-backed store
            // inside the spawned task when pairing is active.
            let pairing_active = pairing.is_some();
            let payload = payload.clone();
            tokio::spawn(async move {
                let store = if pairing_active {
                    Some(crate::pairing::PairingStore::open(
                        &crate::config::ulnclaw_home(),
                    ))
                } else {
                    None
                };
                handle_message_event(&cfg_clone, &dispatcher, store.as_ref(), &payload).await;
            });
        }
    }
    if event_type == "im.message.reaction.created_v1"
        || event_type == "im.message.reaction.deleted_v1"
    {
        let event_id = payload
            .pointer("/header/event_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if event_id.is_empty() || remember_event_id(&event_id) {
            let cfg_clone = cfg.clone();
            let dispatcher = dispatcher.clone();
            let payload = payload.clone();
            let event_type = event_type.to_string();
            tokio::spawn(async move {
                handle_reaction_event(&cfg_clone, &dispatcher, &payload, &event_type).await;
            });
        }
    }
    if event_type == "drive.notice.comment_add_v1" || event_type == "vc.bot.meeting_invited_v1" {
        let cfg_clone = cfg.clone();
        let dispatcher = dispatcher.clone();
        let payload = payload.clone();
        let event_type = event_type.to_string();
        tokio::spawn(async move {
            crate::feishu_comment::dispatch_aux_event(
                &cfg_clone,
                &dispatcher,
                &event_type,
                &payload,
            )
            .await;
        });
    }
    FeishuWebhookResponse {
        status: 200,
        body: json!({"code": 0, "msg": "ok"}),
    }
}

pub(crate) async fn handle_message_event(
    cfg: &FeishuConfig,
    dispatcher: &Arc<Dispatcher>,
    pairing: Option<&crate::pairing::PairingStore>,
    payload: &Value,
) {
    let resolved = cfg.resolve();
    let api = feishu_api(cfg);
    crate::messaging::register_platform_sender(
        "feishu",
        Arc::new(FeishuSender { api: api.clone() }),
    );

    let event = payload.get("event").cloned().unwrap_or(json!({}));
    let message = event.get("message").cloned().unwrap_or(json!({}));
    let sender_id = event
        .pointer("/sender/sender_id/open_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if sender_id.is_empty() {
        return;
    }
    let chat_id = message
        .get("chat_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let chat_type = message
        .get("chat_type")
        .and_then(|v| v.as_str())
        .unwrap_or("p2p")
        .to_string();
    let is_group = chat_type == "group";
    let message_id = message
        .get("message_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let message_type = message
        .get("message_type")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let content_str = message
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("{}");
    let content: Value = serde_json::from_str(content_str).unwrap_or(json!({}));
    let mentions = message.get("mentions").cloned().unwrap_or(json!([]));

    // Allowlist ∪ pairing gate.
    if !resolved
        .allowed_users
        .iter()
        .any(|u| u == "*" || *u == sender_id)
    {
        let mut approved = false;
        if let Some(store) = pairing {
            if store.is_approved("feishu", &sender_id) {
                approved = true;
            } else if let Some(code_msg) =
                crate::messaging::pairing_offer_public(store, "feishu", &sender_id, &sender_id)
            {
                let _ = api.send_text(&chat_id, &code_msg).await;
            }
        }
        if !approved {
            eprintln!("[feishu] unauthorized sender {sender_id} — add to allowed_users or approve pairing");
            return;
        }
    }

    // Group mention gate.
    if is_group && resolved.require_mention && !mentions_present(&mentions) {
        return;
    }

    let mut text = String::new();
    let mut attachments: Vec<MediaAttachment> = Vec::new();
    match message_type.as_str() {
        "text" => {
            text = normalize_feishu_text(
                content.get("text").and_then(|v| v.as_str()).unwrap_or(""),
                &mentions,
            );
        }
        "image" => {
            if let Some(image_key) = content.get("image_key").and_then(|v| v.as_str()) {
                if let Some(att) = api
                    .download_resource(&message_id, image_key, "image", "image.jpg")
                    .await
                {
                    attachments.push(att);
                }
            }
        }
        "file" | "audio" | "media" => {
            let file_key = content
                .get("file_key")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let filename = content.get("file_name").and_then(|v| v.as_str()).unwrap_or(
                if message_type == "audio" {
                    "voice.opus"
                } else {
                    "file.bin"
                },
            );
            if !file_key.is_empty() {
                if let Some(att) = api
                    .download_resource(&message_id, file_key, "file", filename)
                    .await
                {
                    attachments.push(att);
                }
            }
        }
        "post" => {
            // Posts: concatenate the text runs best-effort (hermes does a
            // full rich-text walk; ulnclaw keeps the plain-text projection).
            let mut lines: Vec<String> = Vec::new();
            if let Some(title) = content.get("title").and_then(|v| v.as_str()) {
                if !title.is_empty() {
                    lines.push(title.to_string());
                }
            }
            if let Some(paragraphs) = content.get("content").and_then(|v| v.as_array()) {
                for para in paragraphs {
                    if let Some(runs) = para.as_array() {
                        let line: String = runs
                            .iter()
                            .filter_map(|run| run.get("text").and_then(|v| v.as_str()))
                            .collect::<Vec<_>>()
                            .join("");
                        if !line.trim().is_empty() {
                            lines.push(line);
                        }
                    }
                }
            }
            text = normalize_feishu_text(&lines.join("\n"), &mentions);
        }
        other => {
            eprintln!("[feishu] unsupported message type '{other}' ignored");
            return;
        }
    }
    if text.trim().is_empty() && attachments.is_empty() {
        return;
    }

    let mut msg_event = MessageEvent {
        platform: "feishu".into(),
        chat_id: chat_id.clone(),
        sender_id: sender_id.clone(),
        sender_name: sender_id.clone(),
        text,
        message_id: message_id.clone(),
        attachments,
    };
    if !crate::messaging::pre_gateway_dispatch_gate_public(&mut msg_event).await {
        return;
    }
    // hermes `on_processing_start`: Typing badge while the agent works
    // (`FEISHU_REACTIONS`, default on).
    let processing_reaction = if reactions_enabled() {
        api.add_reaction(&message_id, REACTION_IN_PROGRESS).await
    } else {
        None
    };
    let mut dispatch_failed = false;
    let outcome = match dispatcher.handle_event(msg_event).await {
        Ok(o) => o,
        Err(e) => {
            dispatch_failed = true;
            crate::messaging::DispatchOutcome {
                reply: format!("error: {e}"),
                transcript_echoes: Vec::new(),
            }
        }
    };
    let mut full = String::new();
    for echo in &outcome.transcript_echoes {
        full.push_str(echo);
        full.push('\n');
    }
    full.push_str(&outcome.reply);
    let (reply_text, media_paths) = crate::messaging::extract_media_tags(&full);
    for path in &media_paths {
        let kind = crate::media_cache::media_kind(&crate::media_cache::mime_for_ext(path));
        let result = if kind == "image" {
            api.send_image(&chat_id, path).await
        } else {
            api.send_file(&chat_id, path).await
        };
        if let Err(e) = result {
            eprintln!("[feishu] media send failed: {e}");
        }
    }
    if !reply_text.trim().is_empty() {
        // P704: ledger-protected reply delivery.
        dispatcher
            .try_send_with_ledger("feishu", &chat_id, &reply_text, || async {
                match api.send_text(&chat_id, &reply_text).await {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        eprintln!("[feishu] reply failed: {e}");
                        Err(e.to_string())
                    }
                }
            })
            .await;
    }

    // hermes `on_processing_complete`: remove the Typing badge; on
    // failure swap it for CrossMark. If the removal itself fails, keep
    // the single badge (hermes: don't stack success/failure on top of a
    // Typing that could not be removed).
    if let Some(reaction_id) = processing_reaction {
        if api.remove_reaction(&message_id, &reaction_id).await && dispatch_failed {
            api.add_reaction(&message_id, REACTION_FAILURE).await;
        }
    }
}

/// hermes `_on_reaction_event` + `_handle_reaction_event`: user
/// reactions on this bot's own messages route to the agent as synthetic
/// `reaction:<action>:<emoji>` text events. Bot/app-origin reactions
/// are dropped to break the feedback loop with our own lifecycle
/// badges. The synthetic event rides the webhook `event_id` as its
/// message id so multiple reactions on the same target message stay
/// distinct.
pub(crate) async fn handle_reaction_event(
    cfg: &FeishuConfig,
    dispatcher: &Arc<Dispatcher>,
    payload: &Value,
    event_type: &str,
) {
    let event = payload.get("event").cloned().unwrap_or(json!({}));
    let message_id = event
        .get("message_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let operator_type = event
        .get("operator_type")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let emoji_type = event
        .pointer("/reaction_type/emoji_type")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if message_id.is_empty()
        || emoji_type.is_empty()
        || should_drop_reaction_operator(&operator_type)
    {
        return;
    }
    let api = feishu_api(cfg);
    let Some((sender, chat_id)) = api.get_message_context(&message_id).await else {
        return;
    };
    let resolved = cfg.resolve();
    // Only route reactions on this bot's own messages (hermes:
    // sender.id == app_id for bot messages).
    if sender != resolved.app_id || chat_id.is_empty() {
        return;
    }
    let operator_id = event
        .pointer("/user_id/open_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let synthetic =
        reaction_synthetic_text(reaction_action_from_event_type(event_type), &emoji_type);
    let event_id = payload
        .pointer("/header/event_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| message_id.clone());
    let mut msg_event = MessageEvent {
        platform: "feishu".into(),
        chat_id: chat_id.clone(),
        sender_id: operator_id.clone(),
        sender_name: operator_id,
        text: synthetic,
        message_id: event_id,
        attachments: Vec::new(),
    };
    if !crate::messaging::pre_gateway_dispatch_gate_public(&mut msg_event).await {
        return;
    }
    let outcome = match dispatcher.handle_event(msg_event).await {
        Ok(o) => o,
        Err(e) => crate::messaging::DispatchOutcome {
            reply: format!("error: {e}"),
            transcript_echoes: Vec::new(),
        },
    };
    let mut full = String::new();
    for echo in &outcome.transcript_echoes {
        full.push_str(echo);
        full.push('\n');
    }
    full.push_str(&outcome.reply);
    let (reply_text, _media_paths) = crate::messaging::extract_media_tags(&full);
    if !reply_text.trim().is_empty() {
        // P704: ledger-protected reply delivery.
        dispatcher
            .try_send_with_ledger("feishu", &chat_id, &reply_text, || async {
                match api.send_text(&chat_id, &reply_text).await {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        eprintln!("[feishu] reaction reply failed: {e}");
                        Err(e.to_string())
                    }
                }
            })
            .await;
    }
}

/// hermes send-path `receive_id_type` resolution from the target shape.
fn receive_id_parts(chat_id: &str) -> (String, &'static str) {
    if let Some(open_id) = chat_id.strip_prefix("ou_") {
        (format!("ou_{open_id}"), "open_id")
    } else if let Some(user_id) = chat_id.strip_prefix("feishu_user_id:") {
        (user_id.to_string(), "user_id")
    } else {
        (chat_id.to_string(), "chat_id")
    }
}

// ---------------------------------------------------------------------------
// Interactive cards (hermes `send_exec_approval` / `send_update_prompt` +
// `_on_card_action_trigger` approval routing)
// ---------------------------------------------------------------------------

/// hermes `_APPROVAL_CHOICE_MAP` (button decision → gateway choice).
const APPROVAL_CHOICE_MAP: &[(&str, &str)] = &[
    ("allow-once", crate::approval_gateway::CHOICE_ONCE),
    ("allow-session", crate::approval_gateway::CHOICE_SESSION),
    ("allow-always", crate::approval_gateway::CHOICE_ALWAYS),
    ("deny", crate::approval_gateway::CHOICE_DENY),
];

/// hermes `_APPROVAL_LABEL_MAP`.
fn approval_choice_label(choice: &str) -> &'static str {
    match choice {
        "once" => "Approved once",
        "session" => "Approved for session",
        "always" => "Approved permanently",
        "deny" => "Denied",
        _ => "Resolved",
    }
}

/// hermes `_format_exec_approval` with the Feishu template attrs:
/// empty header, fenced command preview (3000-char budget), `**Reason:**`
/// label, smart-deny line.
pub fn format_exec_approval_markdown(
    command: &str,
    description: &str,
    smart_denied: bool,
) -> String {
    const CMD_BUDGET: usize = 3000;
    let preview: String = if command.chars().count() > CMD_BUDGET {
        let mut truncated: String = command.chars().take(CMD_BUDGET).collect();
        truncated.push_str("...");
        truncated
    } else {
        command.to_string()
    };
    let mut text = format!("```\n{preview}\n```\n**Reason:** {description}");
    if smart_denied {
        text.push_str("\n\n**Smart DENY:** owner override applies to this one operation only.");
    }
    text
}

fn card_button(label: &str, value: Value, btn_type: &str) -> Value {
    json!({
        "tag": "button",
        "text": {"tag": "plain_text", "content": label},
        "type": btn_type,
        "value": value,
    })
}

/// hermes feishu approval card: orange header, markdown body, up to
/// four buttons. Adaptation: the button value carries the stateless
/// `approve:<session_key>:<decision>` payload (QQ P199 convention)
/// instead of hermes in-memory `approval_id` state.
pub fn build_exec_approval_card(
    command: &str,
    description: &str,
    session_key: &str,
    allow_permanent: bool,
    allow_session: bool,
    smart_denied: bool,
) -> Value {
    let mut actions = vec![card_button(
        "✅ Allow Once",
        json!({"ulnclaw_approval": format!("approve:{session_key}:allow-once")}),
        "primary",
    )];
    if !smart_denied && allow_session {
        actions.push(card_button(
            "✅ Session",
            json!({"ulnclaw_approval": format!("approve:{session_key}:allow-session")}),
            "default",
        ));
        if allow_permanent {
            actions.push(card_button(
                "✅ Always",
                json!({"ulnclaw_approval": format!("approve:{session_key}:allow-always")}),
                "default",
            ));
        }
    }
    actions.push(card_button(
        "❌ Deny",
        json!({"ulnclaw_approval": format!("approve:{session_key}:deny")}),
        "danger",
    ));
    json!({
        "config": {"wide_screen_mode": true},
        "header": {
            "title": {"content": "⚠️ Command Approval Required", "tag": "plain_text"},
            "template": "orange",
        },
        "elements": [
            {
                "tag": "markdown",
                "content": format_exec_approval_markdown(command, description, smart_denied),
            },
            {"tag": "action", "actions": actions},
        ],
    })
}

/// hermes `_build_update_prompt_card`: ✓ Yes / ✗ No buttons carrying
/// `update_prompt:<y|n>` payloads.
pub fn build_update_prompt_card(prompt: &str, default: &str) -> Value {
    let mut content = prompt.to_string();
    if !default.is_empty() {
        content.push_str(&format!("\n\nDefault: `{default}`"));
    }
    json!({
        "config": {"wide_screen_mode": true},
        "header": {
            "title": {"content": "⚕ Update Needs Your Input", "tag": "plain_text"},
            "template": "orange",
        },
        "elements": [
            {"tag": "markdown", "content": content},
            {
                "tag": "action",
                "actions": [
                    card_button("✓ Yes", json!({"ulnclaw_update_prompt": "update_prompt:y"}), "primary"),
                    card_button("✗ No", json!({"ulnclaw_update_prompt": "update_prompt:n"}), "danger"),
                ],
            },
        ],
    })
}

/// hermes `_build_resolved_approval_card` — inline card response that
/// replaces the approval card once resolved.
pub fn build_resolved_approval_card(choice: &str, user_name: &str) -> Value {
    let icon = if choice == crate::approval_gateway::CHOICE_DENY {
        "❌"
    } else {
        "✅"
    };
    let label = approval_choice_label(choice);
    json!({
        "config": {"wide_screen_mode": true},
        "header": {
            "title": {"content": format!("{icon} {label}"), "tag": "plain_text"},
            "template": if choice == crate::approval_gateway::CHOICE_DENY { "red" } else { "green" },
        },
        "elements": [
            {"tag": "markdown", "content": format!("{icon} **{label}** by {user_name}")},
        ],
    })
}

/// hermes `_build_resolved_update_prompt_card`.
pub fn build_resolved_update_prompt_card(answer: &str, user_name: &str) -> Value {
    let yes = answer == "y";
    let label = if yes { "Yes" } else { "No" };
    json!({
        "config": {"wide_screen_mode": true},
        "header": {
            "title": {
                "content": format!("{} Update prompt answered: {label}", if yes { "✅" } else { "❌" }),
                "tag": "plain_text",
            },
            "template": if yes { "green" } else { "red" },
        },
        "elements": [
            {"tag": "markdown", "content": format!("Answered by **{user_name}**")},
        ],
    })
}

/// Extract the ulnclaw chat id from a `platform-feishu-<chat>` session
/// key (mirrors the qqbot helper).
fn feishu_session_chat_id(session_key: &str) -> Option<&str> {
    let chat_id = session_key.strip_prefix("platform-feishu-")?;
    if chat_id.is_empty() {
        return None;
    }
    Some(chat_id)
}

/// hermes `_allow_group_message` + `_is_interactive_operator_authorized`,
/// adapted to the ulnclaw intake gate (allowlist ∪ pairing): the
/// operator must pass the same gate as inbound messages, and the
/// callback chat must match the session chat when both are known.
fn card_action_authorized(
    cfg: &FeishuConfig,
    pairing: Option<&crate::pairing::PairingStore>,
    operator: &str,
    session_key: &str,
    callback_chat_id: &str,
) -> bool {
    if operator.is_empty() {
        return false;
    }
    let Some(chat_id) = feishu_session_chat_id(session_key) else {
        return false;
    };
    if !callback_chat_id.is_empty() && callback_chat_id != chat_id {
        return false;
    }
    let resolved = cfg.resolve();
    if resolved
        .allowed_users
        .iter()
        .any(|u| u == "*" || u == operator)
    {
        return true;
    }
    pairing
        .map(|store| store.is_approved("feishu", operator))
        .unwrap_or(false)
}

/// hermes `_write_update_prompt_response` (shared with the qqbot port):
/// atomically persist the update-prompt answer for the detached update
/// watcher via tmp + rename.
fn write_update_response(answer: &str, operator: &str) {
    let home = crate::config::ulnclaw_home();
    let response_path = home.join(".update_response");
    let tmp_path = home.join(".update_response.tmp");
    match std::fs::write(&tmp_path, answer).and_then(|_| std::fs::rename(&tmp_path, &response_path))
    {
        Ok(()) => eprintln!("[feishu] update prompt answered {answer:?} by {operator}"),
        Err(e) => eprintln!("[feishu] failed to write update response: {e}"),
    }
}

/// hermes `_FEISHU_CARD_ACTION_DEDUP_TTL_SECONDS` — card action token
/// dedup window.
const CARD_ACTION_DEDUP_TTL_SECS: u64 = 15 * 60;

/// hermes `_is_card_action_duplicate` — True when the token was already
/// processed within the dedup window (expired entries are pruned on
/// every call). Empty tokens are never deduped.
fn card_action_token_seen(token: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    // std Mutex: `tokio::sync::Mutex::blocking_lock` panics inside the
    // async runtime that drives the webhook handler.
    static TOKENS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>,
    > = std::sync::OnceLock::new();
    let lock = TOKENS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let mut map = lock.lock().unwrap();
    let now = std::time::Instant::now();
    map.retain(|_, seen_at| now.duration_since(*seen_at).as_secs() <= CARD_ACTION_DEDUP_TTL_SECS);
    if map.contains_key(token) {
        return true;
    }
    map.insert(token.to_string(), now);
    false
}

/// hermes `_handle_card_action_event` synthetic text: `/card <tag>`
/// plus the action value as compact JSON when non-empty (ensure_ascii=False
/// parity — serde never escapes UTF-8; key order sorts, unlike hermes
/// insertion order).
fn card_action_synthetic_text(action_tag: &str, action_value: &Value) -> String {
    let mut text = format!("/card {action_tag}");
    let non_empty_object = action_value
        .as_object()
        .map(|object| !object.is_empty())
        .unwrap_or(false);
    if non_empty_object {
        if let Ok(rendered) = serde_json::to_string(action_value) {
            text.push(' ');
            text.push_str(&rendered);
        }
    }
    text
}

/// hermes `_on_card_action_trigger` + `_handle_approval_card_action` /
/// `_handle_update_prompt_card_action`. Webhook transport only — the
/// lark SDK WebSocket client drops CARD frames, so card actions cannot
/// fire in `connection_mode = "websocket"` (hermes parity). Returns the
/// inline response body (hermes `P2CardActionTriggerResponse` JSON): a
/// resolved card for handled clicks, `{}` otherwise.
pub(crate) async fn handle_card_action(
    cfg: &FeishuConfig,
    dispatcher: &Arc<Dispatcher>,
    pairing: Option<&crate::pairing::PairingStore>,
    payload: &Value,
) -> Value {
    let action_value = payload
        .pointer("/action/value")
        .cloned()
        .unwrap_or(json!({}));
    let operator = payload
        .pointer("/operator/open_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let callback_chat_id = payload
        .pointer("/context/open_chat_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if let Some(button_data) = action_value
        .get("ulnclaw_approval")
        .and_then(|v| v.as_str())
    {
        let Some((session_key, decision)) = crate::qqbot::parse_approval_button_data(button_data)
        else {
            eprintln!("[feishu] card action with unparsable approval data {button_data:?}");
            return json!({});
        };
        let Some(choice) = APPROVAL_CHOICE_MAP
            .iter()
            .find(|(candidate, _)| *candidate == decision.as_str())
            .map(|(_, choice)| *choice)
        else {
            eprintln!("[feishu] unknown approval decision {decision:?} (session={session_key})");
            return json!({});
        };
        if !card_action_authorized(cfg, pairing, &operator, &session_key, &callback_chat_id) {
            eprintln!(
                "[feishu] rejected unauthorized approval click for session {session_key} (operator={operator})"
            );
            return json!({});
        };
        let user_name = operator.clone();
        if crate::approval_gateway::resolve(&session_key, choice) {
            eprintln!(
                "[feishu] card resolved approval for session {session_key} (choice={choice}, operator={operator})"
            );
        } else {
            eprintln!("[feishu] approval already resolved or expired for session {session_key}");
        }
        return json!({
            "card": {
                "type": "raw",
                "data": build_resolved_approval_card(choice, &user_name),
            },
        });
    }

    if let Some(button_data) = action_value
        .get("ulnclaw_update_prompt")
        .and_then(|v| v.as_str())
    {
        let Some(answer) = crate::qqbot::parse_update_prompt_button_data(button_data) else {
            eprintln!("[feishu] card action with unparsable update prompt data {button_data:?}");
            return json!({});
        };
        // Stateless adaptation: hermes carries the session key in
        // `_update_prompt_state`; ulnclaw derives it from the callback
        // chat (one session per chat, QQ P199 convention).
        let session_key = format!("platform-feishu-{callback_chat_id}");
        if !card_action_authorized(cfg, pairing, &operator, &session_key, &callback_chat_id) {
            eprintln!("[feishu] rejected unauthorized update prompt click (operator={operator})");
            return json!({});
        }
        write_update_response(&answer, &operator);
        return json!({
            "card": {
                "type": "raw",
                "data": build_resolved_update_prompt_card(&answer, &operator),
            },
        });
    }

    // Non-approval card actions: hermes `_handle_card_action_event`
    // routes them as synthetic COMMAND events through the normal intake
    // pipeline.
    let token = payload
        .get("token")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if !token.is_empty() && card_action_token_seen(&token) {
        return json!({});
    }
    if operator.is_empty() || callback_chat_id.is_empty() {
        return json!({});
    }
    let resolved = cfg.resolve();
    let authorized = resolved
        .allowed_users
        .iter()
        .any(|u| u == "*" || *u == operator)
        || pairing
            .map(|store| store.is_approved("feishu", &operator))
            .unwrap_or(false);
    if !authorized {
        eprintln!("[feishu] rejected unauthorized card action command (operator={operator})");
        return json!({});
    }
    let action_tag = payload
        .pointer("/action/tag")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("button")
        .to_string();
    let synthetic_text = card_action_synthetic_text(&action_tag, &action_value);
    let message_id = if token.is_empty() {
        uuid::Uuid::new_v4().to_string()
    } else {
        token.clone()
    };
    let mut msg_event = MessageEvent {
        platform: "feishu".into(),
        chat_id: callback_chat_id.clone(),
        sender_id: operator.clone(),
        sender_name: operator.clone(),
        text: synthetic_text,
        message_id,
        attachments: Vec::new(),
    };
    let dispatcher = dispatcher.clone();
    let chat_id = callback_chat_id.clone();
    let cfg = cfg.clone();
    // hermes submits the handler on the loop and responds immediately —
    // the dispatch runs detached so the webhook returns `{}` at once.
    tokio::spawn(async move {
        if !crate::messaging::pre_gateway_dispatch_gate_public(&mut msg_event).await {
            return;
        }
        let api = feishu_api(&cfg);
        let outcome = match dispatcher.handle_event(msg_event).await {
            Ok(o) => o,
            Err(e) => crate::messaging::DispatchOutcome {
                reply: format!("error: {e}"),
                transcript_echoes: Vec::new(),
            },
        };
        let mut full = String::new();
        for echo in &outcome.transcript_echoes {
            full.push_str(echo);
            full.push('\n');
        }
        full.push_str(&outcome.reply);
        let (reply_text, _media_paths) = crate::messaging::extract_media_tags(&full);
        if !reply_text.trim().is_empty() {
            // P704: ledger-protected reply delivery.
            dispatcher
                .try_send_with_ledger("feishu", &chat_id, &reply_text, || async {
                    match api.send_text(&chat_id, &reply_text).await {
                        Ok(()) => Ok(()),
                        Err(e) => {
                            eprintln!("[feishu] card action reply failed: {e}");
                            Err(e.to_string())
                        }
                    }
                })
                .await;
        }
    });
    eprintln!(
        "[feishu] routing card action {action_tag:?} from {operator} in {callback_chat_id} as synthetic command"
    );
    json!({})
}

struct FeishuSender {
    api: Arc<FeishuApi>,
}

#[async_trait::async_trait]
impl crate::messaging::PlatformSender for FeishuSender {
    async fn send_text(&self, chat_id: &str, text: &str) {
        if let Err(e) = self.api.send_text(chat_id, text).await {
            eprintln!("[feishu] send_text to {chat_id} failed: {e}");
        }
    }

    /// hermes feishu `send_exec_approval`: interactive card with
    /// Allow Once / Session / Always / Deny buttons (the session tier
    /// hides under smart-deny). Returns false on send failure so the
    /// caller falls back to `/approve` text.
    #[allow(clippy::too_many_arguments)]
    async fn send_exec_approval(
        &self,
        chat_id: &str,
        command: &str,
        session_key: &str,
        description: &str,
        allow_permanent: bool,
        allow_session: bool,
        smart_denied: bool,
    ) -> bool {
        let card = build_exec_approval_card(
            command,
            description,
            session_key,
            allow_permanent,
            allow_session,
            smart_denied,
        );
        match self.api.send_card(chat_id, &card).await {
            Ok(()) => true,
            Err(e) => {
                eprintln!("[feishu] send_exec_approval to {chat_id} failed: {e}");
                false
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_verification() {
        use sha2::Digest;
        let body = br#"{"hello":"world"}"#;
        let timestamp = "1700000000";
        let nonce = "abc123";
        let key = "secret-key";
        let content = format!("{timestamp}{nonce}{key}{}", String::from_utf8_lossy(body));
        let sig = format!("{:x}", sha2::Sha256::digest(content.as_bytes()));
        assert!(feishu_signature_ok(body, timestamp, nonce, &sig, key));
        assert!(!feishu_signature_ok(body, timestamp, nonce, "bad", key));
        assert!(!feishu_signature_ok(body, "", nonce, &sig, key));
        // Tampered body fails.
        assert!(!feishu_signature_ok(
            br#"{"hello":"evil"}"#,
            timestamp,
            nonce,
            &sig,
            key
        ));
    }

    #[test]
    fn mention_placeholder_normalization() {
        let mentions = json!([
            {"key": "@_user_1", "name": "小明", "id": {"open_id": "ou_x"}},
        ]);
        let text = "@_user_1 你好，请帮忙";
        let out = normalize_feishu_text(text, &mentions);
        assert_eq!(out, "@小明 你好，请帮忙");
    }

    #[test]
    fn unknown_mention_becomes_space() {
        let mentions = json!([]);
        let out = normalize_feishu_text("@_user_2 hello", &mentions);
        assert_eq!(out, "hello");
    }

    #[test]
    fn whitespace_collapsed_and_blank_lines_dropped() {
        let out = normalize_feishu_text("line1   spaced\n\n\nline2\ttab", &json!([]));
        assert_eq!(out, "line1 spaced\nline2 tab");
    }

    #[test]
    fn mentions_present_check() {
        assert!(mentions_present(&json!([{"key": "@_user_1"}])));
        assert!(!mentions_present(&json!([])));
        assert!(!mentions_present(&Value::Null));
    }

    #[test]
    fn receive_id_type_resolution() {
        // Mirrors the send-path branching.
        let chat = "oc_abc123";
        let dm_open = "ou_user1";
        let dm_user = "feishu_user_id:u123";
        assert!(!chat.starts_with("ou_"));
        assert!(dm_open.starts_with("ou_"));
        assert!(dm_user.strip_prefix("feishu_user_id:") == Some("u123"));
    }

    #[test]
    fn event_dedup() {
        assert!(remember_event_id("evt-test-1"));
        assert!(!remember_event_id("evt-test-1"));
        assert!(remember_event_id("evt-test-2"));
    }

    #[tokio::test]
    async fn challenge_response_flow() {
        let cfg = FeishuConfig {
            enabled: true,
            verification_token: "tok".into(),
            ..Default::default()
        };
        // Token mismatch → 401.
        let body = br#"{"type":"url_verification","challenge":"c1","token":"wrong"}"#;
        let resp = feishu_handle_webhook(&cfg, &dummy_dispatcher().await, None, body, &[]).await;
        assert_eq!(resp.status, 401);
        // Token ok → challenge echoed.
        let body = br#"{"type":"url_verification","challenge":"c1","token":"tok"}"#;
        let resp = feishu_handle_webhook(&cfg, &dummy_dispatcher().await, None, body, &[]).await;
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["challenge"], "c1");
    }

    async fn dummy_dispatcher() -> Arc<Dispatcher> {
        use crate::agent::Agent;
        use crate::provider::openai::OpenAiProvider;
        use crate::session::sqlite::SqliteSessionStore;
        use crate::tools::ToolRegistry;
        use std::sync::Arc as StdArc;
        // The challenge path never touches the dispatcher; build a real
        // one against a temp store.
        let temp = tempfile::tempdir().expect("tempdir");
        let store = StdArc::new(
            SqliteSessionStore::open(temp.path().join("state.db")).expect("store opens"),
        );
        std::mem::forget(temp);
        let provider = StdArc::new(
            OpenAiProvider::builder()
                .endpoint("http://127.0.0.1:9/v1")
                .model("test-model")
                .name("test")
                .build()
                .expect("provider builds"),
        );
        let agent = Agent::new(provider, ToolRegistry::new()).with_store(store.clone());
        Dispatcher::new(StdArc::new(agent), store)
    }

    #[test]
    fn reactions_enabled_default_and_env() {
        let _guard = crate::models_dev::test_env_lock();
        std::env::remove_var("FEISHU_REACTIONS");
        assert!(reactions_enabled());
        std::env::set_var("FEISHU_REACTIONS", "false");
        assert!(!reactions_enabled());
        std::env::set_var("FEISHU_REACTIONS", "0");
        assert!(!reactions_enabled());
        std::env::set_var("FEISHU_REACTIONS", "true");
        assert!(reactions_enabled());
        std::env::remove_var("FEISHU_REACTIONS");
    }

    #[test]
    fn reaction_action_mapping() {
        assert_eq!(
            reaction_action_from_event_type("im.message.reaction.created_v1"),
            "added"
        );
        assert_eq!(
            reaction_action_from_event_type("im.message.reaction.deleted_v1"),
            "removed"
        );
    }

    #[test]
    fn reaction_operator_drop_rules() {
        // hermes loop breaker: bot/app-origin reactions are dropped,
        // human reaction operators pass.
        assert!(should_drop_reaction_operator("bot"));
        assert!(should_drop_reaction_operator("app"));
        assert!(!should_drop_reaction_operator("user"));
        assert!(!should_drop_reaction_operator(""));
    }

    #[test]
    fn reaction_synthetic_text_format() {
        assert_eq!(
            reaction_synthetic_text("added", "ThumbsUp"),
            "reaction:added:ThumbsUp"
        );
        assert_eq!(
            reaction_synthetic_text("removed", "Typing"),
            "reaction:removed:Typing"
        );
    }

    #[test]
    fn exec_approval_card_hermes_layout() {
        let card = build_exec_approval_card(
            "rm -rf /tmp/x",
            "dangerous command",
            "platform-feishu-oc_1",
            true,
            true,
            false,
        );
        assert_eq!(
            card["header"]["title"]["content"],
            "⚠️ Command Approval Required"
        );
        assert_eq!(card["header"]["template"], "orange");
        let markdown = card["elements"][0]["content"].as_str().unwrap();
        assert!(markdown.contains("```\nrm -rf /tmp/x\n```"));
        assert!(markdown.contains("**Reason:** dangerous command"));
        let actions = card["elements"][1]["actions"].as_array().unwrap();
        assert_eq!(actions.len(), 4);
        assert_eq!(actions[0]["text"]["content"], "✅ Allow Once");
        assert_eq!(actions[0]["type"], "primary");
        assert_eq!(
            actions[0]["value"]["ulnclaw_approval"],
            "approve:platform-feishu-oc_1:allow-once"
        );
        assert_eq!(actions[1]["text"]["content"], "✅ Session");
        assert_eq!(
            actions[1]["value"]["ulnclaw_approval"],
            "approve:platform-feishu-oc_1:allow-session"
        );
        assert_eq!(actions[2]["text"]["content"], "✅ Always");
        assert_eq!(actions[3]["text"]["content"], "❌ Deny");
        assert_eq!(actions[3]["type"], "danger");
    }

    #[test]
    fn exec_approval_card_smart_deny_hides_session_tiers() {
        let card = build_exec_approval_card("cmd", "d", "platform-feishu-oc_1", true, true, true);
        let actions = card["elements"][1]["actions"].as_array().unwrap();
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0]["text"]["content"], "✅ Allow Once");
        assert_eq!(actions[1]["text"]["content"], "❌ Deny");
        let markdown = card["elements"][0]["content"].as_str().unwrap();
        assert!(markdown.contains("**Smart DENY:**"));
        // allow_session=false also collapses the tiers.
        let card = build_exec_approval_card("cmd", "d", "platform-feishu-oc_1", true, false, false);
        assert_eq!(card["elements"][1]["actions"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn format_exec_approval_markdown_truncates_long_commands() {
        let command = "x".repeat(3001);
        let markdown = format_exec_approval_markdown(&command, "d", false);
        assert!(markdown.contains(&format!("{}...\n```", "x".repeat(3000))));
        assert!(!markdown.contains(&"x".repeat(3001)));
    }

    #[test]
    fn update_prompt_card_layout() {
        let card = build_update_prompt_card("Upgrade now?", "yes");
        assert_eq!(
            card["header"]["title"]["content"],
            "⚕ Update Needs Your Input"
        );
        let markdown = card["elements"][0]["content"].as_str().unwrap();
        assert!(markdown.contains("Upgrade now?"));
        assert!(markdown.contains("Default: `yes`"));
        let actions = card["elements"][1]["actions"].as_array().unwrap();
        assert_eq!(
            actions[0]["value"]["ulnclaw_update_prompt"],
            "update_prompt:y"
        );
        assert_eq!(
            actions[1]["value"]["ulnclaw_update_prompt"],
            "update_prompt:n"
        );
        assert_eq!(actions[1]["type"], "danger");
    }

    #[test]
    fn resolved_cards_layout() {
        let approved = build_resolved_approval_card("always", "ou_admin");
        assert_eq!(
            approved["header"]["title"]["content"],
            "✅ Approved permanently"
        );
        assert_eq!(approved["header"]["template"], "green");
        assert_eq!(
            approved["elements"][0]["content"],
            "✅ **Approved permanently** by ou_admin"
        );
        let denied = build_resolved_approval_card("deny", "ou_admin");
        assert_eq!(denied["header"]["title"]["content"], "❌ Denied");
        assert_eq!(denied["header"]["template"], "red");
        let answered = build_resolved_update_prompt_card("y", "ou_admin");
        assert_eq!(
            answered["header"]["title"]["content"],
            "✅ Update prompt answered: Yes"
        );
        assert_eq!(
            answered["elements"][0]["content"],
            "Answered by **ou_admin**"
        );
    }

    #[test]
    fn card_action_authorization_gate() {
        let temp = tempfile::tempdir().unwrap();
        let store = crate::pairing::PairingStore::open(temp.path());
        let cfg = FeishuConfig {
            allowed_users: vec!["ou_admin".into()],
            ..Default::default()
        };
        let session = "platform-feishu-oc_1";
        // Allowlisted operator passes; chat mismatch and strangers fail.
        assert!(card_action_authorized(
            &cfg,
            Some(&store),
            "ou_admin",
            session,
            "oc_1"
        ));
        assert!(card_action_authorized(
            &cfg,
            Some(&store),
            "ou_admin",
            session,
            ""
        ));
        assert!(!card_action_authorized(
            &cfg,
            Some(&store),
            "ou_admin",
            session,
            "oc_other"
        ));
        assert!(!card_action_authorized(
            &cfg,
            Some(&store),
            "ou_stranger",
            session,
            "oc_1"
        ));
        assert!(!card_action_authorized(
            &cfg,
            Some(&store),
            "",
            session,
            "oc_1"
        ));
        // Wildcard allowlist.
        let wildcard = FeishuConfig {
            allowed_users: vec!["*".into()],
            ..Default::default()
        };
        assert!(card_action_authorized(
            &wildcard,
            Some(&store),
            "ou_anyone",
            session,
            "oc_1"
        ));
        // Pairing approval passes the gate.
        let code = store
            .generate_code("feishu", "ou_paired", "Paired")
            .unwrap();
        store.approve_code("feishu", &code);
        assert!(card_action_authorized(
            &cfg,
            Some(&store),
            "ou_paired",
            session,
            "oc_1"
        ));
    }

    #[tokio::test]
    async fn webhook_card_action_resolves_approval() {
        let cfg = FeishuConfig {
            allowed_users: vec!["ou_admin".into()],
            ..Default::default()
        };
        let session = "platform-feishu-oc_cardtest";
        let _handle = crate::approval_gateway::register(session, "ls", "d", false, true, true);
        assert_eq!(crate::approval_gateway::pending_count(session), 1);
        let body = serde_json::to_vec(&json!({
            "operator": {"open_id": "ou_admin"},
            "context": {"open_chat_id": "oc_cardtest"},
            "action": {
                "tag": "button",
                "value": {"ulnclaw_approval": format!("approve:{session}:allow-once")},
            },
        }))
        .unwrap();
        let resp = feishu_handle_webhook(&cfg, &dummy_dispatcher().await, None, &body, &[]).await;
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["card"]["type"], "raw");
        assert_eq!(
            resp.body["card"]["data"]["header"]["title"]["content"],
            "✅ Approved once"
        );
        assert_eq!(crate::approval_gateway::pending_count(session), 0);
    }

    #[tokio::test]
    async fn webhook_unauthorized_card_action_leaves_approval_pending() {
        let cfg = FeishuConfig {
            allowed_users: vec!["ou_admin".into()],
            ..Default::default()
        };
        let session = "platform-feishu-oc_cardtest2";
        let _handle = crate::approval_gateway::register(session, "ls", "d", false, true, true);
        let body = serde_json::to_vec(&json!({
            "operator": {"open_id": "ou_intruder"},
            "context": {"open_chat_id": "oc_cardtest2"},
            "action": {
                "tag": "button",
                "value": {"ulnclaw_approval": format!("approve:{session}:deny")},
            },
        }))
        .unwrap();
        let resp = feishu_handle_webhook(&cfg, &dummy_dispatcher().await, None, &body, &[]).await;
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, json!({}));
        assert_eq!(crate::approval_gateway::pending_count(session), 1);
        // Clean up the pending entry so later tests start fresh.
        crate::approval_gateway::resolve(session, crate::approval_gateway::CHOICE_DENY);
    }

    #[tokio::test]
    async fn webhook_read_receipt_ignored() {
        let cfg = FeishuConfig::default();
        let body =
            br#"{"header":{"event_type":"im.message.message_read_v1","event_id":"r1"},"event":{}}"#;
        let resp = feishu_handle_webhook(&cfg, &dummy_dispatcher().await, None, body, &[]).await;
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["code"], 0);
    }

    #[test]
    fn card_action_token_dedup_window() {
        let token = format!("p210-token-{}", uuid::Uuid::new_v4().simple());
        assert!(!card_action_token_seen(&token));
        assert!(card_action_token_seen(&token));
        let other = format!("p210-other-{}", uuid::Uuid::new_v4().simple());
        assert!(!card_action_token_seen(&other));
        // Empty tokens are never deduped (hermes `if not token: return False`).
        assert!(!card_action_token_seen(""));
        assert!(!card_action_token_seen(""));
    }

    #[test]
    fn card_action_synthetic_text_formatting() {
        // No value → bare command.
        assert_eq!(
            card_action_synthetic_text("button", &json!({})),
            "/card button"
        );
        // Non-empty value rides as compact JSON; UTF-8 stays raw
        // (ensure_ascii=False parity); serde_json keys sort.
        let value = json!({"choice": "选项A", "action": "go"});
        assert_eq!(
            card_action_synthetic_text("button", &value),
            "/card button {\"action\":\"go\",\"choice\":\"选项A\"}"
        );
        assert_eq!(
            card_action_synthetic_text("select_static", &json!({})),
            "/card select_static"
        );
    }

    // multi_thread flavor: the routing path builds the process-wide API
    // handle via a blocking lock, which needs block_in_place support.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn card_action_command_routing_gates() {
        let dispatcher = dummy_dispatcher().await;
        // Unauthorized operator (no allowlist, no pairing) → {} and no dispatch.
        let cfg = FeishuConfig::default();
        let payload = json!({
            "token": format!("p210-unauth-{}", uuid::Uuid::new_v4().simple()),
            "action": {"tag": "button", "value": {"custom": "x"}},
            "operator": {"open_id": "ou_stranger"},
            "context": {"open_chat_id": "oc_1"},
        });
        assert_eq!(
            handle_card_action(&cfg, &dispatcher, None, &payload).await,
            json!({})
        );
        // Authorized (wildcard) → {} response; the synthetic command
        // dispatch runs detached.
        let wildcard = FeishuConfig {
            allowed_users: vec!["*".into()],
            ..Default::default()
        };
        let payload = json!({
            "token": format!("p210-auth-{}", uuid::Uuid::new_v4().simple()),
            "action": {"tag": "button", "value": {"custom": "x"}},
            "operator": {"open_id": "ou_user"},
            "context": {"open_chat_id": "oc_1"},
        });
        assert_eq!(
            handle_card_action(&wildcard, &dispatcher, None, &payload).await,
            json!({})
        );
        // Duplicate token is dropped (still {}).
        assert_eq!(
            handle_card_action(&wildcard, &dispatcher, None, &payload).await,
            json!({})
        );
        // Missing operator → {} (dropped before gating).
        let incomplete = json!({
            "action": {"tag": "button", "value": {}},
            "operator": {"open_id": ""},
            "context": {"open_chat_id": "oc_1"},
        });
        assert_eq!(
            handle_card_action(&wildcard, &dispatcher, None, &incomplete).await,
            json!({})
        );
    }
}

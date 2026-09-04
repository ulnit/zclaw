//! QQ Bot platform adapter — port of hermes
//! `gateway/platforms/qqbot/` @ v2026.8.3 (adapter.py, constants.py,
//! utils.py, chunked_upload.py essentials).
//!
//! Uses the official QQ Bot API v2: a WebSocket gateway connection for
//! inbound events (C2C/group @-mentions/guild channel/guild DM) and the
//! REST API (`api.sgroup.qq.com`) for outbound messages and media.
//! Voice notes prefer QQ's built-in `asr_refer_text` transcript, then
//! fall back to the central `[stt]` pipeline via the cached audio.
//!
//! Inline keyboards are ported (`keyboards.py`): exec-approval prompts
//! render as ✅ 允许一次 / ⭐ 始终允许 / ❌ 拒绝 callback buttons via the
//! `send_exec_approval` contract, and INTERACTION_CREATE clicks route
//! back through `approval_gateway` with operator authorization (hermes
//! `_default_interaction_dispatch`). Update-prompt ✓/✗ keyboards and the
//! atomic `.update_response` answer file are ported as well. Known
//! difference: the DM-policy wizard half of the hermes onboarding flow
//! is not ported (QR scan-to-configure — `onboard.py`, AES-256-GCM
//! bound secrets — IS ported via `ulnclaw qq login`). Guild-DM replies
//! route
//! through `/dms/<guild_id>/messages` (hermes' send path left the `dm`
//! chat type unrouted).

use crate::messaging::{Dispatcher, MediaAttachment, MessageEvent};
use crate::pairing::PairingStore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// hermes qqbot/constants.py.
const API_BASE: &str = "https://api.sgroup.qq.com";
const TOKEN_URL: &str = "https://bots.qq.com/app/getAppAccessToken";
const GATEWAY_URL_PATH: &str = "/gateway";

const DEFAULT_API_TIMEOUT_SECS: u64 = 30;
const FILE_UPLOAD_TIMEOUT_SECS: u64 = 120;
const CONNECT_TIMEOUT_SECS: u64 = 20;

const RECONNECT_BACKOFF: &[u64] = &[2, 5, 10, 30, 60];
const MAX_RECONNECT_ATTEMPTS: u32 = 100;
const RATE_LIMIT_DELAY_SECS: u64 = 60;
const QUICK_DISCONNECT_THRESHOLD_SECS: f64 = 5.0;
const MAX_QUICK_DISCONNECT_COUNT: u32 = 3;

/// hermes MAX_MESSAGE_LENGTH for qqbot.
const MAX_MESSAGE_LENGTH: usize = 4000;
const DEDUP_WINDOW_SECS: u64 = 300;
const DEDUP_MAX_SIZE: usize = 1000;

const MSG_TYPE_TEXT: i64 = 0;
const MSG_TYPE_MARKDOWN: i64 = 2;
const MSG_TYPE_MEDIA: i64 = 7;

const MEDIA_TYPE_IMAGE: i64 = 1;
const MEDIA_TYPE_VIDEO: i64 = 2;
const MEDIA_TYPE_VOICE: i64 = 3;
const MEDIA_TYPE_FILE: i64 = 4;

/// hermes `keyboards.py` button_data prefixes.
const APPROVAL_BUTTON_PREFIX: &str = "approve:";
const UPDATE_PROMPT_PREFIX: &str = "update_prompt:";
/// hermes `_APPROVAL_TIMEOUT_SECONDS` (matches the gateway default
/// approval timeout).
const APPROVAL_TIMEOUT_SECONDS: u32 = 300;

/// hermes identify intents: C2C/group @-messages + public guild messages
/// + guild direct messages + interactions.
const INTENTS: u64 = (1 << 25) | (1 << 30) | (1 << 12) | (1 << 26);

const BIZ_CODE_DAILY_LIMIT: i64 = 40093002;
const BIZ_CODE_PART_RETRYABLE: i64 = 40093001;
/// First 10,002,432 bytes feed the `md5_10m` hash (QQ API spec).
const MD5_10M_SIZE: usize = 10_002_432;
const PART_UPLOAD_MAX_RETRIES: u32 = 2;
const PART_FINISH_RETRY_INTERVAL_SECS: f64 = 1.0;
const COMPLETE_UPLOAD_MAX_RETRIES: u32 = 2;

fn user_agent() -> String {
    format!(
        "QQBotAdapter/1.1.0 (Rust; {}; ulnclaw/{})",
        std::env::consts::OS,
        env!("CARGO_PKG_VERSION")
    )
}

/// `[messaging.qq]` — official QQ Bot API v2 adapter (hermes
/// `platforms.qq` extra config + QQ_* env vars).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct QQBotConfig {
    pub enabled: bool,
    /// Bot app id (fallback `QQ_APP_ID`).
    pub app_id: String,
    /// Bot client secret (fallback `QQ_CLIENT_SECRET`).
    pub client_secret: String,
    /// Send replies as QQ markdown (msg_type 2); when false, markdown is
    /// stripped and plain text (msg_type 0) is sent.
    pub markdown_support: bool,
    /// DM intake policy: `pairing` (default) | `allowlist` | `open` |
    /// `disabled` (hermes dm_policy).
    pub dm_policy: String,
    /// DM allowlist (`*` wildcard supported).
    pub allow_from: Vec<String>,
    /// Group intake policy: `pairing` (default — groups stay closed;
    /// hermes has no group pairing flow) | `allowlist` | `open` |
    /// `disabled`.
    pub group_policy: String,
    /// Group/guild allowlist (`*` wildcard supported).
    pub group_allow_from: Vec<String>,
}

impl Default for QQBotConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            app_id: String::new(),
            client_secret: String::new(),
            markdown_support: true,
            dm_policy: "pairing".into(),
            allow_from: Vec::new(),
            group_policy: "pairing".into(),
            group_allow_from: Vec::new(),
        }
    }
}

fn env_or_none(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

pub fn resolve_app_id(cfg: &QQBotConfig) -> String {
    let trimmed = cfg.app_id.trim();
    if !trimmed.is_empty() {
        return trimmed.to_string();
    }
    if let Some(value) = env_or_none("QQ_APP_ID") {
        return value;
    }
    // Scan-to-configure credentials persisted by `ulnclaw qq login`.
    load_onboard_credentials(&crate::config::ulnclaw_home())
        .and_then(|v| {
            v.get("app_id")
                .and_then(|id| id.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_default()
}

pub fn resolve_client_secret(cfg: &QQBotConfig) -> String {
    let trimmed = cfg.client_secret.trim();
    if !trimmed.is_empty() {
        return trimmed.to_string();
    }
    if let Some(value) = env_or_none("QQ_CLIENT_SECRET") {
        return value;
    }
    load_onboard_credentials(&crate::config::ulnclaw_home())
        .and_then(|v| {
            v.get("client_secret")
                .and_then(|secret| secret.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_default()
}

fn open_dm_opted_in() -> bool {
    for var in ["GATEWAY_ALLOW_ALL_USERS", "QQ_ALLOW_ALL_USERS"] {
        if matches!(
            std::env::var(var)
                .unwrap_or_default()
                .to_lowercase()
                .as_str(),
            "true" | "1" | "yes"
        ) {
            return true;
        }
    }
    false
}

/// hermes `_entry_matches`: case-insensitive equality with `*` wildcard.
pub fn entry_matches(entries: &[String], target: &str) -> bool {
    let normalized_target = target.trim().to_lowercase();
    entries.iter().any(|entry| {
        let normalized = entry.trim().to_lowercase();
        normalized == "*" || normalized == normalized_target
    })
}

/// hermes `_strip_at_mention`.
pub fn strip_at_mention(content: &str) -> String {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"^@\S+\s*").unwrap());
    re.replace(content.trim(), "").to_string()
}

/// Minimal markdown stripping for `markdown_support = false` sends
/// (hermes `helpers.strip_markdown` essence).
pub fn strip_markdown(content: &str) -> String {
    static BOLD: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static LINK: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static FENCE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static HEADING: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let bold = BOLD.get_or_init(|| regex::Regex::new(r"\*\*([^*]+)\*\*|__([^_]+)__").unwrap());
    let link = LINK.get_or_init(|| regex::Regex::new(r"\[([^\]]+)\]\(([^)]+)\)").unwrap());
    let fence = FENCE.get_or_init(|| regex::Regex::new(r"(?m)^```[^\n]*$").unwrap());
    let heading = HEADING.get_or_init(|| regex::Regex::new(r"(?m)^#{1,6}\s+").unwrap());
    let mut out = bold.replace_all(content, "$1$2").to_string();
    out = link.replace_all(&out, "$1").to_string();
    out = fence.replace_all(&out, "").to_string();
    out = heading.replace_all(&out, "").to_string();
    out.replace('`', "")
}

/// Paragraph-aware chunking at `max` (hermes `truncate_message` essence).
pub fn chunk_reply(content: &str, max: usize) -> Vec<String> {
    if content.chars().count() <= max {
        return vec![content.to_string()];
    }
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    for paragraph in content.split("\n\n") {
        let paragraph = paragraph.trim_matches('\n');
        if paragraph.is_empty() {
            continue;
        }
        if paragraph.chars().count() > max {
            if !current.is_empty() {
                chunks.push(std::mem::take(&mut current));
            }
            let mut piece = String::new();
            let mut piece_len = 0usize;
            for ch in paragraph.chars() {
                if piece_len + 1 > max {
                    chunks.push(std::mem::take(&mut piece));
                    piece_len = 0;
                }
                piece.push(ch);
                piece_len += 1;
            }
            if !piece.is_empty() {
                chunks.push(piece);
            }
            continue;
        }
        let candidate = if current.is_empty() {
            paragraph.to_string()
        } else {
            format!("{current}\n\n{paragraph}")
        };
        if candidate.chars().count() <= max {
            current = candidate;
        } else {
            if !current.is_empty() {
                chunks.push(std::mem::take(&mut current));
            }
            current = paragraph.to_string();
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// hermes `_next_msg_seq`: time^rand within 0..65535.
pub fn next_msg_seq() -> u32 {
    let time_part = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        % 100_000_000) as u32;
    let mut rand_bytes = [0u8; 2];
    getrandom_fill(&mut rand_bytes);
    let rand = u16::from_le_bytes(rand_bytes) as u32;
    (time_part ^ rand) % 65536
}

fn getrandom_fill(bytes: &mut [u8]) {
    if let Ok(mut file) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        if file.read_exact(bytes).is_ok() {
            return;
        }
    }
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0x9E3779B97F4A7C15);
    for slot in bytes.iter_mut() {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *slot = (seed >> 33) as u8;
    }
}

fn md5_hex(data: &[u8]) -> String {
    use md5::Digest;
    format!("{:x}", md5::Md5::digest(data))
}

fn sha1_hex(data: &[u8]) -> String {
    use sha1::Digest;
    format!("{:x}", sha1::Sha1::digest(data))
}

/// hermes `_parse_qq_timestamp`: ISO 8601 string or integer milliseconds.
pub fn parse_qq_timestamp(raw: &str) -> chrono::DateTime<chrono::Utc> {
    if raw.is_empty() {
        return chrono::Utc::now();
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(raw) {
        return dt.with_timezone(&chrono::Utc);
    }
    if let Ok(ms) = raw.parse::<i64>() {
        if let Some(dt) = chrono::DateTime::from_timestamp_millis(ms) {
            return dt;
        }
    }
    chrono::Utc::now()
}

// ---------------------------------------------------------------------------
// Shared adapter handle: token cache + REST + outbound
// ---------------------------------------------------------------------------

struct TokenCache {
    token: String,
    expires_at: Instant,
}

pub struct QQHandle {
    client: reqwest::Client,
    app_id: String,
    client_secret: String,
    markdown_support: bool,
    token: Mutex<Option<TokenCache>>,
    chat_type_map: Mutex<HashMap<String, String>>,
    last_msg_id: Mutex<HashMap<String, String>>,
    connected: AtomicBool,
}

impl QQHandle {
    /// hermes `_ensure_token` (singleflight via the request-side Mutex).
    pub async fn ensure_token(&self) -> std::result::Result<String, String> {
        {
            let cache = self.token.lock().unwrap();
            if let Some(cache) = cache.as_ref() {
                if Instant::now() + Duration::from_secs(60) < cache.expires_at {
                    return Ok(cache.token.clone());
                }
            }
        }
        let response = self
            .client
            .post(TOKEN_URL)
            .timeout(Duration::from_secs(DEFAULT_API_TIMEOUT_SECS))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header("User-Agent", user_agent())
            .json(&json!({ "appId": self.app_id, "clientSecret": self.client_secret }))
            .send()
            .await
            .map_err(|e| format!("Failed to get QQ Bot access token: {e}"))?;
        let status = response.status();
        let data: Value = response
            .json()
            .await
            .map_err(|e| format!("QQ token response: {e}"))?;
        if !status.is_success() {
            return Err(format!("QQ token HTTP {}: {data}", status.as_u16()));
        }
        let token = data
            .get("access_token")
            .and_then(|v| v.as_str())
            .filter(|t| !t.is_empty())
            .ok_or_else(|| format!("QQ Bot token response missing access_token: {data}"))?
            .to_string();
        let expires_in = data
            .get("expires_in")
            .and_then(|v| v.as_u64())
            .unwrap_or(7200);
        self.token.lock().unwrap().replace(TokenCache {
            token: token.clone(),
            expires_at: Instant::now() + Duration::from_secs(expires_in),
        });
        Ok(token)
    }

    /// hermes `_api_request`.
    pub async fn api_request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
        timeout: Duration,
    ) -> std::result::Result<Value, String> {
        let token = self.ensure_token().await?;
        let url = format!("{API_BASE}{path}");
        let mut request = self
            .client
            .request(method, &url)
            .timeout(timeout)
            .header("Authorization", format!("QQBot {token}"))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header("User-Agent", user_agent());
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request
            .send()
            .await
            .map_err(|e| format!("QQ Bot API timeout [{path}]: {e}"))?;
        let status = response.status();
        let data: Value = response.json().await.unwrap_or(Value::Null);
        if status.as_u16() >= 400 {
            let message = data
                .get("message")
                .map(|m| m.to_string())
                .unwrap_or_else(|| data.to_string());
            return Err(format!(
                "QQ Bot API error [{}] {path}: {message}",
                status.as_u16()
            ));
        }
        Ok(data)
    }

    /// hermes `_get_gateway_url`.
    async fn gateway_url(&self) -> std::result::Result<String, String> {
        let token = self.ensure_token().await?;
        let response = self
            .client
            .get(format!("{API_BASE}{GATEWAY_URL_PATH}"))
            .timeout(Duration::from_secs(DEFAULT_API_TIMEOUT_SECS))
            .header("Authorization", format!("QQBot {token}"))
            .header("User-Agent", user_agent())
            .send()
            .await
            .map_err(|e| format!("Failed to get QQ Bot gateway URL: {e}"))?;
        let data: Value = response.json().await.map_err(|e| e.to_string())?;
        data.get("url")
            .and_then(|v| v.as_str())
            .filter(|u| !u.is_empty())
            .map(|u| u.to_string())
            .ok_or_else(|| format!("QQ Bot gateway response missing url: {data}"))
    }

    fn chat_type(&self, chat_id: &str) -> String {
        self.chat_type_map
            .lock()
            .unwrap()
            .get(chat_id)
            .cloned()
            .unwrap_or_else(|| "c2c".into())
    }

    /// hermes `format_message`.
    fn format_message(&self, content: &str) -> String {
        if self.markdown_support {
            content.to_string()
        } else {
            strip_markdown(content)
        }
    }

    /// hermes `send()`: format + chunk + per-chunk retry.
    pub async fn send_text(&self, chat_id: &str, content: &str) {
        if content.trim().is_empty() {
            return;
        }
        let formatted = self.format_message(content);
        let reply_to = self.last_msg_id.lock().unwrap().get(chat_id).cloned();
        for (idx, chunk) in chunk_reply(&formatted, MAX_MESSAGE_LENGTH)
            .iter()
            .enumerate()
        {
            // Only the first chunk rides the passive reply.
            let reply = if idx == 0 { reply_to.clone() } else { None };
            if let Err(e) = self.send_chunk(chat_id, chunk, reply.as_deref()).await {
                eprintln!("[qqbot] send failed: {e}");
                return;
            }
        }
    }

    /// hermes `_send_chunk` with retry + exponential backoff.
    async fn send_chunk(
        &self,
        chat_id: &str,
        content: &str,
        reply_to: Option<&str>,
    ) -> std::result::Result<(), String> {
        let chat_type = self.chat_type(chat_id);
        let mut last_error = String::from("Unknown error");
        for attempt in 0..3u32 {
            let result = match chat_type.as_str() {
                "c2c" => self.send_c2c_text(chat_id, content, reply_to).await,
                "group" => self.send_group_text(chat_id, content, reply_to).await,
                "guild" => self.send_guild_text(chat_id, content, reply_to).await,
                "dm" => self.send_dm_text(chat_id, content, reply_to).await,
                other => return Err(format!("Unknown chat type {other} for {chat_id}")),
            };
            match result {
                Ok(()) => return Ok(()),
                Err(e) => {
                    let lower = e.to_lowercase();
                    if ["invalid", "forbidden", "not found", "bad request"]
                        .iter()
                        .any(|k| lower.contains(k))
                    {
                        return Err(e);
                    }
                    last_error = e;
                    if attempt < 2 {
                        let delay = 1.0f64 * 2f64.powi(attempt as i32);
                        eprintln!(
                            "[qqbot] send retry {}/3 after {delay:.1}s: {last_error}",
                            attempt + 1
                        );
                        tokio::time::sleep(Duration::from_secs_f64(delay)).await;
                    }
                }
            }
        }
        Err(last_error)
    }

    /// hermes `_build_text_body` + `_send_c2c_text`.
    async fn send_c2c_text(
        &self,
        openid: &str,
        content: &str,
        reply_to: Option<&str>,
    ) -> std::result::Result<(), String> {
        let body = self.build_text_body(content, reply_to);
        self.api_request(
            reqwest::Method::POST,
            &format!("/v2/users/{openid}/messages"),
            Some(&body),
            Duration::from_secs(DEFAULT_API_TIMEOUT_SECS),
        )
        .await?;
        Ok(())
    }

    async fn send_group_text(
        &self,
        group_openid: &str,
        content: &str,
        reply_to: Option<&str>,
    ) -> std::result::Result<(), String> {
        let body = self.build_text_body(content, reply_to);
        self.api_request(
            reqwest::Method::POST,
            &format!("/v2/groups/{group_openid}/messages"),
            Some(&body),
            Duration::from_secs(DEFAULT_API_TIMEOUT_SECS),
        )
        .await?;
        Ok(())
    }

    async fn send_guild_text(
        &self,
        channel_id: &str,
        content: &str,
        reply_to: Option<&str>,
    ) -> std::result::Result<(), String> {
        let truncated: String = content.chars().take(MAX_MESSAGE_LENGTH).collect();
        let mut body = json!({ "content": truncated });
        if let Some(reply_to) = reply_to {
            body["msg_id"] = json!(reply_to);
        }
        self.api_request(
            reqwest::Method::POST,
            &format!("/channels/{channel_id}/messages"),
            Some(&body),
            Duration::from_secs(DEFAULT_API_TIMEOUT_SECS),
        )
        .await?;
        Ok(())
    }

    /// Guild-DM reply via `/dms/<guild_id>/messages` (hermes left the
    /// `dm` chat type unrouted; ulnclaw completes the path).
    async fn send_dm_text(
        &self,
        guild_id: &str,
        content: &str,
        reply_to: Option<&str>,
    ) -> std::result::Result<(), String> {
        let truncated: String = content.chars().take(MAX_MESSAGE_LENGTH).collect();
        let mut body = json!({ "content": truncated });
        if let Some(reply_to) = reply_to {
            body["msg_id"] = json!(reply_to);
        }
        self.api_request(
            reqwest::Method::POST,
            &format!("/dms/{guild_id}/messages"),
            Some(&body),
            Duration::from_secs(DEFAULT_API_TIMEOUT_SECS),
        )
        .await?;
        Ok(())
    }

    fn build_text_body(&self, content: &str, reply_to: Option<&str>) -> Value {
        let truncated: String = content.chars().take(MAX_MESSAGE_LENGTH).collect();
        let msg_seq = next_msg_seq();
        let mut body = if self.markdown_support {
            json!({
                "markdown": { "content": truncated },
                "msg_type": MSG_TYPE_MARKDOWN,
                "msg_seq": msg_seq,
            })
        } else {
            json!({
                "content": truncated,
                "msg_type": MSG_TYPE_TEXT,
                "msg_seq": msg_seq,
            })
        };
        if let Some(reply_to) = reply_to {
            body["msg_id"] = json!(reply_to);
            if !self.markdown_support {
                body["message_reference"] = json!({ "message_id": reply_to });
            }
        }
        body
    }

    /// hermes `send_with_keyboard`: one message with an inline keyboard,
    /// never chunked (a keyboard message has exactly one interactive
    /// surface). C2C/group only — guild/dm chats have no keyboard
    /// support.
    pub async fn send_text_with_keyboard(
        &self,
        chat_id: &str,
        content: &str,
        keyboard: &Value,
    ) -> std::result::Result<(), String> {
        let chat_type = self.chat_type(chat_id);
        if chat_type != "c2c" && chat_type != "group" {
            return Err(format!(
                "inline keyboards not supported for chat_type {chat_type:?}"
            ));
        }
        let formatted = self.format_message(content);
        let reply_to = self.last_msg_id.lock().unwrap().get(chat_id).cloned();
        let mut body = self.build_text_body(&formatted, reply_to.as_deref());
        body["keyboard"] = keyboard.clone();
        let path = if chat_type == "c2c" {
            format!("/v2/users/{chat_id}/messages")
        } else {
            format!("/v2/groups/{chat_id}/messages")
        };
        self.api_request(
            reqwest::Method::POST,
            &path,
            Some(&body),
            Duration::from_secs(DEFAULT_API_TIMEOUT_SECS),
        )
        .await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Inline keyboards (hermes `keyboards.py`)
// ---------------------------------------------------------------------------

/// hermes `_make_callback_button`.
fn callback_button(
    btn_id: &str,
    label: &str,
    visited_label: &str,
    data: &str,
    style: u8,
    group_id: &str,
) -> Value {
    json!({
        "id": btn_id,
        "render_data": {
            "label": label,
            "visited_label": visited_label,
            "style": style,
        },
        "action": {
            "type": 1,
            "data": data,
            "permission": {"type": 2},
            "click_limit": 1,
        },
        "group_id": group_id,
    })
}

/// hermes `build_approval_keyboard`: `[✅ 允许一次] [⭐ 始终允许] [❌ 拒绝]`,
/// all sharing `group_id = "approval"` so one click greys the rest. The
/// persistent-scope button hides when unavailable.
pub fn build_approval_keyboard(session_key: &str, allow_permanent: bool) -> Value {
    let mut buttons = vec![callback_button(
        "allow",
        "✅ 允许一次",
        "已允许",
        &format!("{APPROVAL_BUTTON_PREFIX}{session_key}:allow-once"),
        1,
        "approval",
    )];
    if allow_permanent {
        buttons.push(callback_button(
            "always",
            "⭐ 始终允许",
            "已始终允许",
            &format!("{APPROVAL_BUTTON_PREFIX}{session_key}:allow-always"),
            1,
            "approval",
        ));
    }
    buttons.push(callback_button(
        "deny",
        "❌ 拒绝",
        "已拒绝",
        &format!("{APPROVAL_BUTTON_PREFIX}{session_key}:deny"),
        0,
        "approval",
    ));
    json!({"content": {"rows": [{"buttons": buttons}]}})
}

/// hermes `build_update_prompt_keyboard`: `[✓ 确认] [✗ 取消]`.
pub fn build_update_prompt_keyboard() -> Value {
    json!({"content": {"rows": [{"buttons": [
        callback_button(
            "yes",
            "✓ 确认",
            "已确认",
            &format!("{UPDATE_PROMPT_PREFIX}y"),
            1,
            "update_prompt",
        ),
        callback_button(
            "no",
            "✗ 取消",
            "已取消",
            &format!("{UPDATE_PROMPT_PREFIX}n"),
            0,
            "update_prompt",
        ),
    ]}]}})
}

/// hermes `parse_approval_button_data` — the session key may itself
/// contain colons, so the middle group is greedy up to the decision.
pub fn parse_approval_button_data(button_data: &str) -> Option<(String, String)> {
    let re = regex::Regex::new(r"^approve:(.+):(allow-once|allow-always|deny)$").unwrap();
    let caps = re.captures(button_data)?;
    Some((caps[1].to_string(), caps[2].to_string()))
}

/// hermes `parse_update_prompt_button_data` → `'y'` or `'n'`.
pub fn parse_update_prompt_button_data(button_data: &str) -> Option<String> {
    let re = regex::Regex::new(r"^update_prompt:(y|n)$").unwrap();
    let caps = re.captures(button_data)?;
    Some(caps[1].to_string())
}

/// hermes `_build_exec_text` (no-cwd variant used by
/// `send_exec_approval`): markdown body under the approval keyboard.
pub fn build_exec_approval_text(
    command: &str,
    title: &str,
    description: &str,
    timeout_sec: u32,
) -> String {
    let mut lines: Vec<String> = vec!["🔐 **命令执行审批**".to_string(), String::new()];
    if !command.is_empty() {
        let preview: String = command.chars().take(300).collect();
        lines.push(format!("```\n{preview}\n```"));
    }
    if !title.is_empty() && title != command {
        lines.push(format!("📋 {title}"));
    }
    if !description.is_empty() {
        lines.push(format!("📝 {description}"));
    }
    lines.push(String::new());
    lines.push(format!("⏱️ 超时: {timeout_sec} 秒"));
    lines.join("\n")
}

// ---------------------------------------------------------------------------
// Media upload (hermes `_upload_media` + `chunked_upload.py`)
// ---------------------------------------------------------------------------

impl QQHandle {
    /// Simple upload: platform fetches `url`, or inline base64 `file_data`
    /// (hermes `_upload_media`, transient-failure retry).
    async fn upload_media(
        &self,
        chat_type: &str,
        target_id: &str,
        file_type: i64,
        url: Option<&str>,
        file_data_b64: Option<&str>,
        file_name: Option<&str>,
    ) -> std::result::Result<Value, String> {
        let path = if chat_type == "c2c" {
            format!("/v2/users/{target_id}/files")
        } else {
            format!("/v2/groups/{target_id}/files")
        };
        let mut body = json!({ "file_type": file_type, "srv_send_msg": false });
        if let Some(url) = url {
            body["url"] = json!(url);
        } else if let Some(data) = file_data_b64 {
            body["file_data"] = json!(data);
        }
        if file_type == MEDIA_TYPE_FILE {
            if let Some(file_name) = file_name {
                body["file_name"] = json!(file_name);
            }
        }
        let mut last_error = String::new();
        for attempt in 0..3u32 {
            match self
                .api_request(
                    reqwest::Method::POST,
                    &path,
                    Some(&body),
                    Duration::from_secs(FILE_UPLOAD_TIMEOUT_SECS),
                )
                .await
            {
                Ok(data) => return Ok(data),
                Err(e) => {
                    let permanent = ["400", "401", "Invalid", "timeout", "Timeout"]
                        .iter()
                        .any(|k| e.contains(k));
                    if permanent {
                        return Err(e);
                    }
                    last_error = e;
                    if attempt < 2 {
                        tokio::time::sleep(Duration::from_secs_f64(1.5 * (attempt + 1) as f64))
                            .await;
                    }
                }
            }
        }
        Err(last_error)
    }

    /// hermes `_send_media` for local files: chunked upload for big files,
    /// inline base64 under the ~10 MB API cap, then a msg_type 7 message.
    pub async fn send_media_file(
        &self,
        chat_id: &str,
        path: &Path,
        caption: &str,
    ) -> std::result::Result<(), String> {
        let chat_type = self.chat_type(chat_id);
        if chat_type == "guild" || chat_type == "dm" {
            return Err("Guild channels don't support native media upload via this path".into());
        }
        let metadata = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let file_size = metadata.len() as usize;
        let file_type = media_type_for_path(path);
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "media".into());

        // Inline base64 under the cap keeps small files off the chunked path.
        const INLINE_CAP: usize = 8 * 1024 * 1024;
        let upload = if file_size <= INLINE_CAP {
            let data = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
            use base64::Engine;
            let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
            self.upload_media(
                &chat_type,
                chat_id,
                file_type,
                None,
                Some(&b64),
                if file_type == MEDIA_TYPE_FILE {
                    Some(file_name.as_str())
                } else {
                    None
                },
            )
            .await?
        } else {
            let upload_id = self
                .chunked_upload(&chat_type, chat_id, file_type, &file_name, path, file_size)
                .await?;
            self.complete_upload(&chat_type, chat_id, &upload_id)
                .await?
        };

        let file_info = upload
            .get("file_info")
            .or_else(|| upload.get("data").and_then(|d| d.get("file_info")))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("Upload returned no file_info: {upload}"))?
            .to_string();

        let mut body = json!({
            "msg_type": MSG_TYPE_MEDIA,
            "media": { "file_info": file_info },
            "msg_seq": next_msg_seq(),
        });
        if !caption.trim().is_empty() {
            let truncated: String = caption.chars().take(MAX_MESSAGE_LENGTH).collect();
            body["content"] = json!(truncated);
        }
        let endpoint = if chat_type == "c2c" {
            format!("/v2/users/{chat_id}/messages")
        } else {
            format!("/v2/groups/{chat_id}/messages")
        };
        self.api_request(
            reqwest::Method::POST,
            &endpoint,
            Some(&body),
            Duration::from_secs(DEFAULT_API_TIMEOUT_SECS),
        )
        .await?;
        Ok(())
    }

    /// Deliver an outbound `MEDIA:` path, extension-routed (image/voice/
    /// video/file).
    pub async fn deliver_media_path(&self, chat_id: &str, path: &Path) {
        if let Err(e) = self.send_media_file(chat_id, path, "").await {
            eprintln!("[qqbot] media delivery failed for {}: {e}", path.display());
        }
    }

    /// Steps 1-2 of the chunked flow: upload_prepare → per-part PUT +
    /// upload_part_finish. Returns the upload_id.
    async fn chunked_upload(
        &self,
        chat_type: &str,
        target_id: &str,
        file_type: i64,
        file_name: &str,
        path: &Path,
        file_size: usize,
    ) -> std::result::Result<String, String> {
        let data = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let md5 = md5_hex(&data);
        let sha1 = sha1_hex(&data);
        let md5_10m = md5_hex(&data[..file_size.min(MD5_10M_SIZE)]);

        let base = if chat_type == "c2c" {
            "/v2/users"
        } else {
            "/v2/groups"
        };
        let prepare_body = json!({
            "file_type": file_type,
            "file_name": file_name,
            "file_size": file_size,
            "md5": md5,
            "sha1": sha1,
            "md5_10m": md5_10m,
        });
        let raw = self
            .api_request(
                reqwest::Method::POST,
                &format!("{base}/{target_id}/upload_prepare"),
                Some(&prepare_body),
                Duration::from_secs(FILE_UPLOAD_TIMEOUT_SECS),
            )
            .await
            .map_err(|e| {
                if e.contains(&BIZ_CODE_DAILY_LIMIT.to_string()) {
                    format!(
                        "QQ daily upload limit exceeded for {file_name:?}. Retry tomorrow. ({e})"
                    )
                } else {
                    e
                }
            })?;

        let src = if raw.get("data").map(|d| d.is_object()).unwrap_or(false) {
            raw.get("data").unwrap()
        } else {
            &raw
        };
        let upload_id = src
            .get("upload_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                format!(
                    "upload_prepare response missing upload_id: {}",
                    truncate_str(&raw.to_string(), 200)
                )
            })?
            .to_string();
        let block_size = src.get("block_size").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let retry_timeout = src
            .get("retry_timeout")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let raw_parts = src
            .get("parts")
            .or_else(|| src.get("part_list"))
            .and_then(|v| v.as_array())
            .ok_or_else(|| {
                format!(
                    "upload_prepare response missing parts: {}",
                    truncate_str(&raw.to_string(), 200)
                )
            })?;

        for part in raw_parts {
            let part_index = part
                .get("part_index")
                .or_else(|| part.get("index"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;
            let presigned_url = part
                .get("presigned_url")
                .or_else(|| part.get("url"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if presigned_url.is_empty() {
                return Err(format!(
                    "upload_prepare part {part_index} missing presigned_url"
                ));
            }
            let offset = part_index * block_size;
            let length = block_size.min(file_size.saturating_sub(offset));
            let chunk = &data[offset..offset + length];
            let part_md5 = md5_hex(chunk);

            self.put_presigned(&presigned_url, chunk, part_index)
                .await?;
            self.part_finish(
                base,
                target_id,
                &upload_id,
                part_index,
                length,
                &part_md5,
                retry_timeout,
            )
            .await?;
        }
        Ok(upload_id)
    }

    async fn put_presigned(
        &self,
        url: &str,
        data: &[u8],
        part_index: usize,
    ) -> std::result::Result<(), String> {
        let mut last_error = String::new();
        for attempt in 0..=PART_UPLOAD_MAX_RETRIES {
            let result = self
                .client
                .put(url)
                .timeout(Duration::from_secs(300))
                .header("Content-Type", "application/octet-stream")
                .body(data.to_vec())
                .send()
                .await;
            match result {
                Ok(response) if response.status().is_success() => return Ok(()),
                Ok(response) => {
                    last_error = format!(
                        "COS PUT part {part_index} HTTP {}",
                        response.status().as_u16()
                    )
                }
                Err(e) => last_error = format!("COS PUT part {part_index}: {e}"),
            }
            if attempt < PART_UPLOAD_MAX_RETRIES {
                tokio::time::sleep(Duration::from_secs(2 * (attempt + 1) as u64)).await;
            }
        }
        Err(last_error)
    }

    /// hermes `_part_finish_with_retry` (biz_code 40093001 retry loop).
    async fn part_finish(
        &self,
        base: &str,
        target_id: &str,
        upload_id: &str,
        part_index: usize,
        block_size: usize,
        md5: &str,
        retry_timeout: f64,
    ) -> std::result::Result<(), String> {
        let body = json!({
            "upload_id": upload_id,
            "part_index": part_index,
            "block_size": block_size,
            "md5": md5,
        });
        let start = Instant::now();
        loop {
            match self
                .api_request(
                    reqwest::Method::POST,
                    &format!("{base}/{target_id}/upload_part_finish"),
                    Some(&body),
                    Duration::from_secs(FILE_UPLOAD_TIMEOUT_SECS),
                )
                .await
            {
                Ok(_) => return Ok(()),
                Err(e) => {
                    if !e.contains(&BIZ_CODE_PART_RETRYABLE.to_string()) {
                        return Err(e);
                    }
                    if retry_timeout > 0.0 && start.elapsed().as_secs_f64() >= retry_timeout {
                        return Err(format!("upload_part_finish persistent retry timed out after {retry_timeout:.0}s: {e}"));
                    }
                    if retry_timeout <= 0.0
                        && start.elapsed() > Duration::from_secs(_PART_FINISH_LOCAL_CAP_SECS)
                    {
                        return Err(format!("upload_part_finish retry cap reached: {e}"));
                    }
                    tokio::time::sleep(Duration::from_secs_f64(PART_FINISH_RETRY_INTERVAL_SECS))
                        .await;
                }
            }
        }
    }

    /// hermes `_complete_upload` (exponential-backoff retry).
    async fn complete_upload(
        &self,
        chat_type: &str,
        target_id: &str,
        upload_id: &str,
    ) -> std::result::Result<Value, String> {
        let base = if chat_type == "c2c" {
            "/v2/users"
        } else {
            "/v2/groups"
        };
        let body = json!({ "upload_id": upload_id });
        let mut last_error = String::new();
        for attempt in 0..=COMPLETE_UPLOAD_MAX_RETRIES {
            match self
                .api_request(
                    reqwest::Method::POST,
                    &format!("{base}/{target_id}/files"),
                    Some(&body),
                    Duration::from_secs(FILE_UPLOAD_TIMEOUT_SECS),
                )
                .await
            {
                Ok(data) => return Ok(data),
                Err(e) => {
                    last_error = e;
                    if attempt < COMPLETE_UPLOAD_MAX_RETRIES {
                        let delay = 2.0f64 * 2f64.powi(attempt as i32);
                        tokio::time::sleep(Duration::from_secs_f64(delay)).await;
                    }
                }
            }
        }
        Err(format!(
            "complete_upload failed after {} attempts: {last_error}",
            COMPLETE_UPLOAD_MAX_RETRIES + 1
        ))
    }
}

/// Local fallback cap when the server omits `retry_timeout` (hermes uses
/// the server value; 120s mirrors `_PART_FINISH_DEFAULT_TIMEOUT`).
const _PART_FINISH_LOCAL_CAP_SECS: u64 = 120;

fn media_type_for_path(path: &Path) -> i64 {
    let mime = crate::media_cache::mime_for_ext(path);
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if mime.starts_with("image/") {
        return MEDIA_TYPE_IMAGE;
    }
    if mime.starts_with("video/") {
        return MEDIA_TYPE_VIDEO;
    }
    if mime.starts_with("audio/")
        || matches!(
            ext.as_str(),
            "silk" | "ogg" | "opus" | "mp3" | "wav" | "m4a" | "amr"
        )
    {
        return MEDIA_TYPE_VOICE;
    }
    MEDIA_TYPE_FILE
}

fn truncate_str(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        value.to_string()
    } else {
        value.chars().take(max).collect()
    }
}

// ---------------------------------------------------------------------------
// WebSocket session (hermes `_listen_loop` / `_dispatch_payload` /
// `_send_identify` / `_send_resume`)
// ---------------------------------------------------------------------------

struct WsSession {
    session_id: Option<String>,
    last_seq: Option<u64>,
}

struct Runner {
    handle: Arc<QQHandle>,
    cfg: QQBotConfig,
    dispatcher: Arc<Dispatcher>,
    pairing: Option<Arc<PairingStore>>,
    dedup: Mutex<HashMap<String, Instant>>,
}

impl Runner {
    /// hermes `_is_duplicate` (300s window, 1000-entry cap).
    fn is_duplicate(&self, msg_id: &str) -> bool {
        let mut seen = self.dedup.lock().unwrap();
        dedup_check(&mut seen, msg_id)
    }
}

fn dedup_check(seen: &mut HashMap<String, Instant>, msg_id: &str) -> bool {
    if seen.len() > DEDUP_MAX_SIZE {
        seen.retain(|_, at| at.elapsed() < Duration::from_secs(DEDUP_WINDOW_SECS));
    }
    if let Some(at) = seen.get(msg_id) {
        if at.elapsed() < Duration::from_secs(DEDUP_WINDOW_SECS) {
            return true;
        }
    }
    seen.insert(msg_id.to_string(), Instant::now());
    false
}

/// Start the QQ Bot adapter (token + WS gateway + REST outbound).
pub async fn run(
    cfg: QQBotConfig,
    dispatcher: Arc<Dispatcher>,
    pairing: Option<Arc<PairingStore>>,
) {
    let app_id = resolve_app_id(&cfg);
    let client_secret = resolve_client_secret(&cfg);
    if app_id.is_empty() || client_secret.is_empty() {
        eprintln!("[qqbot] disabled: missing app_id/client_secret (set messaging.qq.app_id/client_secret or QQ_APP_ID/QQ_CLIENT_SECRET)");
        return;
    }
    let handle = Arc::new(QQHandle {
        client: reqwest::Client::new(),
        app_id,
        client_secret,
        markdown_support: cfg.markdown_support,
        token: Mutex::new(None),
        chat_type_map: Mutex::new(HashMap::new()),
        last_msg_id: Mutex::new(HashMap::new()),
        connected: AtomicBool::new(false),
    });
    register_sender(handle.clone());

    let runner = Arc::new(Runner {
        handle,
        cfg,
        dispatcher,
        pairing,
        dedup: Mutex::new(HashMap::new()),
    });

    listen_loop(runner).await;
}

struct QQSender {
    handle: Arc<QQHandle>,
}

#[async_trait::async_trait]
impl crate::messaging::PlatformSender for QQSender {
    async fn send_text(&self, chat_id: &str, text: &str) {
        self.handle.send_text(chat_id, text).await;
    }

    /// hermes qqbot `send_exec_approval` contract: 3-button approval
    /// keyboard (QQ collapses the session tier into "always" — the
    /// `/approve session` text command stays available). Guild/dm chats
    /// return false so the caller falls back to numbered text.
    async fn send_exec_approval(
        &self,
        chat_id: &str,
        command: &str,
        session_key: &str,
        description: &str,
        allow_permanent: bool,
        _allow_session: bool,
        smart_denied: bool,
    ) -> bool {
        let mut description = description.to_string();
        if smart_denied {
            description.push_str(" Owner override applies to this one operation only.");
        }
        let text = build_exec_approval_text(
            command,
            "Execute this command?",
            &description,
            APPROVAL_TIMEOUT_SECONDS,
        );
        let keyboard = build_approval_keyboard(session_key, allow_permanent && !smart_denied);
        match self
            .handle
            .send_text_with_keyboard(chat_id, &text, &keyboard)
            .await
        {
            Ok(()) => true,
            Err(e) => {
                eprintln!("[qqbot] send_exec_approval to {chat_id} failed: {e}");
                false
            }
        }
    }
}

fn register_sender(handle: Arc<QQHandle>) {
    crate::messaging::register_platform_sender("qq", Arc::new(QQSender { handle }));
}

/// hermes `_listen_loop`: reconnect/backoff with close-code semantics.
async fn listen_loop(runner: Arc<Runner>) {
    let mut backoff_idx: usize = 0;
    let mut attempts: u32 = 0;
    let mut quick_disconnect_count: u32 = 0;

    while attempts < MAX_RECONNECT_ATTEMPTS {
        let gateway_url = match runner.handle.gateway_url().await {
            Ok(url) => url,
            Err(e) => {
                eprintln!("[qqbot] gateway URL fetch failed: {e}");
                let delay = RECONNECT_BACKOFF[backoff_idx.min(RECONNECT_BACKOFF.len() - 1)];
                tokio::time::sleep(Duration::from_secs(delay)).await;
                backoff_idx += 1;
                attempts += 1;
                continue;
            }
        };

        let connect_time = Instant::now();
        let mut session = WsSession {
            session_id: None,
            last_seq: None,
        };
        match run_ws_session(&runner, &gateway_url, &mut session).await {
            Ok(()) => {
                backoff_idx = 0;
                quick_disconnect_count = 0;
            }
            Err(code) => {
                eprintln!("[qqbot] WebSocket closed: code={code}");
                match code {
                    4914 => {
                        eprintln!("[qqbot] bot offline/sandbox — not reconnecting");
                        return;
                    }
                    4915 => {
                        eprintln!("[qqbot] bot banned — not reconnecting");
                        return;
                    }
                    4004 => {
                        // Invalid token — drop the cache so the next round refreshes.
                        runner.handle.token.lock().unwrap().take();
                    }
                    4006 | 4007 | 4009 => {
                        // Session invalid — the session state is local to
                        // run_ws_session; nothing to clear here.
                    }
                    4008 => {
                        eprintln!("[qqbot] rate limited — backing off {RATE_LIMIT_DELAY_SECS}s");
                        tokio::time::sleep(Duration::from_secs(RATE_LIMIT_DELAY_SECS)).await;
                    }
                    _ => {}
                }
                let duration = connect_time.elapsed().as_secs_f64();
                if duration < QUICK_DISCONNECT_THRESHOLD_SECS {
                    quick_disconnect_count += 1;
                    if quick_disconnect_count >= MAX_QUICK_DISCONNECT_COUNT {
                        eprintln!(
                            "[qqbot] {} quick disconnects — check app_id/client_secret/intents",
                            quick_disconnect_count
                        );
                        quick_disconnect_count = 0;
                    }
                } else {
                    quick_disconnect_count = 0;
                }
                let delay = RECONNECT_BACKOFF[backoff_idx.min(RECONNECT_BACKOFF.len() - 1)];
                tokio::time::sleep(Duration::from_secs(delay)).await;
                backoff_idx = (backoff_idx + 1).min(RECONNECT_BACKOFF.len() - 1);
                attempts += 1;
            }
        }
    }
    eprintln!("[qqbot] giving up after {MAX_RECONNECT_ATTEMPTS} reconnect attempts");
}

/// One WebSocket connection: Hello → Identify/Resume, heartbeat, dispatch.
/// Returns Ok(()) on clean end, Err(close_code) otherwise.
async fn run_ws_session(
    runner: &Arc<Runner>,
    gateway_url: &str,
    session: &mut WsSession,
) -> std::result::Result<(), u16> {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    let connect = tokio::time::timeout(
        Duration::from_secs(CONNECT_TIMEOUT_SECS + 10),
        tokio_tungstenite::connect_async(gateway_url),
    )
    .await;
    let (ws, _) = match connect {
        Ok(Ok(pair)) => pair,
        Ok(Err(e)) => return Err(close_code_from_error(&e.to_string())),
        Err(_) => return Err(0),
    };
    eprintln!("[qqbot] WebSocket connected to {gateway_url}");
    runner.handle.connected.store(true, Ordering::SeqCst);
    let (mut sink, mut stream) = ws.split();

    let mut heartbeat_interval = Duration::from_secs(30);
    let mut last_heartbeat = Instant::now();
    // Resume state carried across reconnects within this process lives on
    // the session struct; identify needs the token.
    let mut identified = false;

    loop {
        // Heartbeat at 80% of the server interval (hermes).
        if identified && last_heartbeat.elapsed() >= heartbeat_interval {
            let seq = session.last_seq;
            let payload = json!({ "op": 1, "d": seq });
            if sink
                .send(WsMessage::Text(payload.to_string()))
                .await
                .is_err()
            {
                runner.handle.connected.store(false, Ordering::SeqCst);
                return Err(0);
            }
            last_heartbeat = Instant::now();
        }

        let next = tokio::time::timeout(Duration::from_secs(5), stream.next()).await;
        let message = match next {
            Ok(Some(Ok(message))) => message,
            Ok(Some(Err(e))) => {
                runner.handle.connected.store(false, Ordering::SeqCst);
                return Err(close_code_from_error(&e.to_string()));
            }
            Ok(None) => {
                runner.handle.connected.store(false, Ordering::SeqCst);
                return Err(0);
            }
            Err(_) => continue, // poll tick — loop back to heartbeat check
        };

        let WsMessage::Text(text) = message else {
            continue;
        };
        let Ok(payload) = serde_json::from_str::<Value>(&text) else {
            eprintln!(
                "[qqbot] failed to parse payload: {}",
                truncate_str(&text, 120)
            );
            continue;
        };
        let op = payload.get("op").and_then(|v| v.as_u64());
        if let Some(s) = payload.get("s").and_then(|v| v.as_u64()) {
            if session.last_seq.map(|last| s > last).unwrap_or(true) {
                session.last_seq = Some(s);
            }
        }

        match op {
            Some(10) => {
                // Hello — heartbeat interval, then Identify or Resume.
                let interval_ms = payload
                    .get("d")
                    .and_then(|d| d.get("heartbeat_interval"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(30_000);
                heartbeat_interval = Duration::from_secs_f64(interval_ms as f64 / 1000.0 * 0.8);
                last_heartbeat = Instant::now();
                if session.session_id.is_some() && session.last_seq.is_some() {
                    let token = match runner.handle.ensure_token().await {
                        Ok(token) => token,
                        Err(e) => {
                            eprintln!("[qqbot] resume token failed: {e}");
                            continue;
                        }
                    };
                    let resume = json!({
                        "op": 6,
                        "d": {
                            "token": format!("QQBot {token}"),
                            "session_id": session.session_id,
                            "seq": session.last_seq,
                        },
                    });
                    sink.send(WsMessage::Text(resume.to_string())).await.ok();
                } else {
                    let token = match runner.handle.ensure_token().await {
                        Ok(token) => token,
                        Err(e) => {
                            eprintln!("[qqbot] identify token failed: {e}");
                            continue;
                        }
                    };
                    let identify = json!({
                        "op": 2,
                        "d": {
                            "token": format!("QQBot {token}"),
                            "intents": INTENTS,
                            "shard": [0, 1],
                            "properties": {
                                "$os": std::env::consts::OS,
                                "$browser": "ulnclaw",
                                "$device": "ulnclaw",
                            },
                        },
                    });
                    sink.send(WsMessage::Text(identify.to_string())).await.ok();
                    eprintln!("[qqbot] Identify sent");
                }
                identified = true;
            }
            Some(0) => {
                let event_type = payload.get("t").and_then(|v| v.as_str()).unwrap_or("");
                let d = payload.get("d").cloned().unwrap_or(Value::Null);
                match event_type {
                    "READY" => {
                        session.session_id = d
                            .get("session_id")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                        eprintln!("[qqbot] Ready, session_id={:?}", session.session_id);
                    }
                    "RESUMED" => eprintln!("[qqbot] Session resumed"),
                    "C2C_MESSAGE_CREATE"
                    | "GROUP_AT_MESSAGE_CREATE"
                    | "DIRECT_MESSAGE_CREATE"
                    | "GUILD_MESSAGE_CREATE"
                    | "GUILD_AT_MESSAGE_CREATE" => {
                        let runner = runner.clone();
                        let event_type = event_type.to_string();
                        tokio::spawn(async move {
                            on_message(&runner, &event_type, &d).await;
                        });
                    }
                    "INTERACTION_CREATE" => {
                        // ACK + button routing (hermes `_on_interaction` +
                        // `_default_interaction_dispatch`).
                        let runner = runner.clone();
                        tokio::spawn(async move {
                            handle_interaction(&runner, &d).await;
                        });
                    }
                    _ => {}
                }
            }
            Some(7) => {
                // Server-requested reconnect.
                runner.handle.connected.store(false, Ordering::SeqCst);
                return Err(0);
            }
            Some(9) => {
                let resumable = payload.get("d").and_then(|v| v.as_bool()).unwrap_or(false);
                if !resumable {
                    session.session_id = None;
                    session.last_seq = None;
                }
                runner.handle.connected.store(false, Ordering::SeqCst);
                return Err(0);
            }
            Some(11) => {} // heartbeat ACK
            _ => {}
        }
    }
}

fn close_code_from_error(text: &str) -> u16 {
    // tungstenite surfaces close codes in the error string
    // ("Connection closed normally" / protocol errors carry no code).
    for code in [4914u16, 4915, 4004, 4006, 4007, 4008, 4009] {
        if text.contains(&code.to_string()) {
            return code;
        }
    }
    0
}

// ---------------------------------------------------------------------------
// Inbound message handling (hermes `_on_message` + handlers)
// ---------------------------------------------------------------------------

async fn on_message(runner: &Arc<Runner>, event_type: &str, d: &Value) {
    let msg_id = d
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if msg_id.is_empty() || runner.is_duplicate(&msg_id) {
        return;
    }
    let content = d
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let author = d
        .get("author")
        .filter(|v| v.is_object())
        .cloned()
        .unwrap_or(json!({}));

    match event_type {
        "C2C_MESSAGE_CREATE" => handle_c2c(runner, d, &msg_id, &content, &author).await,
        "GROUP_AT_MESSAGE_CREATE" => handle_group(runner, d, &msg_id, &content, &author).await,
        "GUILD_MESSAGE_CREATE" | "GUILD_AT_MESSAGE_CREATE" => {
            handle_guild(runner, d, &msg_id, &content, &author).await
        }
        "DIRECT_MESSAGE_CREATE" => handle_guild_dm(runner, d, &msg_id, &content, &author).await,
        _ => {}
    }
}

/// Process attachments + quoted context into (text_extra, attachments)
/// (hermes `_process_attachments` + `_process_quoted_context`).
async fn collect_inbound(
    runner: &Arc<Runner>,
    d: &Value,
    base_text: &str,
) -> (String, Vec<MediaAttachment>) {
    let mut text = base_text.to_string();
    let mut attachments: Vec<MediaAttachment> = Vec::new();
    let mut info_lines: Vec<String> = Vec::new();

    let mut lists: Vec<Value> = Vec::new();
    if let Some(arr) = d.get("attachments").and_then(|v| v.as_array()) {
        lists.push(Value::Array(arr.clone()));
    }
    // Quoted message: message_type 103 carries the referenced message in
    // msg_elements[0].
    let message_type = d.get("message_type").and_then(|v| v.as_i64()).unwrap_or(0);
    if message_type == 103 {
        if let Some(element) = d
            .get("msg_elements")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
        {
            if let Some(quote_text) = element
                .get("text_element")
                .and_then(|t| t.get("text"))
                .and_then(|v| v.as_str())
            {
                if !quote_text.trim().is_empty() {
                    text = merge_quote(&text, quote_text.trim());
                }
            }
            if let Some(arr) = element.get("attachments").and_then(|v| v.as_array()) {
                lists.push(Value::Array(arr.clone()));
            }
        }
    }

    let home = crate::config::ulnclaw_home();
    for list in lists {
        let Some(items) = list.as_array() else {
            continue;
        };
        for att in items {
            let content_type = att
                .get("content_type")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_lowercase();
            let url_raw = att
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let filename = att
                .get("filename")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let url = if let Some(rest) = url_raw.strip_prefix("//") {
                format!("https:{rest}")
            } else {
                url_raw
            };
            if url.is_empty() {
                continue;
            }
            if is_voice_content_type(&content_type, &filename) {
                // hermes priority: QQ's asr_refer_text first...
                let asr = att
                    .get("asr_refer_text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if !asr.is_empty() {
                    text = append_block(&text, &format!("[Voice] {asr}"));
                    continue;
                }
                // ...then the wav URL / raw audio into the central STT
                // pipeline.
                let wav_url = att
                    .get("voice_wav_url")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                let fetch_url = if wav_url.is_empty() {
                    url.clone()
                } else {
                    fix_scheme(&wav_url)
                };
                if let Some(attachment) =
                    download_attachment(runner, &home, &fetch_url, "audio/wav", &filename).await
                {
                    attachments.push(attachment);
                } else {
                    text = append_block(&text, "[Voice] [语音识别失败]");
                }
            } else if content_type.starts_with("image/") {
                if let Some(attachment) =
                    download_attachment(runner, &home, &url, &content_type, &filename).await
                {
                    attachments.push(attachment);
                }
            } else if let Some(attachment) =
                download_attachment(runner, &home, &url, &content_type, &filename).await
            {
                let kind = if content_type.starts_with("video/") {
                    "video"
                } else {
                    "file"
                };
                let name = if filename.is_empty() {
                    content_type.clone()
                } else {
                    filename.clone()
                };
                info_lines.push(format!("[{kind}: {name} ({})]", attachment.path.display()));
                attachments.push(attachment);
            }
        }
    }
    if !info_lines.is_empty() {
        text = append_block(&text, &info_lines.join("\n"));
    }
    (text, attachments)
}

fn fix_scheme(url: &str) -> String {
    if let Some(rest) = url.strip_prefix("//") {
        format!("https:{rest}")
    } else {
        url.to_string()
    }
}

fn append_block(text: &str, block: &str) -> String {
    if text.trim().is_empty() {
        block.to_string()
    } else {
        format!("{text}\n\n{block}")
    }
}

fn merge_quote(text: &str, quote: &str) -> String {
    let quote_block = format!("[Quote] {quote}");
    if text.trim().is_empty() {
        quote_block
    } else {
        format!("{quote_block}\n{text}")
    }
}

fn is_voice_content_type(content_type: &str, filename: &str) -> bool {
    if content_type.starts_with("audio/") {
        return true;
    }
    let ext = std::path::Path::new(filename)
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    matches!(
        ext.as_str(),
        "silk" | "amr" | "ogg" | "opus" | "mp3" | "wav" | "m4a"
    )
}

async fn download_attachment(
    runner: &Arc<Runner>,
    home: &Path,
    url: &str,
    content_type: &str,
    filename: &str,
) -> Option<MediaAttachment> {
    let response = runner
        .handle
        .client
        .get(url)
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        eprintln!(
            "[qqbot] attachment download HTTP {} for {}",
            response.status().as_u16(),
            truncate_str(url, 80)
        );
        return None;
    }
    let data = response.bytes().await.ok()?.to_vec();
    let mime = if content_type.trim().is_empty() || content_type == "application/octet-stream" {
        crate::media_cache::mime_for_ext(Path::new(filename))
    } else {
        content_type.to_string()
    };
    let path = crate::media_cache::cache_media_bytes(home, &data, &mime, filename).ok()?;
    Some(MediaAttachment {
        path,
        mime,
        bytes: data.len() as u64,
        original_name: filename.to_string(),
    })
}

async fn dispatch_inbound(
    runner: &Arc<Runner>,
    chat_id: &str,
    sender_id: &str,
    sender_name: &str,
    text: &str,
    attachments: Vec<MediaAttachment>,
    msg_id: &str,
) {
    if text.trim().is_empty() && attachments.is_empty() {
        return;
    }
    runner
        .handle
        .last_msg_id
        .lock()
        .unwrap()
        .insert(chat_id.to_string(), msg_id.to_string());
    let mut event = MessageEvent {
        platform: "qq".into(),
        chat_id: chat_id.to_string(),
        sender_id: sender_id.to_string(),
        sender_name: sender_name.to_string(),
        text: text.to_string(),
        message_id: msg_id.to_string(),
        attachments,
    };
    if !crate::messaging::pre_gateway_dispatch_gate_public(&mut event).await {
        return;
    }
    let outcome = match runner.dispatcher.handle_event(event).await {
        Ok(outcome) => outcome,
        Err(e) => crate::messaging::DispatchOutcome {
            reply: format!("error: {e}"),
            transcript_echoes: Vec::new(),
        },
    };
    for echo in &outcome.transcript_echoes {
        runner.handle.send_text(chat_id, echo).await;
    }
    let (reply_text, media_paths) = crate::messaging::extract_media_tags(&outcome.reply);
    for path in &media_paths {
        runner.handle.deliver_media_path(chat_id, path).await;
    }
    if !reply_text.trim().is_empty() {
        // P704: ledger-protected reply delivery (sender id "qq").
        runner
            .dispatcher
            .send_with_ledger("qq", chat_id, &reply_text, || {
                runner.handle.send_text(chat_id, &reply_text)
            })
            .await;
    }
}

async fn handle_c2c(runner: &Arc<Runner>, d: &Value, msg_id: &str, content: &str, author: &Value) {
    let user_openid = author
        .get("user_openid")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if user_openid.is_empty() {
        return;
    }
    if !dm_intake_allowed(&runner.cfg, &user_openid) {
        offer_pairing(runner, &user_openid, &user_openid).await;
        return;
    }
    let (text, attachments) = collect_inbound(runner, d, content).await;
    if !is_dm_fully_authorized(&runner.cfg, runner.pairing.as_ref(), &user_openid) {
        offer_pairing(runner, &user_openid, &user_openid).await;
        return;
    }
    runner
        .handle
        .chat_type_map
        .lock()
        .unwrap()
        .insert(user_openid.clone(), "c2c".into());
    dispatch_inbound(
        runner,
        &user_openid,
        &user_openid,
        &user_openid,
        &text,
        attachments,
        msg_id,
    )
    .await;
}

async fn handle_group(
    runner: &Arc<Runner>,
    d: &Value,
    msg_id: &str,
    content: &str,
    author: &Value,
) {
    let group_openid = d
        .get("group_openid")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if group_openid.is_empty() {
        return;
    }
    let member_openid = author
        .get("member_openid")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if !group_allowed(&runner.cfg, &group_openid, &member_openid) {
        return;
    }
    let text = strip_at_mention(content);
    let (text, attachments) = collect_inbound(runner, d, &text).await;
    runner
        .handle
        .chat_type_map
        .lock()
        .unwrap()
        .insert(group_openid.clone(), "group".into());
    dispatch_inbound(
        runner,
        &group_openid,
        &member_openid,
        &member_openid,
        &text,
        attachments,
        msg_id,
    )
    .await;
}

async fn handle_guild(
    runner: &Arc<Runner>,
    d: &Value,
    msg_id: &str,
    content: &str,
    author: &Value,
) {
    let channel_id = d
        .get("channel_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if channel_id.is_empty() {
        return;
    }
    let guild_id = d
        .get("guild_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let author_id = author
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let gate_id = if guild_id.is_empty() {
        channel_id.clone()
    } else {
        guild_id
    };
    if !group_allowed(&runner.cfg, &gate_id, &author_id) {
        return;
    }
    let nick = d
        .get("member")
        .and_then(|v| v.get("nick"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| author.get("username").and_then(|v| v.as_str()))
        .unwrap_or("")
        .to_string();
    let (text, attachments) = collect_inbound(runner, d, content).await;
    runner
        .handle
        .chat_type_map
        .lock()
        .unwrap()
        .insert(channel_id.clone(), "guild".into());
    dispatch_inbound(
        runner,
        &channel_id,
        &author_id,
        &nick,
        &text,
        attachments,
        msg_id,
    )
    .await;
}

async fn handle_guild_dm(
    runner: &Arc<Runner>,
    d: &Value,
    msg_id: &str,
    content: &str,
    author: &Value,
) {
    let guild_id = d
        .get("guild_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if guild_id.is_empty() {
        return;
    }
    let author_id = author
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if !dm_intake_allowed(&runner.cfg, &author_id) {
        return;
    }
    let (text, attachments) = collect_inbound(runner, d, content).await;
    if !is_dm_fully_authorized(&runner.cfg, runner.pairing.as_ref(), &author_id) {
        offer_pairing(runner, &author_id, &guild_id).await;
        return;
    }
    runner
        .handle
        .chat_type_map
        .lock()
        .unwrap()
        .insert(guild_id.clone(), "dm".into());
    dispatch_inbound(
        runner,
        &guild_id,
        &author_id,
        &author_id,
        &text,
        attachments,
        msg_id,
    )
    .await;
}

// ---------------------------------------------------------------------------
// Access policies (hermes `_is_dm_intake_allowed` / `_is_group_allowed`
// mapped onto the ulnclaw allowlist∪pairing model)
// ---------------------------------------------------------------------------

fn dm_intake_allowed(cfg: &QQBotConfig, user_id: &str) -> bool {
    let principal = user_id.trim();
    if principal.is_empty() {
        return false;
    }
    match cfg.dm_policy.as_str() {
        "disabled" => false,
        "allowlist" => entry_matches(&cfg.allow_from, principal),
        "pairing" => true,
        "open" => open_dm_opted_in(),
        _ => false,
    }
}

fn is_dm_fully_authorized(
    cfg: &QQBotConfig,
    pairing: Option<&Arc<PairingStore>>,
    user_id: &str,
) -> bool {
    if cfg.dm_policy == "open" {
        return true;
    }
    entry_matches(&cfg.allow_from, user_id)
        || pairing
            .map(|store| store.is_approved("qq", user_id))
            .unwrap_or(false)
}

fn group_allowed(cfg: &QQBotConfig, group_id: &str, _user_id: &str) -> bool {
    match cfg.group_policy.as_str() {
        "disabled" => false,
        "allowlist" => entry_matches(&cfg.group_allow_from, group_id),
        // hermes: no group pairing flow — groups stay closed unless the
        // operator moves to allowlist/open.
        "pairing" => false,
        "open" => true,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Inline-keyboard interactions (hermes `_on_interaction` +
// `_default_interaction_dispatch`)
// ---------------------------------------------------------------------------

/// Extract the ulnclaw chat id from a `platform-qq-<chat>` session key.
fn session_chat_id(session_key: &str) -> Option<&str> {
    let chat_id = session_key.strip_prefix("platform-qq-")?;
    if chat_id.is_empty() {
        return None;
    }
    Some(chat_id)
}

/// hermes `_is_authorized_interaction_for_session`, adapted to ulnclaw
/// session keys (`platform-qq-<chat>`, one session per chat): c2c clicks
/// must come from the chat's own user; group/guild clicks must originate
/// in the session's chat from a user who passes the intake gate
/// (allowlist ∪ pairing ∪ open policy).
fn interaction_authorized(
    runner: &Runner,
    operator: &str,
    event_chat: &str,
    session_key: &str,
) -> bool {
    if operator.is_empty() {
        return false;
    }
    let Some(chat_id) = session_chat_id(session_key) else {
        return false;
    };
    match runner.handle.chat_type(chat_id).as_str() {
        "c2c" => operator == chat_id,
        "dm" => is_dm_fully_authorized(&runner.cfg, runner.pairing.as_ref(), operator),
        _ => {
            if !event_chat.is_empty() && event_chat != chat_id {
                return false;
            }
            runner.cfg.group_policy == "open"
                || entry_matches(&runner.cfg.group_allow_from, operator)
                || runner
                    .pairing
                    .as_ref()
                    .map(|store| store.is_approved("qq", operator))
                    .unwrap_or(false)
        }
    }
}

/// hermes `_write_update_response`: atomically persist the update-prompt
/// answer (`y`/`n`) for the detached update watcher via tmp + rename.
fn write_update_response(answer: &str, operator: &str) {
    let home = crate::config::ulnclaw_home();
    let response_path = home.join(".update_response");
    let tmp_path = home.join(".update_response.tmp");
    match std::fs::write(&tmp_path, answer).and_then(|_| std::fs::rename(&tmp_path, &response_path))
    {
        Ok(()) => eprintln!("[qqbot] update prompt answered {answer:?} by {operator}"),
        Err(e) => eprintln!("[qqbot] failed to write update response: {e}"),
    }
}

/// hermes `_on_interaction` + `_default_interaction_dispatch`: ACK the
/// interaction promptly (the client shows an error icon otherwise), then
/// route `approve:<session>:<decision>` clicks into the approval gateway
/// and `update_prompt:<y|n>` clicks into the `.update_response` file.
async fn handle_interaction(runner: &Arc<Runner>, d: &Value) {
    let id = d.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let resolved = d.pointer("/data/resolved");
    let button_data = resolved
        .and_then(|r| r.get("button_data"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let group_member_openid = d
        .get("group_member_openid")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let user_openid = d.get("user_openid").and_then(|v| v.as_str()).unwrap_or("");
    let resolver_user_id = resolved
        .and_then(|r| r.get("user_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    // hermes `InteractionEvent.operator_openid` preference order.
    let operator = [group_member_openid, user_openid, resolver_user_id]
        .into_iter()
        .find(|s| !s.is_empty())
        .unwrap_or("");
    let group_openid = d.get("group_openid").and_then(|v| v.as_str()).unwrap_or("");
    let guild_id = d.get("guild_id").and_then(|v| v.as_str()).unwrap_or("");

    if id.is_empty() {
        eprintln!("[qqbot] INTERACTION_CREATE missing id, skipping ACK");
        return;
    }
    if let Err(e) = runner
        .handle
        .api_request(
            reqwest::Method::PUT,
            &format!("/interactions/{id}"),
            Some(&json!({"code": 0})),
            Duration::from_secs(DEFAULT_API_TIMEOUT_SECS),
        )
        .await
    {
        eprintln!("[qqbot] interaction ACK failed: {e}");
    }
    if button_data.is_empty() {
        return;
    }

    if let Some((session_key, decision)) = parse_approval_button_data(&button_data) {
        // hermes `_APPROVAL_BUTTON_TO_CHOICE` (QQ's 3-button layout has no
        // session tier).
        let choice = match decision.as_str() {
            "allow-once" => crate::approval_gateway::CHOICE_ONCE,
            "allow-always" => crate::approval_gateway::CHOICE_ALWAYS,
            "deny" => crate::approval_gateway::CHOICE_DENY,
            other => {
                eprintln!("[qqbot] unknown approval decision {other:?} (session={session_key})");
                return;
            }
        };
        if !interaction_authorized(runner, operator, group_openid, &session_key) {
            eprintln!(
                "[qqbot] rejected unauthorized approval click for session {session_key} (operator={operator})"
            );
            return;
        }
        if crate::approval_gateway::resolve(&session_key, choice) {
            eprintln!(
                "[qqbot] button resolved approval for session {session_key} (choice={choice}, operator={operator})"
            );
        } else {
            eprintln!("[qqbot] approval already resolved or expired for session {session_key}");
        }
        return;
    }

    if let Some(answer) = parse_update_prompt_button_data(&button_data) {
        let chat = [group_openid, guild_id, user_openid]
            .into_iter()
            .find(|s| !s.is_empty())
            .unwrap_or("");
        let update_session_key = format!("platform-qq-{chat}");
        if !interaction_authorized(runner, operator, group_openid, &update_session_key) {
            eprintln!("[qqbot] rejected unauthorized update prompt click (operator={operator})");
            return;
        }
        write_update_response(&answer, operator);
        return;
    }

    eprintln!("[qqbot] unrecognised button_data {button_data:?} from interaction {id}");
}

async fn offer_pairing(runner: &Runner, sender_id: &str, chat_id: &str) {
    eprintln!("[qqbot] refusing message from {sender_id} — add it to messaging.qq.allow_from or approve a pairing code");
    if let Some(store) = &runner.pairing {
        if let Some(reply) =
            crate::messaging::pairing_offer_public(store, "qq", sender_id, sender_id)
        {
            runner.handle.send_text(chat_id, &reply).await;
        }
    }
}

// ---------------------------------------------------------------------------
// Scan-to-configure onboarding (hermes `qqbot/onboard.py` + `crypto.py`)
// ---------------------------------------------------------------------------

/// hermes `ONBOARD_CREATE_PATH` / `ONBOARD_POLL_PATH` (portal host).
const ONBOARD_CREATE_PATH: &str = "/lite/create_bind_task";
const ONBOARD_POLL_PATH: &str = "/lite/poll_bind_result";
/// hermes `QR_URL_TEMPLATE`.
const QR_URL_TEMPLATE: &str =
    "https://q.qq.com/qqbot/openclaw/connect.html?task_id={task_id}&_wv=2&source=hermes";
/// hermes `ONBOARD_POLL_INTERVAL` / `ONBOARD_API_TIMEOUT`.
const ONBOARD_POLL_INTERVAL_SECS: u64 = 2;
const ONBOARD_API_TIMEOUT_SECS: u64 = 10;
/// hermes `_MAX_REFRESHES` for expired QR codes.
const ONBOARD_MAX_REFRESHES: u32 = 3;

/// hermes `PORTAL_HOST` (overridable via `QQ_PORTAL_HOST` for corporate
/// proxies / test environments).
fn onboard_portal_host() -> String {
    env_or_none("QQ_PORTAL_HOST").unwrap_or_else(|| "q.qq.com".to_string())
}

/// hermes `BindStatus` — `poll_bind_result` status codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindStatus {
    None,
    Pending,
    Completed,
    Expired,
}

impl BindStatus {
    fn from_code(code: i64) -> BindStatus {
        match code {
            1 => BindStatus::Pending,
            2 => BindStatus::Completed,
            3 => BindStatus::Expired,
            _ => BindStatus::None,
        }
    }
}

/// Credentials returned by a successful scan (hermes `qr_register`
/// result dict).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QQOnboardCredentials {
    pub app_id: String,
    pub client_secret: String,
    pub user_openid: String,
}

/// hermes `generate_bind_key` — random 256-bit AES key, base64. The key
/// is passed to `create_bind_task` so the portal encrypts the bot's
/// client_secret before returning it (the secret never travels in
/// plaintext).
pub fn generate_bind_key() -> String {
    let mut bytes = [0u8; 32];
    getrandom_fill(&mut bytes);
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// hermes `decrypt_secret` — AES-256-GCM over
/// `IV (12 bytes) || ciphertext || AuthTag (16 bytes)`, no AAD.
pub fn decrypt_bind_secret(
    encrypted_b64: &str,
    key_b64: &str,
) -> std::result::Result<String, String> {
    use aes_gcm::aead::Aead;
    use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
    use base64::Engine;
    let key_bytes = base64::engine::general_purpose::STANDARD
        .decode(key_b64.trim())
        .map_err(|e| format!("bind key base64: {e}"))?;
    if key_bytes.len() != 32 {
        return Err(format!(
            "bind key must be 32 bytes, got {}",
            key_bytes.len()
        ));
    }
    let raw = base64::engine::general_purpose::STANDARD
        .decode(encrypted_b64.trim())
        .map_err(|e| format!("encrypted secret base64: {e}"))?;
    if raw.len() < 12 + 16 {
        return Err("encrypted secret too short for IV + auth tag".to_string());
    }
    let cipher = Aes256Gcm::new_from_slice(&key_bytes).map_err(|e| format!("aes key: {e}"))?;
    let nonce = Nonce::from_slice(&raw[..12]);
    let plaintext = cipher
        .decrypt(nonce, &raw[12..])
        .map_err(|e| format!("aes-gcm decrypt: {e}"))?;
    String::from_utf8(plaintext).map_err(|e| format!("decrypted secret not utf-8: {e}"))
}

/// hermes `build_connect_url` — QR target URL for a bind task.
pub fn build_connect_url(task_id: &str) -> String {
    use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
    let encoded = utf8_percent_encode(task_id, NON_ALPHANUMERIC).to_string();
    QR_URL_TEMPLATE.replace("{task_id}", &encoded)
}

/// hermes `get_api_headers` — `q.qq.com` requires `Accept:
/// application/json` (without it the server answers with a JavaScript
/// anti-bot challenge page).
fn onboard_request(client: &reqwest::Client, url: &str, body: &Value) -> reqwest::RequestBuilder {
    client
        .post(url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header("User-Agent", "QQBotAdapter/1.1.0 (ulnclaw)")
        .json(body)
        .timeout(Duration::from_secs(ONBOARD_API_TIMEOUT_SECS))
}

/// hermes `_create_bind_task` → `(task_id, aes_key_base64)`.
async fn create_bind_task(
    client: &reqwest::Client,
) -> std::result::Result<(String, String), String> {
    let url = format!("https://{}{}", onboard_portal_host(), ONBOARD_CREATE_PATH);
    let key = generate_bind_key();
    let resp = onboard_request(client, &url, &json!({"key": key}))
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("create_bind_task: {e}"))?;
    let data: Value = resp
        .json()
        .await
        .map_err(|e| format!("create_bind_task json: {e}"))?;
    if data.get("retcode").and_then(|v| v.as_i64()).unwrap_or(-1) != 0 {
        return Err(data
            .get("msg")
            .and_then(|v| v.as_str())
            .unwrap_or("create_bind_task failed")
            .to_string());
    }
    let task_id = data
        .pointer("/data/task_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if task_id.is_empty() {
        return Err("create_bind_task: missing task_id in response".to_string());
    }
    Ok((task_id, key))
}

/// hermes `_poll_bind_result` → `(status, bot_appid, bot_encrypt_secret,
/// user_openid)`.
async fn poll_bind_result(
    client: &reqwest::Client,
    task_id: &str,
) -> std::result::Result<(BindStatus, String, String, String), String> {
    let url = format!("https://{}{}", onboard_portal_host(), ONBOARD_POLL_PATH);
    let resp = onboard_request(client, &url, &json!({"task_id": task_id}))
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("poll_bind_result: {e}"))?;
    let data: Value = resp
        .json()
        .await
        .map_err(|e| format!("poll_bind_result json: {e}"))?;
    if data.get("retcode").and_then(|v| v.as_i64()).unwrap_or(-1) != 0 {
        return Err(data
            .get("msg")
            .and_then(|v| v.as_str())
            .unwrap_or("poll_bind_result failed")
            .to_string());
    }
    let status = BindStatus::from_code(
        data.pointer("/data/status")
            .and_then(|v| v.as_i64())
            .unwrap_or(0),
    );
    let app_id = data
        .pointer("/data/bot_appid")
        .map(|v| v.to_string().trim_matches('"').to_string())
        .unwrap_or_default();
    let encrypt_secret = data
        .pointer("/data/bot_encrypt_secret")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let user_openid = data
        .pointer("/data/user_openid")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Ok((status, app_id, encrypt_secret, user_openid))
}

/// hermes `_render_qr` — terminal ASCII QR (ulnclaw renders with the
/// `qrcode` crate instead of Python `qrcode`).
fn render_onboard_qr(data: &str) -> Option<String> {
    let code = qrcode::QrCode::new(data.as_bytes()).ok()?;
    let width = code.width();
    let colors = code.to_colors();
    let mut out = String::new();
    for row in colors.chunks(width) {
        for color in row {
            out.push_str(match color {
                qrcode::Color::Dark => "██",
                qrcode::Color::Light => "  ",
            });
        }
        out.push('\n');
    }
    Some(out)
}

/// hermes `qr_register` — create bind task → display QR → poll →
/// decrypt credentials, refreshing expired QR codes up to
/// `ONBOARD_MAX_REFRESHES` times within the overall deadline.
pub async fn qr_register(
    timeout_seconds: u64,
) -> std::result::Result<Option<QQOnboardCredentials>, String> {
    let client = reqwest::Client::new();
    let deadline = Instant::now() + Duration::from_secs(timeout_seconds);
    for refresh_count in 0..=ONBOARD_MAX_REFRESHES {
        let (task_id, aes_key) = match create_bind_task(&client).await {
            Ok(result) => result,
            Err(e) => {
                eprintln!("[qqbot] onboard: failed to create bind task: {e}");
                return Ok(None);
            }
        };
        let url = build_connect_url(&task_id);
        println!();
        match render_onboard_qr(&url) {
            Some(ascii) => {
                println!("{ascii}");
                println!("  请使用手机 QQ 扫描上方二维码，或直接打开链接：\n  {url}");
            }
            None => {
                println!("  请在手机 QQ 中打开以下链接：\n  {url}");
            }
        }
        println!();

        loop {
            if Instant::now() >= deadline {
                eprintln!("[qqbot] onboard: poll timed out after {timeout_seconds}s");
                return Ok(None);
            }
            match poll_bind_result(&client, &task_id).await {
                Ok((BindStatus::Completed, app_id, encrypted_secret, user_openid)) => {
                    let client_secret = decrypt_bind_secret(&encrypted_secret, &aes_key)?;
                    println!();
                    println!("  扫码完成！（App ID: {app_id}）");
                    if !user_openid.is_empty() {
                        println!("  扫码者 OpenID: {user_openid}");
                    }
                    return Ok(Some(QQOnboardCredentials {
                        app_id,
                        client_secret,
                        user_openid,
                    }));
                }
                Ok((BindStatus::Expired, _, _, _)) => {
                    if refresh_count >= ONBOARD_MAX_REFRESHES {
                        eprintln!(
                            "[qqbot] onboard: QR code expired {ONBOARD_MAX_REFRESHES} times — giving up"
                        );
                        return Ok(None);
                    }
                    println!(
                        "\n  二维码已过期，正在刷新… ({}/{ONBOARD_MAX_REFRESHES})",
                        refresh_count + 1
                    );
                    break;
                }
                Ok(_) => {}
                Err(_) => {} // transient poll error — retry after the interval
            }
            tokio::time::sleep(Duration::from_secs(ONBOARD_POLL_INTERVAL_SECS)).await;
        }
    }
    Ok(None)
}

fn onboard_credentials_path(home: &Path) -> std::path::PathBuf {
    home.join("qq").join("credentials.json")
}

/// Persist scan-to-configure credentials (hermes saves `QQ_APP_ID` /
/// `QQ_CLIENT_SECRET` env values; ulnclaw writes an account file that
/// `resolve_app_id` / `resolve_client_secret` read as a last fallback).
pub fn save_onboard_credentials(
    home: &Path,
    creds: &QQOnboardCredentials,
) -> std::result::Result<(), String> {
    let payload = json!({
        "app_id": creds.app_id,
        "client_secret": creds.client_secret,
        "user_openid": creds.user_openid,
        "saved_at": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
    });
    let path = onboard_credentials_path(home);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(&payload).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, bytes)
        .and_then(|_| std::fs::rename(&tmp, &path))
        .map_err(|e| e.to_string())
}

/// Load persisted scan-to-configure credentials, if any.
pub fn load_onboard_credentials(home: &Path) -> Option<Value> {
    let raw = std::fs::read_to_string(onboard_credentials_path(home)).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Apply the setup-wizard choices to `<home>/config.toml` (hermes
/// `_setup_qqbot` `save_env_value` writes, expressed in ulnclaw's TOML
/// config): enables `[messaging.qq]`, sets the DM policy + allowlist,
/// preserving every other key in the file.
pub fn apply_setup_to_config(
    home: &Path,
    dm_policy: &str,
    allow_from: &[String],
) -> std::result::Result<(), String> {
    let path = home.join("config.toml");
    let mut root = if path.exists() {
        crate::config_cmd::load_toml(&path)?
    } else {
        toml::Value::Table(Default::default())
    };
    let table = root
        .as_table_mut()
        .ok_or_else(|| "config.toml is not a table".to_string())?;
    let messaging = table
        .entry("messaging")
        .or_insert_with(|| toml::Value::Table(Default::default()));
    let messaging = messaging
        .as_table_mut()
        .ok_or_else(|| "[messaging] is not a table".to_string())?;
    let qq = messaging
        .entry("qq")
        .or_insert_with(|| toml::Value::Table(Default::default()));
    let qq = qq
        .as_table_mut()
        .ok_or_else(|| "[messaging.qq] is not a table".to_string())?;
    qq.insert("enabled".into(), toml::Value::Boolean(true));
    qq.insert(
        "dm_policy".into(),
        toml::Value::String(dm_policy.to_string()),
    );
    qq.insert(
        "allow_from".into(),
        toml::Value::Array(
            allow_from
                .iter()
                .map(|entry| toml::Value::String(entry.clone()))
                .collect(),
        ),
    );
    crate::config_cmd::save_toml(&path, &root)
}

/// Set `key = value` in `<home>/.env` (creating the file if missing,
/// replacing an existing line for the key otherwise) — ulnclaw's
/// `save_env_value` parity for home-channel persistence.
pub fn upsert_env_value(
    home: &Path,
    key: &str,
    value: &str,
) -> std::result::Result<std::path::PathBuf, String> {
    let path = home.join(".env");
    let mut lines: Vec<String> = match std::fs::read_to_string(&path) {
        Ok(existing) => existing.lines().map(String::from).collect(),
        Err(_) => Vec::new(),
    };
    let new_line = format!("{}={}", key, value);
    let prefix = format!("{}=", key);
    let mut replaced = false;
    for line in lines.iter_mut() {
        if line.trim_start().starts_with(&prefix) {
            *line = new_line.clone();
            replaced = true;
            break;
        }
    }
    if !replaced {
        lines.push(new_line);
    }
    let mut out = lines.join("\n");
    out.push('\n');
    std::fs::write(&path, out).map_err(|e| format!("write {}: {}", path.display(), e))?;
    Ok(path)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    #[test]
    fn test_apply_setup_to_config_merges_existing_toml() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            "[model]\nprovider = \"test\"\n\n[messaging.qq]\napp_id = \"123\"\n",
        )
        .unwrap();
        apply_setup_to_config(dir.path(), "allowlist", &["u1".into(), "u2".into()]).unwrap();
        let raw = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        let value: toml::Value = toml::from_str(&raw).unwrap();
        let qq = &value["messaging"]["qq"];
        assert_eq!(qq["enabled"].as_bool(), Some(true));
        assert_eq!(qq["dm_policy"].as_str(), Some("allowlist"));
        assert_eq!(qq["allow_from"].as_array().unwrap().len(), 2);
        // Pre-existing keys survive the merge.
        assert_eq!(qq["app_id"].as_str(), Some("123"));
        assert_eq!(value["model"]["provider"].as_str(), Some("test"));
    }

    #[test]
    fn test_apply_setup_to_config_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        apply_setup_to_config(dir.path(), "open", &[]).unwrap();
        let value: toml::Value =
            toml::from_str(&std::fs::read_to_string(dir.path().join("config.toml")).unwrap())
                .unwrap();
        assert_eq!(value["messaging"]["qq"]["dm_policy"].as_str(), Some("open"));
    }

    #[test]
    fn test_upsert_env_value_creates_and_replaces() {
        let dir = tempfile::tempdir().unwrap();
        upsert_env_value(dir.path(), "QQBOT_HOME_CHANNEL", "openid-1").unwrap();
        let raw = std::fs::read_to_string(dir.path().join(".env")).unwrap();
        assert_eq!(raw, "QQBOT_HOME_CHANNEL=openid-1\n");
        // Replace in place, other lines untouched.
        std::fs::write(
            dir.path().join(".env"),
            "OTHER=1\nQQBOT_HOME_CHANNEL=old\nTAIL=2\n",
        )
        .unwrap();
        upsert_env_value(dir.path(), "QQBOT_HOME_CHANNEL", "openid-2").unwrap();
        let raw = std::fs::read_to_string(dir.path().join(".env")).unwrap();
        assert_eq!(raw, "OTHER=1\nQQBOT_HOME_CHANNEL=openid-2\nTAIL=2\n");
    }

    use super::*;

    fn cfg() -> QQBotConfig {
        QQBotConfig {
            enabled: true,
            app_id: "app".into(),
            client_secret: "secret".into(),
            ..Default::default()
        }
    }

    #[test]
    fn entry_matches_wildcard_and_case() {
        let entries = vec!["Alice".to_string(), "*".to_string()];
        assert!(entry_matches(&entries, "alice"));
        assert!(entry_matches(&entries, "anyone"));
        assert!(entry_matches(&["Alice".into()], "alice"));
        assert!(!entry_matches(&["Alice".into()], "bob"));
    }

    #[test]
    fn strip_at_mention_removes_prefix() {
        assert_eq!(strip_at_mention("@BotBot hello there"), "hello there");
        assert_eq!(strip_at_mention("no mention"), "no mention");
    }

    #[test]
    fn strip_markdown_basics() {
        assert_eq!(strip_markdown("**bold** and __also__"), "bold and also");
        assert_eq!(strip_markdown("[label](http://x)"), "label");
        assert_eq!(strip_markdown("# Heading\ncode `x`"), "Heading\ncode x");
    }

    #[test]
    fn chunk_reply_respects_limit() {
        let short = "hello";
        assert_eq!(chunk_reply(short, MAX_MESSAGE_LENGTH), vec!["hello"]);

        let mut long = String::new();
        for i in 0..80 {
            long.push_str(&format!("Paragraph {i} body text.\n\n"));
        }
        let chunks = chunk_reply(&long, 400);
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            assert!(chunk.chars().count() <= 400);
        }

        // Single giant paragraph hard-splits at the limit.
        let giant = "x".repeat(1000);
        let chunks = chunk_reply(&giant, 400);
        assert_eq!(chunks.len(), 3);
        assert!(chunks.iter().all(|c| c.chars().count() <= 400));
    }

    #[test]
    fn msg_seq_within_range() {
        for _ in 0..32 {
            assert!(next_msg_seq() < 65536);
        }
    }

    #[test]
    fn qq_timestamp_parsing() {
        let iso = parse_qq_timestamp("2026-08-06T10:00:00+08:00");
        assert_eq!(iso.timestamp(), 1785981600);
        let millis = parse_qq_timestamp("1785981600000");
        assert_eq!(millis.timestamp(), 1785981600);
        // Garbage falls back to now (not a panic).
        let _ = parse_qq_timestamp("not a date");
    }

    #[test]
    fn voice_content_type_detection() {
        assert!(is_voice_content_type("audio/silk", ""));
        assert!(is_voice_content_type("", "note.amr"));
        assert!(is_voice_content_type("", "voice.SILK"));
        assert!(!is_voice_content_type("image/png", "pic.png"));
    }

    #[test]
    fn media_type_routing() {
        assert_eq!(media_type_for_path(Path::new("pic.png")), MEDIA_TYPE_IMAGE);
        assert_eq!(media_type_for_path(Path::new("clip.mp4")), MEDIA_TYPE_VIDEO);
        assert_eq!(
            media_type_for_path(Path::new("note.silk")),
            MEDIA_TYPE_VOICE
        );
        assert_eq!(media_type_for_path(Path::new("doc.pdf")), MEDIA_TYPE_FILE);
    }

    #[test]
    fn dm_policy_gates() {
        let mut c = cfg();
        c.dm_policy = "disabled".into();
        assert!(!dm_intake_allowed(&c, "u1"));
        c.dm_policy = "allowlist".into();
        c.allow_from = vec!["u1".into()];
        assert!(dm_intake_allowed(&c, "u1"));
        assert!(!dm_intake_allowed(&c, "u2"));
        c.dm_policy = "pairing".into();
        assert!(dm_intake_allowed(&c, "anyone"));
    }

    #[test]
    fn group_policy_gates() {
        let mut c = cfg();
        c.group_policy = "pairing".into();
        assert!(!group_allowed(&c, "g1", "u1")); // hermes: pairing closes groups
        c.group_policy = "allowlist".into();
        c.group_allow_from = vec!["g1".into()];
        assert!(group_allowed(&c, "g1", "u1"));
        assert!(!group_allowed(&c, "g2", "u1"));
        c.group_policy = "open".into();
        assert!(group_allowed(&c, "anything", "u1"));
    }

    #[test]
    fn quote_and_block_merging() {
        assert_eq!(merge_quote("", "original"), "[Quote] original");
        assert_eq!(merge_quote("reply", "original"), "[Quote] original\nreply");
        assert_eq!(append_block("", "[Voice] hi"), "[Voice] hi");
        assert_eq!(append_block("text", "[Voice] hi"), "text\n\n[Voice] hi");
    }

    #[test]
    fn text_body_markdown_vs_plain() {
        let handle = QQHandle {
            client: reqwest::Client::new(),
            app_id: "a".into(),
            client_secret: "s".into(),
            markdown_support: true,
            token: Mutex::new(None),
            chat_type_map: Mutex::new(HashMap::new()),
            last_msg_id: Mutex::new(HashMap::new()),
            connected: AtomicBool::new(false),
        };
        let body = handle.build_text_body("hello", None);
        assert_eq!(body["msg_type"], MSG_TYPE_MARKDOWN);
        assert_eq!(body["markdown"]["content"], "hello");

        let reply_body = handle.build_text_body("hello", Some("msg-1"));
        assert_eq!(reply_body["msg_id"], "msg-1");
        assert!(reply_body.get("message_reference").is_none());

        let plain = QQHandle {
            markdown_support: false,
            ..QQHandle {
                client: reqwest::Client::new(),
                app_id: "a".into(),
                client_secret: "s".into(),
                markdown_support: false,
                token: Mutex::new(None),
                chat_type_map: Mutex::new(HashMap::new()),
                last_msg_id: Mutex::new(HashMap::new()),
                connected: AtomicBool::new(false),
            }
        };
        let body = plain.build_text_body("hello", Some("msg-1"));
        assert_eq!(body["msg_type"], MSG_TYPE_TEXT);
        assert_eq!(body["content"], "hello");
        assert_eq!(body["message_reference"]["message_id"], "msg-1");
    }

    #[test]
    fn dedup_window() {
        let mut seen: HashMap<String, Instant> = HashMap::new();
        assert!(!dedup_check(&mut seen, "m1"));
        assert!(dedup_check(&mut seen, "m1"));
        assert!(!dedup_check(&mut seen, "m2"));
        // Expired entries pass again.
        seen.insert(
            "m3".into(),
            Instant::now() - Duration::from_secs(DEDUP_WINDOW_SECS + 1),
        );
        assert!(!dedup_check(&mut seen, "m3"));
    }

    #[test]
    fn approval_keyboard_layout_matches_hermes() {
        let kb = build_approval_keyboard("platform-qq-ABC", true);
        let buttons = &kb["content"]["rows"][0]["buttons"];
        assert_eq!(buttons.as_array().unwrap().len(), 3);
        let ids: Vec<&str> = buttons
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["allow", "always", "deny"]);
        assert_eq!(buttons[0]["render_data"]["label"], "✅ 允许一次");
        assert_eq!(buttons[1]["render_data"]["label"], "⭐ 始终允许");
        assert_eq!(buttons[2]["render_data"]["label"], "❌ 拒绝");
        assert_eq!(
            buttons[0]["action"]["data"],
            "approve:platform-qq-ABC:allow-once"
        );
        assert_eq!(
            buttons[1]["action"]["data"],
            "approve:platform-qq-ABC:allow-always"
        );
        assert_eq!(buttons[2]["action"]["data"], "approve:platform-qq-ABC:deny");
        // Callback action, all-users permission, single-use, shared group.
        for b in buttons.as_array().unwrap() {
            assert_eq!(b["action"]["type"], 1);
            assert_eq!(b["action"]["permission"]["type"], 2);
            assert_eq!(b["action"]["click_limit"], 1);
            assert_eq!(b["group_id"], "approval");
        }
        assert_eq!(buttons[2]["render_data"]["style"], 0);
    }

    #[test]
    fn approval_keyboard_hides_permanent_button() {
        let kb = build_approval_keyboard("sk", false);
        let buttons = kb["content"]["rows"][0]["buttons"].as_array().unwrap();
        assert_eq!(buttons.len(), 2);
        assert_eq!(buttons[0]["id"], "allow");
        assert_eq!(buttons[1]["id"], "deny");
    }

    #[test]
    fn update_prompt_keyboard_layout() {
        let kb = build_update_prompt_keyboard();
        let buttons = &kb["content"]["rows"][0]["buttons"];
        assert_eq!(buttons[0]["action"]["data"], "update_prompt:y");
        assert_eq!(buttons[1]["action"]["data"], "update_prompt:n");
        assert_eq!(buttons[0]["group_id"], "update_prompt");
        assert_eq!(buttons[0]["render_data"]["label"], "✓ 确认");
        assert_eq!(buttons[1]["render_data"]["label"], "✗ 取消");
    }

    #[test]
    fn approval_button_data_parses_with_colons() {
        // Session keys may contain colons — the middle group is greedy.
        let parsed = parse_approval_button_data("approve:agent:main:qqbot:c2c:OPENID:allow-always");
        assert_eq!(
            parsed,
            Some((
                "agent:main:qqbot:c2c:OPENID".to_string(),
                "allow-always".to_string()
            ))
        );
        assert_eq!(
            parse_approval_button_data("approve:platform-qq-XYZ:deny"),
            Some(("platform-qq-XYZ".to_string(), "deny".to_string()))
        );
        assert_eq!(parse_approval_button_data("approve:sk:maybe"), None);
        assert_eq!(parse_approval_button_data("update_prompt:y"), None);
        assert_eq!(parse_approval_button_data(""), None);
    }

    #[test]
    fn update_prompt_button_data_parsing() {
        assert_eq!(
            parse_update_prompt_button_data("update_prompt:y"),
            Some("y".to_string())
        );
        assert_eq!(
            parse_update_prompt_button_data("update_prompt:n"),
            Some("n".to_string())
        );
        assert_eq!(parse_update_prompt_button_data("update_prompt:x"), None);
        assert_eq!(parse_update_prompt_button_data("approve:sk:deny"), None);
    }

    #[test]
    fn exec_approval_text_layout() {
        let text =
            build_exec_approval_text("rm -rf /tmp/x", "Execute this command?", "dangerous", 300);
        assert!(text.starts_with("🔐 **命令执行审批**"));
        assert!(text.contains("```\nrm -rf /tmp/x\n```"));
        assert!(text.contains("📋 Execute this command?"));
        assert!(text.contains("📝 dangerous"));
        assert!(text.contains("⏱️ 超时: 300 秒"));
        // Command previews truncate at 300 chars (hermes `command[:300]`).
        let long = "x".repeat(400);
        let text = build_exec_approval_text(&long, "", "", 300);
        assert!(text.contains(&"x".repeat(300)));
        assert!(!text.contains(&"x".repeat(301)));
    }

    #[test]
    fn session_chat_id_extraction() {
        assert_eq!(session_chat_id("platform-qq-ABC"), Some("ABC"));
        assert_eq!(session_chat_id("platform-qq-"), None);
        assert_eq!(session_chat_id("agent:main:qqbot:c2c:X"), None);
    }

    #[test]
    fn bind_status_from_codes() {
        assert_eq!(BindStatus::from_code(0), BindStatus::None);
        assert_eq!(BindStatus::from_code(1), BindStatus::Pending);
        assert_eq!(BindStatus::from_code(2), BindStatus::Completed);
        assert_eq!(BindStatus::from_code(3), BindStatus::Expired);
        assert_eq!(BindStatus::from_code(99), BindStatus::None);
    }

    #[test]
    fn build_connect_url_matches_template() {
        let url = build_connect_url("task 123/abc");
        assert!(url.starts_with("https://q.qq.com/qqbot/openclaw/connect.html?task_id="));
        assert!(url.contains("task%20123%2Fabc"));
        assert!(url.ends_with("&_wv=2&source=hermes"));
    }

    #[test]
    fn decrypt_bind_secret_round_trips() {
        use aes_gcm::aead::Aead;
        use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
        use base64::Engine;
        let key_bytes = [7u8; 32];
        let key_b64 = base64::engine::general_purpose::STANDARD.encode(key_bytes);
        let cipher = Aes256Gcm::new_from_slice(&key_bytes).unwrap();
        let iv = [9u8; 12];
        let mut blob = iv.to_vec();
        // AESGCM.encrypt appends the 16-byte auth tag (hermes layout).
        blob.extend(
            cipher
                .encrypt(Nonce::from_slice(&iv), &b"super-secret-value"[..])
                .unwrap(),
        );
        let encrypted_b64 = base64::engine::general_purpose::STANDARD.encode(&blob);
        assert_eq!(
            decrypt_bind_secret(&encrypted_b64, &key_b64).unwrap(),
            "super-secret-value"
        );
        // Wrong key fails authentication.
        let bad_key = base64::engine::general_purpose::STANDARD.encode([8u8; 32]);
        assert!(decrypt_bind_secret(&encrypted_b64, &bad_key).is_err());
        // Truncated blob rejected.
        assert!(decrypt_bind_secret(
            &base64::engine::general_purpose::STANDARD.encode([0u8; 10]),
            &key_b64
        )
        .is_err());
    }

    #[test]
    fn generate_bind_key_is_32_random_bytes() {
        use base64::Engine;
        let key = generate_bind_key();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&key)
            .unwrap();
        assert_eq!(decoded.len(), 32);
        assert_ne!(key, generate_bind_key());
    }

    #[test]
    fn onboard_credentials_save_and_load() {
        let temp = tempfile::tempdir().unwrap();
        let creds = QQOnboardCredentials {
            app_id: "1024".into(),
            client_secret: "s3cr3t".into(),
            user_openid: "ou_scanner".into(),
        };
        assert!(load_onboard_credentials(temp.path()).is_none());
        save_onboard_credentials(temp.path(), &creds).unwrap();
        let loaded = load_onboard_credentials(temp.path()).unwrap();
        assert_eq!(loaded["app_id"], "1024");
        assert_eq!(loaded["client_secret"], "s3cr3t");
        assert_eq!(loaded["user_openid"], "ou_scanner");
        assert!(loaded["saved_at"].as_str().unwrap().ends_with('Z'));
    }
}

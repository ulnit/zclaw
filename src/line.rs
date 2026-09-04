//! LINE Messaging API platform adapter — port of hermes
//! `plugins/platforms/line` @ v2026.8.3 (adapter.py).
//!
//! Webhook ingress (gateway-mounted `/webhooks/line`; hermes runs a
//! dedicated aiohttp server on `/line/webhook`): raw-body
//! `X-Line-Signature` verification (base64 HMAC-SHA256 keyed by the
//! channel secret, constant-time compare), 1 MiB body cap,
//! webhook-event-id dedup (LRU-1000 against at-least-once retries).
//!
//! Intake resolves the event source (user/group/room → chat id) and
//! applies hermes' three-allowlist gate (`LINE_ALLOWED_USERS` U-ids,
//! `LINE_ALLOWED_GROUPS` C-ids, `LINE_ALLOWED_ROOMS` R-ids,
//! `LINE_ALLOW_ALL_USERS` dev escape hatch) unioned with pairing.
//! Text messages dispatch directly; image/video/audio/file messages
//! download via `api-data.line.me/v2/bot/message/<id>/content` into
//! the media cache; stickers/locations degrade to text notes.
//!
//! Replies prefer the single-use reply token (free, ~60 s TTL) and
//! fall back to the metered Push API when the token is rejected.
//! Messages are markdown-stripped with URLs preserved (`[label](url)`
//! → `label (url)`) and smart-chunked at 4500 chars with a 5-message
//! per-call budget (hermes `split_for_line`).
//!
//! Outbound `MEDIA:` tags (hermes `send_image_file`/`send_voice`/
//! `send_video`): LINE only accepts publicly reachable HTTPS media
//! URLs, so local files are registered under random 30-minute tokens
//! and served by the gateway at `/line/media/<token>/<filename>`;
//! `public_url` (`LINE_PUBLIC_URL`) must point at the gateway's
//! public origin. Images cap at 10 MB, audio/video at 200 MB; video
//! messages get the supplied preview or a 1×1 fallback PNG.
//!
//! Slow-LLM postback button (hermes `_keep_typing` wrapper, PR
//! #18153): when the agent is still running after
//! `slow_response_threshold` (default 45 s, 0 disables) the reply
//! token is spent on a Template Buttons bubble whose postback action
//! carries a request id; the finished reply lands in an in-memory
//! state machine (PENDING → READY/ERROR → DELIVERED, 1 h TTL, 24 h
//! for PENDING) and is delivered when the user taps the button.

use crate::messaging::{Dispatcher, MediaAttachment, MessageEvent};
use base64::Engine;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

/// hermes `LINE_SAFE_BUBBLE_CHARS` (LINE bubble limit is 5000).
const LINE_SAFE_BUBBLE_CHARS: usize = 4500;
/// hermes `LINE_MAX_MESSAGES_PER_CALL`.
const LINE_MAX_MESSAGES_PER_CALL: usize = 5;
/// hermes `WEBHOOK_BODY_MAX_BYTES`.
const WEBHOOK_BODY_MAX_BYTES: usize = 1_048_576;
const EVENT_DEDUP_MAX: usize = 1000;
const API_TIMEOUT: Duration = Duration::from_secs(30);
/// hermes `LINE_REPLY_TOKEN_TTL_SECONDS` (conservative cap below ~60 s).
const LINE_REPLY_TOKEN_TTL: Duration = Duration::from_secs(50);
/// hermes `MEDIA_TOKEN_TTL_SECONDS` — LINE caches URLs aggressively.
const MEDIA_TOKEN_TTL: Duration = Duration::from_secs(1800);
/// hermes `LINE_IMAGE_MAX_BYTES` / `LINE_AV_MAX_BYTES`.
const LINE_IMAGE_MAX_BYTES: u64 = 10 * 1024 * 1024;
const LINE_AV_MAX_BYTES: u64 = 200 * 1024 * 1024;
/// hermes `DEFAULT_MEDIA_PATH_PREFIX`.
const MEDIA_PATH_PREFIX: &str = "/line/media";
/// Postback cache TTLs (hermes `RequestCache`: 1 h READY/ERROR, 24 h
/// PENDING).
const POSTBACK_TTL: Duration = Duration::from_secs(3600);
const POSTBACK_PENDING_TTL: Duration = Duration::from_secs(86400);
const LINE_REPLY_URL: &str = "https://api.line.me/v2/bot/message/reply";
const LINE_PUSH_URL: &str = "https://api.line.me/v2/bot/message/push";
const LINE_LOADING_URL: &str = "https://api.line.me/v2/bot/chat/loading/start";
const LINE_CONTENT_URL: &str = "https://api-data.line.me/v2/bot/message";

/// `[messaging.line]` — LINE adapter (hermes `platforms.line` plugin
/// config + `LINE_*` env vars).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LineConfig {
    pub enabled: bool,
    /// Channel access token (fallback `LINE_CHANNEL_ACCESS_TOKEN`).
    pub channel_access_token: String,
    /// Channel secret for webhook signatures (fallback
    /// `LINE_CHANNEL_SECRET`).
    pub channel_secret: String,
    /// U-prefixed user ids (fallback `LINE_ALLOWED_USERS`).
    pub allowed_users: Vec<String>,
    /// C-prefixed group ids (fallback `LINE_ALLOWED_GROUPS`).
    pub allowed_groups: Vec<String>,
    /// R-prefixed room ids (fallback `LINE_ALLOWED_ROOMS`).
    pub allowed_rooms: Vec<String>,
    /// Dev escape hatch (fallback `LINE_ALLOW_ALL_USERS`).
    pub allow_all_users: bool,
    /// Cron/notification delivery chat (fallback `LINE_HOME_CHANNEL`).
    pub home_channel: String,
    /// Public HTTPS origin serving the gateway (fallback
    /// `LINE_PUBLIC_URL`) — required for outbound media.
    pub public_url: String,
    /// Slow-LLM postback threshold in seconds, 0 disables (fallback
    /// `LINE_SLOW_RESPONSE_THRESHOLD`, hermes default 45).
    pub slow_response_threshold: f64,
    /// Slow-LLM button copy (fallback `LINE_PENDING_TEXT`).
    pub pending_text: String,
    /// Slow-LLM button label (fallback `LINE_BUTTON_LABEL`).
    pub button_label: String,
    /// Repeat-tap notice (fallback `LINE_DELIVERED_TEXT`).
    pub delivered_text: String,
    /// Interrupted-run notice (fallback `LINE_INTERRUPTED_TEXT`).
    pub interrupted_text: String,
}

impl Default for LineConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            channel_access_token: String::new(),
            channel_secret: String::new(),
            allowed_users: Vec::new(),
            allowed_groups: Vec::new(),
            allowed_rooms: Vec::new(),
            allow_all_users: false,
            home_channel: String::new(),
            public_url: String::new(),
            slow_response_threshold: 45.0,
            pending_text: "🤔 Still thinking. Tap below to fetch the answer when it's ready."
                .into(),
            button_label: "Get answer".into(),
            delivered_text: "Already replied ✅".into(),
            interrupted_text: "Run was interrupted before completion.".into(),
        }
    }
}

fn env_trim(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn env_list(name: &str) -> Option<Vec<String>> {
    env_trim(name).map(|raw| {
        raw.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    })
}

/// Resolved runtime settings (env > config, hermes precedence).
#[derive(Debug, Clone)]
pub struct ResolvedLine {
    pub channel_access_token: String,
    pub channel_secret: String,
    pub allowed_users: Vec<String>,
    pub allowed_groups: Vec<String>,
    pub allowed_rooms: Vec<String>,
    pub allow_all_users: bool,
    pub home_channel: String,
    pub public_url: String,
    pub slow_response_threshold: f64,
    pub pending_text: String,
    pub button_label: String,
    pub delivered_text: String,
    pub interrupted_text: String,
}

impl LineConfig {
    pub fn resolve(&self) -> ResolvedLine {
        ResolvedLine {
            channel_access_token: env_trim("LINE_CHANNEL_ACCESS_TOKEN")
                .unwrap_or_else(|| self.channel_access_token.clone()),
            channel_secret: env_trim("LINE_CHANNEL_SECRET")
                .unwrap_or_else(|| self.channel_secret.clone()),
            allowed_users: env_list("LINE_ALLOWED_USERS")
                .unwrap_or_else(|| self.allowed_users.clone()),
            allowed_groups: env_list("LINE_ALLOWED_GROUPS")
                .unwrap_or_else(|| self.allowed_groups.clone()),
            allowed_rooms: env_list("LINE_ALLOWED_ROOMS")
                .unwrap_or_else(|| self.allowed_rooms.clone()),
            allow_all_users: env_trim("LINE_ALLOW_ALL_USERS")
                .map(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes"))
                .unwrap_or(self.allow_all_users),
            home_channel: env_trim("LINE_HOME_CHANNEL")
                .unwrap_or_else(|| self.home_channel.clone()),
            public_url: env_trim("LINE_PUBLIC_URL")
                .unwrap_or_else(|| self.public_url.trim().to_string())
                .trim_end_matches('/')
                .to_string(),
            slow_response_threshold: env_trim("LINE_SLOW_RESPONSE_THRESHOLD")
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(self.slow_response_threshold),
            pending_text: env_trim("LINE_PENDING_TEXT")
                .unwrap_or_else(|| self.pending_text.clone()),
            button_label: env_trim("LINE_BUTTON_LABEL")
                .unwrap_or_else(|| self.button_label.clone()),
            delivered_text: env_trim("LINE_DELIVERED_TEXT")
                .unwrap_or_else(|| self.delivered_text.clone()),
            interrupted_text: env_trim("LINE_INTERRUPTED_TEXT")
                .unwrap_or_else(|| self.interrupted_text.clone()),
        }
    }
}

/// hermes `verify_line_signature` — base64 HMAC-SHA256 over the raw
/// body, constant-time compare.
pub fn verify_line_signature(body: &[u8], signature: &str, channel_secret: &str) -> bool {
    if signature.is_empty() || channel_secret.is_empty() {
        return false;
    }
    let Ok(mut mac) = Hmac::<sha2::Sha256>::new_from_slice(channel_secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    let expected = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
    constant_time_eq(expected.as_bytes(), signature.as_bytes())
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

/// hermes `_resolve_chat` — source block → (chat_id, chat_type).
pub fn resolve_chat(source: &Value) -> (String, &'static str) {
    let src_type = source.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match src_type {
        "group" => (
            source
                .get("groupId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            "group",
        ),
        "room" => (
            source
                .get("roomId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            "room",
        ),
        "user" => (
            source
                .get("userId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            "dm",
        ),
        _ => (String::new(), "dm"),
    }
}

/// hermes `_allowed_for_source` — three-list gate.
pub fn allowed_for_source(cfg: &ResolvedLine, source: &Value) -> bool {
    if cfg.allow_all_users {
        return true;
    }
    let src_type = source.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match src_type {
        "user" => {
            let uid = source.get("userId").and_then(|v| v.as_str()).unwrap_or("");
            !uid.is_empty() && cfg.allowed_users.iter().any(|u| u == uid || u == "*")
        }
        "group" => {
            let gid = source.get("groupId").and_then(|v| v.as_str()).unwrap_or("");
            !gid.is_empty() && cfg.allowed_groups.iter().any(|g| g == gid || g == "*")
        }
        "room" => {
            let rid = source.get("roomId").and_then(|v| v.as_str()).unwrap_or("");
            !rid.is_empty() && cfg.allowed_rooms.iter().any(|r| r == rid || r == "*")
        }
        _ => false,
    }
}

/// hermes `strip_markdown_preserving_urls` — LINE renders markdown
/// literally but auto-links bare URLs.
pub fn strip_markdown_preserving_urls(text: &str) -> String {
    let mut out = text.to_string();
    // Code fences: keep content, drop the fence lines.
    let mut unfenced = String::new();
    let mut in_fence = false;
    for line in out.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        unfenced.push_str(line);
        unfenced.push('\n');
    }
    out = unfenced.trim_end_matches('\n').to_string();
    // Inline code.
    out = crate::sms::strip_paired_pub(&out, "`");
    // Markdown links → "label (url)" (http(s) targets only, hermes).
    out = links_to_label_url(&out);
    // Bold then italic.
    out = crate::sms::strip_paired_pub(&out, "**");
    out = strip_single_star_italics(&out);
    // Headings and bullets.
    out = out
        .lines()
        .map(|line| {
            let trimmed = line.trim_start();
            let hashes = trimmed.chars().take_while(|c| *c == '#').count();
            if (1..=6).contains(&hashes)
                && trimmed.chars().nth(hashes).map(|c| c.is_whitespace()) == Some(true)
            {
                trimmed[hashes..].trim_start()
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    out = out
        .lines()
        .map(|line| {
            let trimmed = line.trim_start();
            if trimmed.starts_with("- ") || trimmed.starts_with("* ") || trimmed.starts_with("+ ") {
                format!("{}• {}", &line[..line.len() - trimmed.len()], &trimmed[2..])
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    out
}

fn links_to_label_url(input: &str) -> String {
    let mut out = String::new();
    let mut rest = input;
    while let Some(start) = rest.find('[') {
        let Some(close_label) = rest[start..].find(']') else {
            break;
        };
        let label_end = start + close_label;
        if rest[label_end..].starts_with("](http://") || rest[label_end..].starts_with("](https://")
        {
            let Some(close_target) = rest[label_end..].find(')') else {
                break;
            };
            let label = &rest[start + 1..label_end];
            let target = &rest[label_end + 2..label_end + close_target];
            if target.contains(char::is_whitespace) {
                out.push_str(&rest[..label_end + close_target + 1]);
                rest = &rest[label_end + close_target + 1..];
                continue;
            }
            out.push_str(&rest[..start]);
            out.push_str(&format!("{label} ({target})"));
            rest = &rest[label_end + close_target + 1..];
        } else {
            out.push_str(&rest[..label_end + 1]);
            rest = &rest[label_end + 1..];
        }
    }
    out.push_str(rest);
    out
}

/// hermes `_MD_ITAL_RE` approximation: paired `*text*` that is not
/// `**` and has no whitespace at the inner edges.
fn strip_single_star_italics(input: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '*'
            && (i == 0 || chars[i - 1] != '*')
            && i + 1 < chars.len()
            && chars[i + 1] != '*'
            && !chars[i + 1].is_whitespace()
        {
            // Find closing single star.
            let mut j = i + 1;
            let mut found = None;
            while j < chars.len() {
                if chars[j] == '*'
                    && (j + 1 >= chars.len() || chars[j + 1] != '*')
                    && (j == i + 1 || !chars[j - 1].is_whitespace())
                {
                    found = Some(j);
                    break;
                }
                j += 1;
            }
            if let Some(end) = found {
                out.extend(&chars[i + 1..end]);
                i = end + 1;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// hermes `split_for_line` — paragraph/line/space-aware chunking with
/// a 5-chunk budget and ellipsis truncation.
pub fn split_for_line(text: &str, max_chars: usize) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    if text.chars().count() <= max_chars {
        return vec![text.to_string()];
    }
    let mut chunks: Vec<String> = Vec::new();
    let mut remaining = text.to_string();
    while !remaining.is_empty() && chunks.len() < LINE_MAX_MESSAGES_PER_CALL {
        if remaining.chars().count() <= max_chars {
            chunks.push(remaining);
            remaining = String::new();
            break;
        }
        let window: String = remaining.chars().take(max_chars).collect();
        let cut = window
            .rfind("\n\n")
            .filter(|c| *c >= max_chars / 2)
            .or_else(|| window.rfind('\n').filter(|c| *c >= max_chars / 2))
            .or_else(|| window.rfind(' ').filter(|c| *c >= max_chars / 2))
            .unwrap_or(max_chars);
        let (head, tail) = remaining.split_at(
            remaining
                .char_indices()
                .nth(cut)
                .map(|(idx, _)| idx)
                .unwrap_or(remaining.len()),
        );
        chunks.push(head.trim_end().to_string());
        remaining = tail.trim_start().to_string();
    }
    if !remaining.is_empty() {
        if let Some(last) = chunks.last_mut() {
            let truncated: String = last.chars().take(max_chars - 1).collect();
            *last = format!("{}…", truncated.trim_end());
        } else {
            let truncated: String = remaining.chars().take(max_chars - 1).collect();
            chunks.push(format!("{truncated}…"));
        }
    }
    chunks
}

struct Runtime {
    cfg: ResolvedLine,
    client: reqwest::Client,
    /// webhookEventId dedup (hermes `_EventIdDedup` LRU-1000).
    seen_events: Mutex<Vec<String>>,
    /// chat_id → (reply token, expiry) — hermes `_reply_tokens`.
    reply_tokens: Mutex<std::collections::HashMap<String, (String, std::time::Instant)>>,
    /// Slow-LLM postback cache — hermes `RequestCache`.
    postback_cache: Mutex<std::collections::HashMap<String, PostbackEntry>>,
    /// chat_id → outstanding postback request id (hermes
    /// `_pending_buttons`).
    pending_buttons: Mutex<std::collections::HashMap<String, String>>,
    /// media token → (path, expiry) — hermes `_media_tokens`.
    media_tokens:
        Mutex<std::collections::HashMap<String, (std::path::PathBuf, std::time::Instant)>>,
}

static RUNTIME: std::sync::OnceLock<Arc<Runtime>> = std::sync::OnceLock::new();

/// Register the adapter (called from `run_messaging` when enabled).
pub fn register(cfg: &LineConfig) {
    let resolved = cfg.resolve();
    if resolved.channel_access_token.is_empty() || resolved.channel_secret.is_empty() {
        eprintln!(
            "[line] enabled but LINE_CHANNEL_ACCESS_TOKEN/LINE_CHANNEL_SECRET are not both set — webhook route will reject"
        );
    }
    let runtime = Arc::new(Runtime {
        client: reqwest::Client::builder()
            .timeout(API_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new()),
        cfg: resolved,
        seen_events: Mutex::new(Vec::new()),
        reply_tokens: Mutex::new(std::collections::HashMap::new()),
        postback_cache: Mutex::new(std::collections::HashMap::new()),
        pending_buttons: Mutex::new(std::collections::HashMap::new()),
        media_tokens: Mutex::new(std::collections::HashMap::new()),
    });
    let _ = RUNTIME.set(runtime.clone());
    crate::messaging::register_platform_sender(
        "line",
        Arc::new(LineSender {
            runtime: runtime.clone(),
        }),
    );
}

fn runtime() -> Option<Arc<Runtime>> {
    RUNTIME.get().cloned()
}

/// Webhook response handed back to the gateway route.
pub struct LineWebhookResponse {
    pub status: u16,
    pub body: Value,
}

fn ack() -> LineWebhookResponse {
    LineWebhookResponse {
        status: 200,
        body: json!({}),
    }
}

/// Gateway webhook entry point, mounted at `/webhooks/line`.
pub async fn line_handle_webhook(
    dispatcher: &Arc<Dispatcher>,
    pairing: Option<&crate::pairing::PairingStore>,
    body: &[u8],
    headers: &[(String, String)],
) -> LineWebhookResponse {
    if body.len() > WEBHOOK_BODY_MAX_BYTES {
        return LineWebhookResponse {
            status: 413,
            body: json!({ "error": "body too large" }),
        };
    }
    let Some(runtime) = runtime() else {
        return LineWebhookResponse {
            status: 503,
            body: json!({ "error": "line adapter not registered" }),
        };
    };
    let signature = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("x-line-signature"))
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    if !verify_line_signature(body, &signature, &runtime.cfg.channel_secret) {
        return LineWebhookResponse {
            status: 401,
            body: json!({ "error": "invalid signature" }),
        };
    }
    let payload: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => {
            return LineWebhookResponse {
                status: 400,
                body: json!({ "error": "invalid JSON" }),
            }
        }
    };
    let events = payload
        .get("events")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for event in events {
        handle_event(&runtime, dispatcher, pairing, &event).await;
    }
    ack()
}

async fn remember_event(runtime: &Runtime, event_id: &str) -> bool {
    if event_id.is_empty() {
        return true;
    }
    let mut seen = runtime.seen_events.lock().await;
    if seen.iter().any(|e| e == event_id) {
        return false;
    }
    seen.push(event_id.to_string());
    if seen.len() > EVENT_DEDUP_MAX {
        let drain = seen.len() - EVENT_DEDUP_MAX;
        seen.drain(..drain);
    }
    true
}

async fn handle_event(
    runtime: &Arc<Runtime>,
    dispatcher: &Arc<Dispatcher>,
    pairing: Option<&crate::pairing::PairingStore>,
    event: &Value,
) {
    let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if event_type == "postback" {
        handle_postback_event(runtime, event).await;
        return;
    }
    if event_type != "message" {
        return;
    }
    let event_id = event
        .get("webhookEventId")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if !remember_event(runtime, event_id).await {
        return;
    }
    let source = event.get("source").cloned().unwrap_or(json!({}));
    let (chat_id, chat_type) = resolve_chat(&source);
    if chat_id.is_empty() {
        return;
    }
    // Three-list gate ∪ pairing.
    if !allowed_for_source(&runtime.cfg, &source) {
        let sender_id = source
            .get("userId")
            .and_then(|v| v.as_str())
            .unwrap_or(&chat_id)
            .to_string();
        if let Some(store) = pairing {
            if !store.is_approved("line", &sender_id) {
                if let Some(code_msg) =
                    crate::messaging::pairing_offer_public(store, "line", &sender_id, &sender_id)
                {
                    let _ = push_text(runtime, &chat_id, &code_msg).await;
                }
            }
        } else {
            eprintln!("[line] unauthorized sender {sender_id} — add to the allowed lists");
        }
        return;
    }
    let reply_token = event
        .get("replyToken")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let message = event.get("message").cloned().unwrap_or(json!({}));
    let msg_type = message.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let message_id = message
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let mut text = String::new();
    let mut attachments = Vec::new();
    match msg_type {
        "text" => {
            text = message
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
        }
        "image" | "video" | "audio" | "file" => {
            match fetch_content(runtime, &message_id).await {
                Ok((data, content_type)) => {
                    let name = message
                        .get("fileName")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let mime: String = if content_type.is_empty() {
                        guess_mime(msg_type).to_string()
                    } else {
                        content_type
                    };
                    match crate::media_cache::cache_media_bytes(
                        &crate::config::ulnclaw_home(),
                        &data,
                        &mime,
                        name,
                    ) {
                        Ok(path) => attachments.push(MediaAttachment {
                            path,
                            mime: mime.to_string(),
                            bytes: data.len() as u64,
                            original_name: name.to_string(),
                        }),
                        Err(e) => eprintln!("[line] media cache failed: {e}"),
                    }
                }
                Err(e) => eprintln!("[line] content fetch failed: {e}"),
            }
            if let Some(caption) = message.get("text").and_then(|v| v.as_str()) {
                text = caption.trim().to_string();
            }
        }
        "sticker" => text = "[sticker]".to_string(),
        "location" => {
            let lat = message
                .get("latitude")
                .map(|v| v.to_string())
                .unwrap_or_default();
            let lng = message
                .get("longitude")
                .map(|v| v.to_string())
                .unwrap_or_default();
            text = format!("[location: {lat}, {lng}]");
        }
        _ => return,
    }
    if text.is_empty() && attachments.is_empty() {
        return;
    }
    let sender_id = source
        .get("userId")
        .and_then(|v| v.as_str())
        .unwrap_or(&chat_id)
        .to_string();
    // Stash the single-use reply token for the outbound path (hermes
    // `_reply_tokens`); consumed by send_reply or the slow-LLM button.
    stash_reply_token(runtime, &chat_id, &reply_token).await;
    // Best-effort loading indicator while the agent thinks.
    send_loading(runtime, &chat_id).await;
    let mut gate_event = MessageEvent {
        platform: "line".into(),
        chat_id: chat_id.clone(),
        sender_name: sender_id.clone(),
        sender_id,
        text,
        message_id,
        attachments,
    };
    if !crate::messaging::pre_gateway_dispatch_gate_public(&mut gate_event).await {
        return;
    }
    let dispatch = dispatcher.handle_event(gate_event);
    tokio::pin!(dispatch);
    let result = if runtime.cfg.slow_response_threshold > 0.0 {
        match tokio::time::timeout(
            Duration::from_secs_f64(runtime.cfg.slow_response_threshold),
            &mut dispatch,
        )
        .await
        {
            Ok(result) => result,
            Err(_) => {
                // Still running past threshold — offer the postback
                // button (hermes `_keep_typing` wrapper).
                fire_slow_postback_button(runtime, &chat_id).await;
                dispatch.await
            }
        }
    } else {
        dispatch.await
    };
    let outcome = match result {
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
    // Outstanding slow-LLM button: hand the reply to the postback cache
    // so the user fetches it by tap (system busy-acks bypass, hermes).
    if let Some(rid) = pending_button_for(runtime, &chat_id).await {
        if !is_system_bypass(&full) {
            set_postback_ready(runtime, &rid, &full).await;
            return;
        }
    }
    let (reply_text, media) = crate::messaging::extract_media_tags(&full);
    let reply_text = reply_text.trim().to_string();
    if !reply_text.is_empty() {
        // P705: ledger-protected reply delivery.
        dispatcher
            .send_with_ledger("line", &chat_id, &reply_text, || {
                send_reply(runtime, &chat_id, &reply_text)
            })
            .await;
    }
    for path in &media {
        send_media_file(runtime, &chat_id, path).await;
    }
    let _ = chat_type;
}

/// Reply-token-first delivery with Push fallback (hermes core flow).
async fn send_reply(runtime: &Runtime, chat_id: &str, content: &str) {
    let reply_token = take_reply_token(runtime, chat_id).await.unwrap_or_default();
    let formatted = strip_markdown_preserving_urls(content);
    let chunks = split_for_line(&formatted, LINE_SAFE_BUBBLE_CHARS);
    if chunks.is_empty() {
        return;
    }
    for batch in chunks.chunks(LINE_MAX_MESSAGES_PER_CALL) {
        let messages: Vec<Value> = batch
            .iter()
            .map(|t| json!({ "type": "text", "text": t }))
            .collect();
        if !reply_token.is_empty() {
            let ok = runtime
                .client
                .post(LINE_REPLY_URL)
                .bearer_auth(&runtime.cfg.channel_access_token)
                .json(&json!({ "replyToken": reply_token, "messages": messages }))
                .send()
                .await
                .map(|r| r.status().is_success())
                .unwrap_or(false);
            if ok {
                continue;
            }
            eprintln!("[line] reply token rejected — falling back to push");
        }
        if let Err(e) = push_messages(runtime, chat_id, &messages).await {
            eprintln!("[line] push to {chat_id} failed: {e}");
        }
    }
}

async fn push_messages(runtime: &Runtime, chat_id: &str, messages: &[Value]) -> Result<(), String> {
    let resp = runtime
        .client
        .post(LINE_PUSH_URL)
        .bearer_auth(&runtime.cfg.channel_access_token)
        .json(&json!({ "to": chat_id, "messages": messages }))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.status().is_success() {
        Ok(())
    } else {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        Err(format!("HTTP {status}: {}", &body[..body.len().min(200)]))
    }
}

async fn push_text(runtime: &Runtime, chat_id: &str, text: &str) -> Result<(), String> {
    let messages: Vec<Value> = split_for_line(text, LINE_SAFE_BUBBLE_CHARS)
        .into_iter()
        .map(|t| json!({ "type": "text", "text": t }))
        .collect();
    push_messages(runtime, chat_id, &messages).await
}

async fn send_loading(runtime: &Runtime, chat_id: &str) {
    let _ = runtime
        .client
        .post(LINE_LOADING_URL)
        .bearer_auth(&runtime.cfg.channel_access_token)
        .json(&json!({ "chatId": chat_id, "loadingSeconds": 60 }))
        .send()
        .await;
}

async fn fetch_content(runtime: &Runtime, message_id: &str) -> Result<(Vec<u8>, String), String> {
    let url = format!("{LINE_CONTENT_URL}/{message_id}/content");
    let resp = runtime
        .client
        .get(&url)
        .bearer_auth(&runtime.cfg.channel_access_token)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("content HTTP {}", resp.status()));
    }
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    Ok((bytes.to_vec(), content_type))
}

fn guess_mime(msg_type: &str) -> &'static str {
    match msg_type {
        "image" => "image/jpeg",
        "video" => "video/mp4",
        "audio" => "audio/mp4",
        _ => "application/octet-stream",
    }
}

struct LineSender {
    runtime: Arc<Runtime>,
}

#[async_trait::async_trait]
impl crate::messaging::PlatformSender for LineSender {
    async fn send_text(&self, chat_id: &str, text: &str) {
        if let Err(e) = push_text(&self.runtime, chat_id, text).await {
            eprintln!("[line] send_text to {chat_id} failed: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// Reply-token stash (hermes `_reply_tokens`)
// ---------------------------------------------------------------------------

async fn stash_reply_token(runtime: &Runtime, chat_id: &str, reply_token: &str) {
    if chat_id.is_empty() || reply_token.is_empty() {
        return;
    }
    runtime.reply_tokens.lock().await.insert(
        chat_id.to_string(),
        (
            reply_token.to_string(),
            std::time::Instant::now() + LINE_REPLY_TOKEN_TTL,
        ),
    );
}

/// Consume the stashed reply token when still unexpired (hermes
/// `_consume_reply_token`).
async fn take_reply_token(runtime: &Runtime, chat_id: &str) -> Option<String> {
    let (token, expiry) = runtime.reply_tokens.lock().await.remove(chat_id)?;
    if token.is_empty() || std::time::Instant::now() >= expiry {
        return None;
    }
    Some(token)
}

// ---------------------------------------------------------------------------
// Slow-LLM postback button (hermes PR #18153 state machine)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
enum PostbackState {
    Pending,
    Ready,
    Delivered,
    Error,
}

#[derive(Debug, Clone)]
struct PostbackEntry {
    state: PostbackState,
    payload: String,
    chat_id: String,
    created_at: std::time::Instant,
    updated_at: std::time::Instant,
}

fn prune_postbacks(cache: &mut std::collections::HashMap<String, PostbackEntry>) {
    cache.retain(|_, entry| {
        let ttl = if entry.state == PostbackState::Pending {
            POSTBACK_PENDING_TTL
        } else {
            POSTBACK_TTL
        };
        let reference = if entry.state == PostbackState::Pending {
            entry.created_at
        } else {
            entry.updated_at
        };
        reference.elapsed() < ttl
    });
}

async fn register_pending(runtime: &Runtime, chat_id: &str) -> String {
    let rid = uuid::Uuid::new_v4().to_string();
    let mut cache = runtime.postback_cache.lock().await;
    prune_postbacks(&mut cache);
    cache.insert(
        rid.clone(),
        PostbackEntry {
            state: PostbackState::Pending,
            payload: String::new(),
            chat_id: chat_id.to_string(),
            created_at: std::time::Instant::now(),
            updated_at: std::time::Instant::now(),
        },
    );
    rid
}

async fn set_postback_ready(runtime: &Runtime, rid: &str, payload: &str) {
    let mut cache = runtime.postback_cache.lock().await;
    if let Some(entry) = cache.get_mut(rid) {
        if entry.state == PostbackState::Pending {
            entry.state = PostbackState::Ready;
            entry.payload = payload.to_string();
            entry.updated_at = std::time::Instant::now();
        }
    }
}

async fn set_postback_error(runtime: &Runtime, rid: &str, message: &str) {
    let mut cache = runtime.postback_cache.lock().await;
    if let Some(entry) = cache.get_mut(rid) {
        if entry.state == PostbackState::Pending {
            entry.state = PostbackState::Error;
            entry.payload = message.to_string();
            entry.updated_at = std::time::Instant::now();
        }
    }
}

async fn mark_postback_delivered(runtime: &Runtime, rid: &str) {
    let mut cache = runtime.postback_cache.lock().await;
    if let Some(entry) = cache.get_mut(rid) {
        if matches!(entry.state, PostbackState::Ready | PostbackState::Error) {
            entry.state = PostbackState::Delivered;
            entry.updated_at = std::time::Instant::now();
        }
    }
}

async fn postback_snapshot(runtime: &Runtime, rid: &str) -> Option<(PostbackState, String)> {
    runtime
        .postback_cache
        .lock()
        .await
        .get(rid)
        .map(|entry| (entry.state, entry.payload.clone()))
}

async fn pending_button_for(runtime: &Runtime, chat_id: &str) -> Option<String> {
    runtime.pending_buttons.lock().await.get(chat_id).cloned()
}

/// hermes `RequestCache.find_pending_for_chat` — orphan PENDING lookup
/// by chat.
async fn find_pending_for_chat(runtime: &Runtime, chat_id: &str) -> Option<String> {
    runtime
        .postback_cache
        .lock()
        .await
        .iter()
        .find(|(_, entry)| entry.state == PostbackState::Pending && entry.chat_id == chat_id)
        .map(|(rid, _)| rid.clone())
}

/// hermes `_SYSTEM_BYPASS_PREFIXES` — busy-acks stay visible even with
/// an outstanding postback button.
fn is_system_bypass(content: &str) -> bool {
    const PREFIXES: [&str; 4] = [
        "\u{26A1} Interrupting",
        "\u{23F3} Queued",
        "\u{23E9} Steered",
        "\u{1F4BE}",
    ];
    !content.is_empty() && PREFIXES.iter().any(|p| content.starts_with(p))
}

/// hermes `build_postback_button_message` — Template Buttons bubble
/// (text ≤ 160 chars, altText ≤ 400, label ≤ 20).
pub fn build_postback_button_message(text: &str, button_label: &str, request_id: &str) -> Value {
    let truncated: String = if text.chars().count() <= 160 {
        text.to_string()
    } else {
        text.chars().take(157).collect::<String>() + "..."
    };
    let alt: String = if text.chars().count() <= 400 {
        text.to_string()
    } else {
        text.chars().take(397).collect::<String>() + "..."
    };
    let label: String = if button_label.chars().count() <= 20 {
        button_label.to_string()
    } else {
        button_label.chars().take(20).collect()
    };
    let label = if label.is_empty() {
        "Get answer".into()
    } else {
        label
    };
    let display: String = button_label.chars().take(300).collect();
    let display = if display.is_empty() {
        "Get answer".into()
    } else {
        display
    };
    json!({
        "type": "template",
        "altText": alt,
        "template": {
            "type": "buttons",
            "text": truncated,
            "actions": [{
                "type": "postback",
                "label": label,
                "data": serde_json::to_string(&json!({
                    "action": "show_response",
                    "request_id": request_id,
                }))
                .unwrap_or_default(),
                "displayText": display,
            }],
        },
    })
}

/// Fire the slow-LLM button once the threshold elapses (hermes
/// `_fire_postback`): requires an unconsumed reply token and at most
/// one outstanding button per chat.
async fn fire_slow_postback_button(runtime: &Runtime, chat_id: &str) {
    if pending_button_for(runtime, chat_id).await.is_some()
        || find_pending_for_chat(runtime, chat_id).await.is_some()
    {
        return;
    }
    let Some(token) = take_reply_token(runtime, chat_id).await else {
        return;
    };
    let rid = register_pending(runtime, chat_id).await;
    let message =
        build_postback_button_message(&runtime.cfg.pending_text, &runtime.cfg.button_label, &rid);
    match reply_raw(runtime, &token, &[message]).await {
        Ok(()) => {
            runtime
                .pending_buttons
                .lock()
                .await
                .insert(chat_id.to_string(), rid.clone());
            eprintln!("[line] sent slow-LLM postback button for {chat_id} (rid={rid})");
        }
        Err(e) => {
            eprintln!("[line] postback button send failed: {e}");
            set_postback_error(runtime, &rid, &runtime.cfg.interrupted_text).await;
        }
    }
}

/// Raw Reply API call (single-use token).
async fn reply_raw(runtime: &Runtime, reply_token: &str, messages: &[Value]) -> Result<(), String> {
    let resp = runtime
        .client
        .post(LINE_REPLY_URL)
        .bearer_auth(&runtime.cfg.channel_access_token)
        .json(&json!({ "replyToken": reply_token, "messages": messages }))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.status().is_success() {
        Ok(())
    } else {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        Err(format!("HTTP {status}: {}", &body[..body.len().min(200)]))
    }
}

/// User tapped the slow-LLM postback button — deliver the cached
/// payload (hermes `_handle_postback_event`).
async fn handle_postback_event(runtime: &Runtime, event: &Value) {
    let data = event
        .pointer("/postback/data")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let reply_token = event
        .get("replyToken")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let source = event.get("source").cloned().unwrap_or(json!({}));
    let (chat_id, _) = resolve_chat(&source);
    let parsed: Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(_) => return,
    };
    if parsed.get("action").and_then(|v| v.as_str()) != Some("show_response") {
        return;
    }
    let rid = match parsed.get("request_id").and_then(|v| v.as_str()) {
        Some(rid) if !rid.is_empty() => rid.to_string(),
        _ => return,
    };
    let Some((state, payload)) = postback_snapshot(runtime, &rid).await else {
        return;
    };
    match state {
        PostbackState::Ready => {
            let formatted = strip_markdown_preserving_urls(&payload);
            let chunks = split_for_line(&formatted, LINE_SAFE_BUBBLE_CHARS);
            let messages: Vec<Value> = chunks
                .iter()
                .take(LINE_MAX_MESSAGES_PER_CALL)
                .map(|t| json!({ "type": "text", "text": t }))
                .collect();
            if messages.is_empty() {
                return;
            }
            let delivered = if !reply_token.is_empty() {
                match reply_raw(runtime, &reply_token, &messages).await {
                    Ok(()) => true,
                    Err(e) => {
                        eprintln!("[line] postback reply failed ({e}); falling back to push");
                        push_messages(runtime, &chat_id, &messages).await.is_ok()
                    }
                }
            } else {
                push_messages(runtime, &chat_id, &messages).await.is_ok()
            };
            if delivered {
                mark_postback_delivered(runtime, &rid).await;
                runtime.pending_buttons.lock().await.remove(&chat_id);
            }
        }
        PostbackState::Error => {
            let text = if payload.is_empty() {
                runtime.cfg.interrupted_text.clone()
            } else {
                payload
            };
            if !reply_token.is_empty() {
                let _ = reply_raw(
                    runtime,
                    &reply_token,
                    &[json!({ "type": "text", "text": text })],
                )
                .await;
            }
            mark_postback_delivered(runtime, &rid).await;
            runtime.pending_buttons.lock().await.remove(&chat_id);
        }
        PostbackState::Delivered => {
            if !reply_token.is_empty() {
                let _ = reply_raw(
                    runtime,
                    &reply_token,
                    &[json!({ "type": "text", "text": runtime.cfg.delivered_text })],
                )
                .await;
            }
        }
        PostbackState::Pending => {
            if !reply_token.is_empty() {
                let _ = reply_raw(
                    runtime,
                    &reply_token,
                    &[json!({ "type": "text", "text": runtime.cfg.pending_text })],
                )
                .await;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Outbound media (hermes send_image_file / send_voice / send_video)
// ---------------------------------------------------------------------------

/// 1×1 transparent PNG — fallback video preview (hermes
/// `_FALLBACK_PNG_PREVIEW`, LINE requires `previewImageUrl`).
const FALLBACK_PNG_PREVIEW: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x00, 0x37, 0x7a, 0x7f, 0xf2, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44,
    0xae, 0x42, 0x60, 0x82,
];

/// Register a local file for HTTPS serving; returns the URL token
/// (hermes `_register_media`, expired tokens evicted first).
async fn register_media(runtime: &Runtime, path: &std::path::Path) -> String {
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let token = uuid::Uuid::new_v4().simple().to_string();
    let mut tokens = runtime.media_tokens.lock().await;
    tokens.retain(|_, (_, expiry)| expiry.elapsed() < MEDIA_TOKEN_TTL);
    tokens.insert(
        token.clone(),
        (resolved, std::time::Instant::now() + MEDIA_TOKEN_TTL),
    );
    token
}

/// Public HTTPS URL for a media token (hermes `_media_url`).
pub fn media_url(public_url: &str, token: &str, filename: &str) -> String {
    let safe_name: String = filename
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => c.to_string(),
            _ => format!("%{:02X}", c as u32),
        })
        .collect();
    format!("{public_url}{MEDIA_PATH_PREFIX}/{token}/{safe_name}")
}

/// Send one `MEDIA:` file as the matching LINE message type (hermes
/// `send_image_file`/`send_voice`/`send_video`).
async fn send_media_file(runtime: &Runtime, chat_id: &str, path: &std::path::Path) {
    if runtime.cfg.public_url.is_empty() {
        eprintln!(
            "[line] media send skipped: LINE only accepts public HTTPS media URLs — set [messaging.line] public_url (LINE_PUBLIC_URL)"
        );
        return;
    }
    let metadata = match std::fs::metadata(path) {
        Ok(m) if m.is_file() => m,
        _ => {
            eprintln!("[line] media file not found: {}", path.display());
            return;
        }
    };
    let mime = crate::media_cache::mime_for_ext(path);
    let filename = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "media".into());
    let kind = mime.split('/').next().unwrap_or("");
    let message = match kind {
        "image" => {
            if metadata.len() > LINE_IMAGE_MAX_BYTES {
                eprintln!("[line] image exceeds 10 MB LINE limit: {}", path.display());
                return;
            }
            let token = register_media(runtime, path).await;
            json!({
                "type": "image",
                "originalContentUrl": media_url(&runtime.cfg.public_url, &token, &filename),
                "previewImageUrl": media_url(&runtime.cfg.public_url, &token, &filename),
            })
        }
        "audio" => {
            if metadata.len() > LINE_AV_MAX_BYTES {
                eprintln!("[line] audio exceeds 200 MB LINE limit: {}", path.display());
                return;
            }
            let token = register_media(runtime, path).await;
            json!({
                "type": "audio",
                "originalContentUrl": media_url(&runtime.cfg.public_url, &token, &filename),
                "duration": 1000,
            })
        }
        "video" => {
            if metadata.len() > LINE_AV_MAX_BYTES {
                eprintln!("[line] video exceeds 200 MB LINE limit: {}", path.display());
                return;
            }
            let token = register_media(runtime, path).await;
            // LINE requires previewImageUrl — serve a 1×1 PNG from the
            // media cache (content-addressed, allowed root).
            let preview_path = crate::media_cache::cache_media_bytes(
                &crate::config::ulnclaw_home(),
                FALLBACK_PNG_PREVIEW,
                "image/png",
                "preview.png",
            );
            let preview_url = match preview_path {
                Ok(preview) => {
                    let preview_token = register_media(runtime, &preview).await;
                    media_url(&runtime.cfg.public_url, &preview_token, "preview.png")
                }
                Err(e) => {
                    eprintln!("[line] video preview cache failed: {e}");
                    return;
                }
            };
            json!({
                "type": "video",
                "originalContentUrl": media_url(&runtime.cfg.public_url, &token, &filename),
                "previewImageUrl": preview_url,
            })
        }
        _ => {
            eprintln!(
                "[line] unsupported media type for {}: {mime}",
                path.display()
            );
            return;
        }
    };
    if let Err(e) = push_messages(runtime, chat_id, &[message]).await {
        eprintln!("[line] media send to {chat_id} failed: {e}");
    }
}

/// Gateway media lookup result.
pub enum LineMediaResult {
    Found(Vec<u8>, String),
    NotFound,
    Gone,
    Forbidden,
}

/// Serve a registered media token (gateway `/line/media/:token/...`
/// route; hermes `_handle_media` with the allowed-roots defence).
pub async fn line_serve_media(token: &str) -> LineMediaResult {
    let Some(runtime) = runtime() else {
        return LineMediaResult::NotFound;
    };
    let entry = {
        let tokens = runtime.media_tokens.lock().await;
        tokens.get(token).cloned()
    };
    let Some((path, expiry)) = entry else {
        return LineMediaResult::NotFound;
    };
    if std::time::Instant::now() >= expiry {
        runtime.media_tokens.lock().await.remove(token);
        return LineMediaResult::Gone;
    }
    if !path.is_file() {
        return LineMediaResult::NotFound;
    }
    let canonical = match std::fs::canonicalize(&path) {
        Ok(c) => c,
        Err(_) => return LineMediaResult::NotFound,
    };
    let mut roots = vec![crate::config::ulnclaw_home()];
    if let Some(tmp) = std::env::temp_dir().canonicalize().ok() {
        roots.push(tmp);
    }
    let allowed = roots.iter().any(|root| {
        std::fs::canonicalize(root)
            .map(|r| canonical.starts_with(&r))
            .unwrap_or(false)
    });
    if !allowed {
        eprintln!(
            "[line] refusing to serve media outside allowed roots: {}",
            canonical.display()
        );
        return LineMediaResult::Forbidden;
    }
    match std::fs::read(&canonical) {
        Ok(bytes) => {
            let mime = crate::media_cache::mime_for_ext(&canonical);
            LineMediaResult::Found(bytes, mime)
        }
        Err(_) => LineMediaResult::NotFound,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig_for(body: &[u8], secret: &str) -> String {
        let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
    }

    #[test]
    fn signature_roundtrip() {
        let body = br#"{"events":[]}"#;
        let sig = sig_for(body, "secret123");
        assert!(verify_line_signature(body, &sig, "secret123"));
        assert!(!verify_line_signature(body, &sig, "wrong"));
        assert!(!verify_line_signature(body, "bogus", "secret123"));
        assert!(!verify_line_signature(body, "", "secret123"));
        assert!(!verify_line_signature(body, &sig, ""));
    }

    #[test]
    fn resolve_chat_sources() {
        let (id, t) = resolve_chat(&json!({"type":"user","userId":"U123"}));
        assert_eq!((id.as_str(), t), ("U123", "dm"));
        let (id, t) = resolve_chat(&json!({"type":"group","groupId":"C456","userId":"U1"}));
        assert_eq!((id.as_str(), t), ("C456", "group"));
        let (id, t) = resolve_chat(&json!({"type":"room","roomId":"R789"}));
        assert_eq!((id.as_str(), t), ("R789", "room"));
        let (id, _) = resolve_chat(&json!({}));
        assert!(id.is_empty());
    }

    #[test]
    fn three_list_gate() {
        let cfg = ResolvedLine {
            channel_access_token: String::new(),
            channel_secret: String::new(),
            allowed_users: vec!["U1".into()],
            allowed_groups: vec!["C1".into()],
            allowed_rooms: Vec::new(),
            allow_all_users: false,
            home_channel: String::new(),
            public_url: String::new(),
            slow_response_threshold: 45.0,
            pending_text: String::new(),
            button_label: String::new(),
            delivered_text: String::new(),
            interrupted_text: String::new(),
        };
        assert!(allowed_for_source(
            &cfg,
            &json!({"type":"user","userId":"U1"})
        ));
        assert!(!allowed_for_source(
            &cfg,
            &json!({"type":"user","userId":"U2"})
        ));
        assert!(allowed_for_source(
            &cfg,
            &json!({"type":"group","groupId":"C1"})
        ));
        assert!(!allowed_for_source(
            &cfg,
            &json!({"type":"room","roomId":"R1"})
        ));
        let mut open = cfg.clone();
        open.allow_all_users = true;
        assert!(allowed_for_source(
            &open,
            &json!({"type":"room","roomId":"R1"})
        ));
    }

    #[test]
    fn markdown_stripping_preserves_urls() {
        let out = strip_markdown_preserving_urls("**bold** and `code`");
        assert_eq!(out, "bold and code");
        let out = strip_markdown_preserving_urls("[docs](https://example.com/a)");
        assert_eq!(out, "docs (https://example.com/a)");
        let out = strip_markdown_preserving_urls("## Title\n- item");
        assert_eq!(out, "Title\n• item");
        let out = strip_markdown_preserving_urls("```\ncode line\n```");
        assert_eq!(out, "code line");
        // Bare URLs stay untouched (LINE auto-links them).
        let out = strip_markdown_preserving_urls("see https://example.com now");
        assert_eq!(out, "see https://example.com now");
    }

    #[test]
    fn split_respects_budget_and_ellipsis() {
        // 30000 chars > 5 x 4500 budget → final chunk truncated with an
        // ellipsis (hermes split_for_line semantics).
        let text = "word ".repeat(6000);
        let chunks = split_for_line(text.trim(), LINE_SAFE_BUBBLE_CHARS);
        assert!(chunks.len() <= LINE_MAX_MESSAGES_PER_CALL);
        for chunk in &chunks[..chunks.len() - 1] {
            assert!(chunk.chars().count() <= LINE_SAFE_BUBBLE_CHARS);
        }
        assert!(chunks.last().unwrap().ends_with('…'));
        // Short text passes through.
        assert_eq!(split_for_line("hi", LINE_SAFE_BUBBLE_CHARS), vec!["hi"]);
    }

    #[test]
    fn split_prefers_paragraph_breaks() {
        let para = "a".repeat(100);
        let text = format!("{para}\n\n{para}\n\n{para}");
        let chunks = split_for_line(&text, 220);
        assert!(chunks.len() >= 2);
        assert!(chunks[0].chars().count() <= 220);
    }

    #[test]
    fn resolve_env_precedence() {
        let _guard = crate::models_dev::test_env_lock();
        std::env::set_var("LINE_CHANNEL_SECRET", "env-secret");
        std::env::set_var("LINE_ALLOWED_GROUPS", "C1, C2");
        let cfg = LineConfig::default();
        let resolved = cfg.resolve();
        assert_eq!(resolved.channel_secret, "env-secret");
        assert_eq!(
            resolved.allowed_groups,
            vec!["C1".to_string(), "C2".to_string()]
        );
        std::env::remove_var("LINE_CHANNEL_SECRET");
        std::env::remove_var("LINE_ALLOWED_GROUPS");
    }

    #[test]
    fn webhook_body_limit_matches_hermes() {
        assert_eq!(WEBHOOK_BODY_MAX_BYTES, 1_048_576);
        assert_eq!(LINE_SAFE_BUBBLE_CHARS, 4500);
        assert_eq!(LINE_MAX_MESSAGES_PER_CALL, 5);
        assert_eq!(MEDIA_TOKEN_TTL, Duration::from_secs(1800));
        assert_eq!(LINE_IMAGE_MAX_BYTES, 10 * 1024 * 1024);
    }

    #[test]
    fn postback_button_message_shape() {
        let msg = build_postback_button_message("answer pending", "Get answer", "rid-1");
        assert_eq!(msg["type"], "template");
        assert_eq!(msg["altText"], "answer pending");
        assert_eq!(msg["template"]["type"], "buttons");
        assert_eq!(msg["template"]["text"], "answer pending");
        let action = &msg["template"]["actions"][0];
        assert_eq!(action["type"], "postback");
        assert_eq!(action["label"], "Get answer");
        let data: Value = serde_json::from_str(action["data"].as_str().unwrap()).unwrap();
        assert_eq!(data["action"], "show_response");
        assert_eq!(data["request_id"], "rid-1");
        // Long text truncated to LINE limits (160 / 400 chars).
        let long = "x".repeat(500);
        let msg = build_postback_button_message(&long, "Get answer", "rid-2");
        assert_eq!(
            msg["template"]["text"].as_str().unwrap().chars().count(),
            160
        );
        assert_eq!(msg["altText"].as_str().unwrap().chars().count(), 400);
    }

    #[test]
    fn system_bypass_prefixes() {
        assert!(is_system_bypass("\u{26A1} Interrupting run"));
        assert!(is_system_bypass("\u{23F3} Queued"));
        assert!(is_system_bypass("\u{23E9} Steered"));
        assert!(is_system_bypass("\u{1F4BE} background review"));
        assert!(!is_system_bypass("normal reply"));
        assert!(!is_system_bypass(""));
    }

    #[tokio::test]
    async fn postback_cache_state_machine() {
        let runtime = Arc::new(Runtime {
            cfg: LineConfig::default().resolve(),
            client: reqwest::Client::new(),
            seen_events: Mutex::new(Vec::new()),
            reply_tokens: Mutex::new(std::collections::HashMap::new()),
            postback_cache: Mutex::new(std::collections::HashMap::new()),
            pending_buttons: Mutex::new(std::collections::HashMap::new()),
            media_tokens: Mutex::new(std::collections::HashMap::new()),
        });
        let rid = register_pending(&runtime, "U1").await;
        assert_eq!(
            postback_snapshot(&runtime, &rid).await.unwrap().0,
            PostbackState::Pending
        );
        set_postback_ready(&runtime, &rid, "the answer").await;
        let (state, payload) = postback_snapshot(&runtime, &rid).await.unwrap();
        assert_eq!(state, PostbackState::Ready);
        assert_eq!(payload, "the answer");
        // set_ready twice is a no-op once READY.
        set_postback_ready(&runtime, &rid, "other").await;
        assert_eq!(
            postback_snapshot(&runtime, &rid).await.unwrap().1,
            "the answer"
        );
        mark_postback_delivered(&runtime, &rid).await;
        assert_eq!(
            postback_snapshot(&runtime, &rid).await.unwrap().0,
            PostbackState::Delivered
        );
        // ERROR path only from PENDING.
        let rid2 = register_pending(&runtime, "U1").await;
        set_postback_error(&runtime, &rid2, "interrupted").await;
        let (state, payload) = postback_snapshot(&runtime, &rid2).await.unwrap();
        assert_eq!(state, PostbackState::Error);
        assert_eq!(payload, "interrupted");
    }

    #[tokio::test]
    async fn reply_token_stash_expiry() {
        let runtime = Arc::new(Runtime {
            cfg: LineConfig::default().resolve(),
            client: reqwest::Client::new(),
            seen_events: Mutex::new(Vec::new()),
            reply_tokens: Mutex::new(std::collections::HashMap::new()),
            postback_cache: Mutex::new(std::collections::HashMap::new()),
            pending_buttons: Mutex::new(std::collections::HashMap::new()),
            media_tokens: Mutex::new(std::collections::HashMap::new()),
        });
        stash_reply_token(&runtime, "U1", "tok-1").await;
        assert_eq!(
            take_reply_token(&runtime, "U1").await.as_deref(),
            Some("tok-1")
        );
        // Consumed — second take is empty.
        assert!(take_reply_token(&runtime, "U1").await.is_none());
        // Expired tokens are dropped.
        runtime.reply_tokens.lock().await.insert(
            "U2".into(),
            (
                "tok-2".into(),
                std::time::Instant::now() - Duration::from_secs(1),
            ),
        );
        assert!(take_reply_token(&runtime, "U2").await.is_none());
    }

    #[test]
    fn media_url_shape() {
        let url = media_url("https://bot.example.com", "tok_1", "my file.png");
        assert_eq!(
            url,
            "https://bot.example.com/line/media/tok_1/my%20file.png"
        );
    }

    #[tokio::test]
    async fn media_lookup_unknown_token_is_404() {
        assert!(matches!(
            line_serve_media("nope").await,
            LineMediaResult::NotFound
        ));
    }

    #[test]
    fn slow_threshold_env_override() {
        let _guard = crate::models_dev::test_env_lock();
        std::env::set_var("LINE_SLOW_RESPONSE_THRESHOLD", "12.5");
        std::env::set_var("LINE_PUBLIC_URL", "https://bot.example.com/");
        let resolved = LineConfig::default().resolve();
        assert_eq!(resolved.slow_response_threshold, 12.5);
        assert_eq!(resolved.public_url, "https://bot.example.com");
        std::env::remove_var("LINE_SLOW_RESPONSE_THRESHOLD");
        std::env::remove_var("LINE_PUBLIC_URL");
    }
}

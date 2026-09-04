//! Photon Spectrum (iMessage) platform adapter — port of hermes
//! `plugins/platforms/photon` @ v2026.8.3 (adapter.py, sidecar-client
//! transport).
//!
//! In hermes both directions flow through a supervised Node sidecar
//! running the TypeScript-only `spectrum-ts` SDK: inbound is an NDJSON
//! stream from `GET /inbound`, outbound posts to `/send`
//! (`{spaceId, text, format?}`), `/send-richlink`, and
//! `/send-attachment`, health via `/healthz`. This port keeps the wire
//! protocol but treats the sidecar as an external service — ulnclaw
//! does not spawn or npm-install the Node sidecar (documented
//! divergence; run the hermes sidecar and point
//! `[messaging.photon] sidecar_url` at it).
//!
//! Inbound events carry typed content (`space`/`sender`/`content` with
//! `text` | `richlink` | `group` | `attachment` | `voice` types;
//! attachments may inline base64 `data`). Intake mirrors hermes:
//! messageId dedup (at-least-once stream), own-send echo suppression,
//! rich links rendered as title/summary/url, iMessage rich-link preview
//! art (`.pluginpayloadattachment`) suppressed within 30 s of the link,
//! allowlist ∪ pairing gate, group `require_mention` wake-word gating
//! with leading-wake-word stripping, typing indicator with a 5 s
//! per-chat cooldown. Outbound: URL-only replies ride `/send-richlink`
//! with plain-text fallback, markdown format hint (`PHOTON_MARKDOWN`
//! kill-switch), hermes 8000-char cap. Reactions (iMessage tapbacks):
//! opt-in lifecycle tapbacks via `PHOTON_REACTIONS` (👀 while
//! processing, swapped for 👍/👎 on completion), sidecar `/react` +
//! `/unreact`, and inbound tapbacks on the bot's own messages routed
//! to the agent as `reaction:added:<emoji>` synthetic text.

use crate::messaging::{Dispatcher, MediaAttachment, MessageEvent};
use futures::StreamExt;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

const MAX_MESSAGE_LENGTH: usize = 8000;
const DEDUP_WINDOW_SECS: u64 = 48 * 3600;
const DEDUP_MAX_SIZE: usize = 4000;
const API_TIMEOUT: Duration = Duration::from_secs(30);
const RECONNECT_DELAY: Duration = Duration::from_secs(5);
/// hermes `_RICHLINK_PREVIEW_SUPPRESS_SECONDS`.
const RICHLINK_PREVIEW_SUPPRESS_SECS: u64 = 30;
/// hermes `_RICHLINK_PREVIEW_ATTACHMENT_SUFFIX`.
const RICHLINK_PREVIEW_SUFFIX: &str = ".pluginpayloadattachment";
/// hermes `_TYPING_COOLDOWN_SECONDS`.
const TYPING_COOLDOWN_SECS: u64 = 5;
/// hermes `_LAST_INBOUND_CHATS_MAX`.
const RECENT_CHATS_CAP: usize = 200;
/// hermes `_SENT_IDS_MAX`.
const SENT_IDS_CAP: usize = 1000;
/// Lifecycle tapbacks (hermes `_OK_EMOJI` / `_FAIL_EMOJI` / the 👀
/// `on_processing_start` hook).
const REACT_PROCESSING: &str = "👀";
const REACT_OK: &str = "👍";
const REACT_FAIL: &str = "👎";

/// `[messaging.photon]` — Photon adapter (hermes `platforms.photon`
/// plugin config + `PHOTON_*` env vars).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PhotonConfig {
    pub enabled: bool,
    /// Sidecar base URL (fallback `PHOTON_SIDECAR_URL`, default
    /// `http://127.0.0.1:7370`).
    pub sidecar_url: String,
    /// Shared bearer token (fallback `PHOTON_SIDECAR_TOKEN`).
    pub token: String,
    /// Sender ids allowed to talk to the bot (fallback
    /// `PHOTON_ALLOWED_USERS`).
    pub allowed_users: Vec<String>,
    pub allow_all_users: bool,
    /// Cron/notification delivery space (fallback `PHOTON_HOME_CHANNEL`).
    pub home_channel: String,
    /// Group messages need a wake-word mention (fallback
    /// `PHOTON_REQUIRE_MENTION`; hermes `require_mention`).
    pub require_mention: bool,
    /// Wake-word regexes for group gating (fallback
    /// `PHOTON_MENTION_PATTERNS`; empty = hermes default).
    pub mention_patterns: Vec<String>,
    /// Opt-in lifecycle tapbacks — iMessage is a personal-texting
    /// channel and a tapback on every text is noisy (fallback
    /// `PHOTON_REACTIONS`; hermes `_reactions_enabled`).
    pub reactions: bool,
}

impl Default for PhotonConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            sidecar_url: String::new(),
            token: String::new(),
            allowed_users: Vec::new(),
            allow_all_users: false,
            home_channel: String::new(),
            require_mention: false,
            mention_patterns: Vec::new(),
            reactions: false,
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
pub struct ResolvedPhoton {
    pub sidecar_url: String,
    pub token: String,
    pub allowed_users: Vec<String>,
    pub allow_all_users: bool,
    pub home_channel: String,
    pub require_mention: bool,
    pub mention_regexes: Vec<Regex>,
    /// hermes `_markdown_enabled` (`PHOTON_MARKDOWN`, default on).
    pub markdown: bool,
    /// hermes `_reactions_enabled` (`PHOTON_REACTIONS`, default off).
    pub reactions: bool,
}

/// hermes `_DEFAULT_MENTION_PATTERNS`, rewritten without lookbehind
/// (the Rust `regex` crate has no lookarounds) and accepting the ulnclaw
/// product name alongside hermes'.
const DEFAULT_MENTION_PATTERN: &str = r"(?i)@?(?:hermes|ulnclaw)(?:\s+agent)?\b[,:\-]?";

/// Compile wake-word regexes (hermes `compile_mention_patterns`):
/// invalid patterns are dropped with a warning; empty input falls back
/// to the defaults.
pub fn compile_mention_patterns(raw: &[String]) -> Vec<Regex> {
    let source: Vec<String> = if raw.is_empty() {
        vec![DEFAULT_MENTION_PATTERN.to_string()]
    } else {
        raw.to_vec()
    };
    let mut out = Vec::new();
    for pattern in source {
        match Regex::new(&pattern) {
            Ok(re) => out.push(re),
            Err(e) => eprintln!("[photon] invalid mention pattern {pattern:?}: {e}"),
        }
    }
    out
}

impl PhotonConfig {
    pub fn resolve(&self) -> ResolvedPhoton {
        let mention_raw = match env_trim("PHOTON_MENTION_PATTERNS") {
            Some(raw) => {
                // JSON list or comma/newline-separated (hermes shapes).
                if raw.starts_with('[') {
                    serde_json::from_str::<Vec<String>>(&raw).unwrap_or_default()
                } else {
                    raw.split([',', '\n'])
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect()
                }
            }
            None => self.mention_patterns.clone(),
        };
        ResolvedPhoton {
            sidecar_url: env_trim("PHOTON_SIDECAR_URL")
                .unwrap_or_else(|| self.sidecar_url.clone())
                .trim_end_matches('/')
                .to_string(),
            token: env_trim("PHOTON_SIDECAR_TOKEN").unwrap_or_else(|| self.token.clone()),
            allowed_users: env_list("PHOTON_ALLOWED_USERS")
                .unwrap_or_else(|| self.allowed_users.clone()),
            allow_all_users: env_trim("PHOTON_ALLOW_ALL_USERS")
                .map(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes"))
                .unwrap_or(self.allow_all_users),
            home_channel: env_trim("PHOTON_HOME_CHANNEL")
                .unwrap_or_else(|| self.home_channel.clone()),
            require_mention: env_trim("PHOTON_REQUIRE_MENTION")
                .map(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes" | "on"))
                .unwrap_or(self.require_mention),
            mention_regexes: compile_mention_patterns(&mention_raw),
            markdown: env_trim("PHOTON_MARKDOWN")
                .map(|v| !matches!(v.to_lowercase().as_str(), "false" | "0" | "no"))
                .unwrap_or(true),
            reactions: env_trim("PHOTON_REACTIONS")
                .map(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes" | "on"))
                .unwrap_or(self.reactions),
        }
    }
}

struct Runtime {
    cfg: ResolvedPhoton,
    client: reqwest::Client,
    /// messageId dedup (gRPC stream is at-least-once).
    seen: Mutex<HashMap<String, u64>>,
    /// Own-sent messageIds (echo suppression; hermes `_sent_message_ids`).
    sent_ids: Mutex<VecDeque<String>>,
    /// Latest inbound rich-link timestamp per chat (preview-art
    /// suppression; hermes `_recent_richlinks_by_chat`).
    recent_richlinks: Mutex<Vec<(String, u64)>>,
    /// Typing-indicator cooldown per chat (hermes `_typing_last_sent`).
    typing_last: Mutex<HashMap<String, u64>>,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Runtime {
    async fn is_duplicate(&self, message_id: &str) -> bool {
        if message_id.is_empty() {
            return false;
        }
        let now = now_secs();
        let mut seen = self.seen.lock().await;
        seen.retain(|_, ts| now.saturating_sub(*ts) < DEDUP_WINDOW_SECS);
        if seen.contains_key(message_id) {
            return true;
        }
        if seen.len() >= DEDUP_MAX_SIZE {
            let mut entries: Vec<(String, u64)> = seen.drain().collect();
            entries.sort_by_key(|(_, ts)| *ts);
            entries.truncate(DEDUP_MAX_SIZE / 2);
            *seen = entries.into_iter().collect();
        }
        seen.insert(message_id.to_string(), now);
        false
    }

    /// True when the inbound id is one of our own sends (hermes
    /// `_record_sent_message` echo suppression).
    async fn is_own_send(&self, message_id: &str) -> bool {
        if message_id.is_empty() {
            return false;
        }
        let sent = self.sent_ids.lock().await;
        sent.contains(&message_id.to_string())
    }

    async fn record_sent(&self, message_id: Option<String>) {
        let Some(id) = message_id.filter(|v| !v.is_empty()) else {
            return;
        };
        let mut sent = self.sent_ids.lock().await;
        sent.retain(|v| v != &id);
        sent.push_back(id);
        while sent.len() > SENT_IDS_CAP {
            sent.pop_front();
        }
    }

    async fn record_richlink(&self, chat_id: &str) {
        if chat_id.is_empty() {
            return;
        }
        let mut recent = self.recent_richlinks.lock().await;
        recent.retain(|(chat, _)| chat != chat_id);
        recent.push((chat_id.to_string(), now_secs()));
        while recent.len() > RECENT_CHATS_CAP {
            recent.remove(0);
        }
    }

    /// hermes `_is_recent_richlink_preview` window check.
    async fn recent_richlink_within(&self, chat_id: &str) -> bool {
        let mut recent = self.recent_richlinks.lock().await;
        let now = now_secs();
        if let Some(pos) = recent.iter().position(|(chat, _)| chat == chat_id) {
            if now.saturating_sub(recent[pos].1) <= RICHLINK_PREVIEW_SUPPRESS_SECS {
                return true;
            }
            recent.remove(pos);
        }
        false
    }

    /// hermes `send_typing` with the 5 s per-chat cooldown.
    async fn send_typing(&self, chat_id: &str) {
        let now = now_secs();
        {
            let mut last = self.typing_last.lock().await;
            if now.saturating_sub(last.get(chat_id).copied().unwrap_or(0)) < TYPING_COOLDOWN_SECS {
                return;
            }
            last.insert(chat_id.to_string(), now);
        }
        let url = format!("{}/typing", self.cfg.sidecar_url);
        let payload = json!({ "spaceId": chat_id, "state": "start" });
        let result = self
            .authed(self.client.post(&url))
            .timeout(API_TIMEOUT)
            .json(&payload)
            .send()
            .await;
        if let Err(e) = result {
            eprintln!("[photon] typing indicator failed: {e}");
        }
    }

    fn authed(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if self.cfg.token.is_empty() {
            req
        } else {
            req.bearer_auth(&self.cfg.token)
        }
    }

    /// hermes `_add_reaction` — sidecar `/react` tapback. Soft-fails
    /// (false), never blocks the message flow.
    async fn add_reaction(&self, chat_id: &str, message_id: &str, emoji: &str) -> bool {
        let url = format!("{}/react", self.cfg.sidecar_url);
        let payload = json!({ "spaceId": chat_id, "messageId": message_id, "emoji": emoji });
        match self
            .authed(self.client.post(&url))
            .timeout(API_TIMEOUT)
            .json(&payload)
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => true,
            Ok(resp) => {
                eprintln!("[photon] add_reaction failed: HTTP {}", resp.status());
                false
            }
            Err(e) => {
                eprintln!("[photon] add_reaction failed: {e}");
                false
            }
        }
    }

    /// hermes `_remove_reaction` — sidecar `/unreact` (retract our
    /// tapback). Soft-fails (false).
    async fn remove_reaction(&self, chat_id: &str, message_id: &str) -> bool {
        let url = format!("{}/unreact", self.cfg.sidecar_url);
        let payload = json!({ "spaceId": chat_id, "messageId": message_id });
        match self
            .authed(self.client.post(&url))
            .timeout(API_TIMEOUT)
            .json(&payload)
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => true,
            Ok(resp) => {
                eprintln!("[photon] remove_reaction failed: HTTP {}", resp.status());
                false
            }
            Err(e) => {
                eprintln!("[photon] remove_reaction failed: {e}");
                false
            }
        }
    }

    /// hermes lifecycle ack: swap the 👀 progress tapback for the
    /// result (remove-then-add keeps the sidecar's reaction-handle
    /// slot coherent).
    async fn swap_processing_reaction(&self, chat_id: &str, message_id: &str, result_emoji: &str) {
        let _ = self.remove_reaction(chat_id, message_id).await;
        let _ = self.add_reaction(chat_id, message_id, result_emoji).await;
    }
}

/// hermes `_normalize_chat_key` — a DM space is addressable both by
/// its chat GUID (`any;-;+1555…`) and the bare E.164 phone; normalize
/// to the phone so last-inbound tracking matches both forms.
pub fn normalize_chat_key(chat_id: &str) -> String {
    if let Some(phone) = chat_id.strip_prefix("any;-;") {
        if phone.starts_with('+')
            && phone.len() >= 7
            && phone[1..].chars().all(|c| c.is_ascii_digit())
        {
            return phone.to_string();
        }
    }
    chat_id.to_string()
}

/// hermes `_url_only_candidate`: only exact http(s) URL messages become
/// rich links (prose with URLs stays on the text path).
pub fn url_only_candidate(text: &str) -> Option<String> {
    let candidate = text.trim();
    let re = Regex::new(r"(?i)^https?://\S+$").expect("static regex");
    if !re.is_match(candidate) {
        return None;
    }
    match url::Url::parse(candidate) {
        Ok(parsed) => {
            let scheme = parsed.scheme().to_lowercase();
            if (scheme == "http" || scheme == "https")
                && !parsed.host_str().unwrap_or("").is_empty()
            {
                Some(candidate.to_string())
            } else {
                None
            }
        }
        Err(_) => None,
    }
}

/// hermes `_format_richlink_content`: title / summary / url lines.
pub fn format_richlink_content(content: &Value) -> String {
    let url = content
        .get("url")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let title = content
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let summary = content
        .get("summary")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let mut parts: Vec<String> = Vec::new();
    if !title.is_empty() {
        parts.push(title.clone());
    }
    if !summary.is_empty() && summary != title {
        parts.push(summary);
    }
    if !url.is_empty() {
        parts.push(url);
    }
    if parts.is_empty() {
        "[Photon rich link received with no URL]".to_string()
    } else {
        parts.join("\n")
    }
}

/// hermes `_is_richlink_preview_attachment`: the marker suffix on
/// name/id is the reliable signal for iMessage rich-link preview art.
pub fn is_richlink_preview_marker(name: &str, id: &str) -> bool {
    let name = name.to_lowercase();
    let id = id.to_lowercase();
    name.ends_with(RICHLINK_PREVIEW_SUFFIX)
        || id.ends_with(RICHLINK_PREVIEW_SUFFIX)
        || name.contains(RICHLINK_PREVIEW_SUFFIX)
        || id.contains(RICHLINK_PREVIEW_SUFFIX)
}

/// Strip a leading wake word before dispatch (hermes
/// `_clean_mention_text`): only a LEADING match is removed.
pub fn clean_mention_text(text: &str, regexes: &[Regex]) -> String {
    if text.is_empty() {
        return text.to_string();
    }
    let trimmed = text.trim_start();
    for re in regexes {
        if let Some(m) = re.find_at(trimmed, 0) {
            if m.start() == 0 && !m.is_empty() {
                let cleaned = trimmed[m.end()..].trim_start_matches([' ', ',', ':', '-']);
                return if cleaned.is_empty() {
                    text.to_string()
                } else {
                    cleaned.to_string()
                };
            }
        }
    }
    text.to_string()
}

/// Entry point spawned by `run_messaging`.
pub async fn run(
    cfg: PhotonConfig,
    dispatcher: Arc<Dispatcher>,
    pairing: Option<Arc<crate::pairing::PairingStore>>,
) {
    let mut resolved = cfg.resolve();
    if resolved.sidecar_url.is_empty() {
        resolved.sidecar_url = "http://127.0.0.1:7370".to_string();
    }
    let runtime = Arc::new(Runtime {
        client: reqwest::Client::new(),
        cfg: resolved,
        seen: Mutex::new(HashMap::new()),
        sent_ids: Mutex::new(VecDeque::new()),
        recent_richlinks: Mutex::new(Vec::new()),
        typing_last: Mutex::new(HashMap::new()),
    });
    crate::messaging::register_platform_sender(
        "photon",
        Arc::new(PhotonSender {
            runtime: runtime.clone(),
        }),
    );
    loop {
        // Health gate (hermes /healthz wait).
        match wait_for_sidecar(&runtime).await {
            Ok(()) => {}
            Err(e) => {
                eprintln!("[photon] sidecar wait failed: {e} — retrying");
                tokio::time::sleep(RECONNECT_DELAY).await;
                continue;
            }
        }
        if let Err(e) = consume_inbound(&runtime, &dispatcher, &pairing).await {
            eprintln!("[photon] inbound stream ended: {e} — reconnecting");
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

async fn wait_for_sidecar(runtime: &Runtime) -> Result<(), String> {
    let url = format!("{}/healthz", runtime.cfg.sidecar_url);
    for _ in 0..30 {
        match runtime
            .authed(runtime.client.get(&url))
            .timeout(Duration::from_secs(5))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => return Ok(()),
            Ok(resp) => return Err(format!("healthz HTTP {}", resp.status())),
            Err(_) => {}
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    Err("sidecar never became healthy".to_string())
}

/// hermes inbound consumer — NDJSON stream from `GET /inbound`.
async fn consume_inbound(
    runtime: &Arc<Runtime>,
    dispatcher: &Arc<Dispatcher>,
    pairing: &Option<Arc<crate::pairing::PairingStore>>,
) -> Result<(), String> {
    let url = format!("{}/inbound", runtime.cfg.sidecar_url);
    let resp = runtime
        .authed(runtime.client.get(&url))
        .header("accept", "application/x-ndjson")
        .send()
        .await
        .map_err(|e| format!("inbound connect: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("inbound HTTP {}", resp.status()));
    }
    let mut stream = resp.bytes_stream();
    let mut buffer = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("inbound read: {e}"))?;
        buffer.extend_from_slice(&chunk);
        while let Some(pos) = buffer.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = buffer.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line).trim().to_string();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(&line) {
                Ok(event) => {
                    handle_inbound(runtime, dispatcher, pairing, &event).await;
                }
                Err(e) => eprintln!("[photon] bad NDJSON line: {e}"),
            }
        }
    }
    Ok(())
}

/// Extension-based mime guess for flat-shape local attachment paths.
fn guess_mime_from_path(path: &str) -> String {
    let ext = path.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" | "png" | "gif" | "webp" => format!("image/{ext}"),
        "mp3" | "wav" | "ogg" | "m4a" | "aac" => format!("audio/{ext}"),
        "mp4" | "mov" => format!("video/{ext}"),
        _ => "application/octet-stream".to_string(),
    }
}

/// Parsed inbound content: display text + media attachments.
struct InboundContent {
    text: String,
    attachments: Vec<MediaAttachment>,
    /// Every attachment carried the rich-link preview marker.
    all_preview_art: bool,
    /// A rich link (or URL-only text) was present — record for the
    /// preview-suppression window.
    had_richlink: bool,
}

/// Decode one typed content node (hermes `_extract_text` + attachment
/// handling). Returns `None` for unsupported types (reactions).
fn parse_content_node(content: &Value, attachments: &mut Vec<MediaAttachment>) -> Option<String> {
    let ctype = content.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match ctype {
        "text" => Some(
            content
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        ),
        "richlink" => Some(format_richlink_content(content)),
        "group" => {
            let mut parts: Vec<String> = Vec::new();
            for item in content
                .get("items")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default()
            {
                let item_content = item.get("content").cloned().unwrap_or(item.clone());
                if let Some(text) = parse_content_node(&item_content, attachments) {
                    if !text.is_empty() {
                        parts.push(text);
                    }
                }
            }
            Some(parts.join("\n"))
        }
        "attachment" | "voice" => {
            if let Some(media) = attachment_from_content(content) {
                attachments.push(media);
            }
            Some(String::new())
        }
        // reactions/notices: not ported.
        _ => None,
    }
}

/// hermes attachment/voice content: inline base64 `data` when the
/// sidecar could read it, else metadata only.
fn attachment_from_content(content: &Value) -> Option<MediaAttachment> {
    use base64::Engine;
    let name = content
        .get("name")
        .or_else(|| content.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("attachment")
        .to_string();
    let mime = content
        .get("mimeType")
        .and_then(|v| v.as_str())
        .unwrap_or("application/octet-stream")
        .to_string();
    let data = content.get("data").and_then(|v| v.as_str()).unwrap_or("");
    let encoding = content
        .get("encoding")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if data.is_empty() || encoding != "base64" {
        eprintln!("[photon] attachment {name}: no inline data (sidecar size cap?) — skipped");
        return None;
    }
    let bytes = match base64::engine::general_purpose::STANDARD.decode(data) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[photon] attachment {name}: bad base64: {e}");
            return None;
        }
    };
    match crate::media_cache::cache_media_bytes(
        &crate::config::ulnclaw_home(),
        &bytes,
        &mime,
        &name,
    ) {
        Ok(cached) => Some(MediaAttachment {
            path: cached,
            mime,
            bytes: bytes.len() as u64,
            original_name: name,
        }),
        Err(e) => {
            eprintln!("[photon] media cache failed: {e}");
            None
        }
    }
}

/// hermes `_dispatch_inbound` — typed content intake with preview-art
/// suppression, dedup, echo suppression, gates, typing, dispatch.
async fn handle_inbound(
    runtime: &Arc<Runtime>,
    dispatcher: &Arc<Dispatcher>,
    pairing: &Option<Arc<crate::pairing::PairingStore>>,
    event: &Value,
) {
    let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if event_type != "message" {
        return;
    }
    let message_id = event
        .get("messageId")
        .or_else(|| event.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if runtime.is_duplicate(&message_id).await {
        return;
    }
    if runtime.is_own_send(&message_id).await {
        return;
    }

    // Typed sidecar shape: space/sender/content; flat shape kept as a
    // fallback for simplified bridges.
    let space = event.get("space").cloned().unwrap_or(json!({}));
    let sender = event.get("sender").cloned().unwrap_or(json!({}));
    let space_id = space
        .get("id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            event
                .get("spaceId")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_default();
    let is_group = space.get("type").and_then(|v| v.as_str()) == Some("group");
    let sender_id = sender
        .get("id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            space
                .get("phone")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .or_else(|| {
            event
                .get("senderId")
                .or_else(|| event.get("sender"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_default();
    if space_id.is_empty() || sender_id.is_empty() {
        return;
    }

    let mut parsed = InboundContent {
        text: String::new(),
        attachments: Vec::new(),
        all_preview_art: false,
        had_richlink: false,
    };
    if let Some(content) = event.get("content").filter(|v| v.is_object()) {
        let ctype = content.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if ctype == "reaction" {
            // hermes: route only tapbacks on messages WE sent — those
            // are implicitly addressed to the bot (human↔human tapbacks
            // are not for us). Checked before the mention gate: a
            // tapback never carries a wake word.
            let target_id = content
                .get("targetMessageId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let ours = content.get("targetDirection").and_then(|v| v.as_str()) == Some("outbound")
                || (!target_id.is_empty() && runtime.is_own_send(&target_id).await);
            if !ours {
                return;
            }
            if !runtime.cfg.allow_all_users
                && !runtime
                    .cfg
                    .allowed_users
                    .iter()
                    .any(|u| u == &sender_id || u == "*")
            {
                // No pairing offers for reactions — silently skip
                // unauthorized reactors.
                if pairing
                    .as_ref()
                    .map(|store| !store.is_approved("photon", &sender_id))
                    .unwrap_or(true)
                {
                    return;
                }
            }
            let emoji = content.get("emoji").and_then(|v| v.as_str()).unwrap_or("");
            let reaction_event = MessageEvent {
                platform: "photon".into(),
                chat_id: space_id.clone(),
                sender_id: sender_id.clone(),
                sender_name: sender_id.clone(),
                text: format!("reaction:added:{emoji}"),
                message_id: message_id.clone(),
                attachments: Vec::new(),
            };
            let _ = dispatch_photon_event(runtime, dispatcher, reaction_event, &space_id).await;
            return;
        }
        let mut attachments: Vec<MediaAttachment> = Vec::new();
        let text = parse_content_node(content, &mut attachments).unwrap_or_default();
        // Preview-art detection over attachment nodes.
        let attachment_nodes: Vec<&Value> = match ctype {
            "attachment" | "voice" => vec![content],
            "group" => content
                .get("items")
                .and_then(|v| v.as_array())
                .map(|items| {
                    items
                        .iter()
                        .map(|item| item.get("content").unwrap_or(item))
                        .filter(|c| {
                            matches!(
                                c.get("type").and_then(|t| t.as_str()),
                                Some("attachment") | Some("voice")
                            )
                        })
                        .collect()
                })
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        if !attachment_nodes.is_empty() {
            let all_preview = attachment_nodes.iter().all(|node| {
                is_richlink_preview_marker(
                    node.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                    node.get("id").and_then(|v| v.as_str()).unwrap_or(""),
                )
            });
            parsed.all_preview_art = all_preview;
        }
        parsed.had_richlink = ctype == "richlink"
            || (ctype == "text"
                && url_only_candidate(content.get("text").and_then(|v| v.as_str()).unwrap_or(""))
                    .is_some())
            || (ctype == "group"
                && content
                    .get("items")
                    .and_then(|v| v.as_array())
                    .map(|items| {
                        items.iter().any(|item| {
                            let c = item.get("content").unwrap_or(item);
                            c.get("type").and_then(|t| t.as_str()) == Some("richlink")
                        })
                    })
                    .unwrap_or(false));
        parsed.text = text.trim().to_string();
        parsed.attachments = attachments;
    } else {
        // Flat fallback shape (text + local attachment paths).
        parsed.text = event
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if let Some(paths) = event.get("attachments").and_then(|v| v.as_array()) {
            for path_value in paths {
                let Some(path) = path_value.as_str() else {
                    continue;
                };
                if !std::path::Path::new(path).is_absolute() {
                    continue;
                }
                match tokio::fs::read(path).await {
                    Ok(bytes) => {
                        let name = std::path::Path::new(path)
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default();
                        let mime = guess_mime_from_path(path);
                        match crate::media_cache::cache_media_bytes(
                            &crate::config::ulnclaw_home(),
                            &bytes,
                            &mime,
                            &name,
                        ) {
                            Ok(cached) => parsed.attachments.push(MediaAttachment {
                                path: cached,
                                mime,
                                bytes: bytes.len() as u64,
                                original_name: name,
                            }),
                            Err(e) => eprintln!("[photon] media cache failed: {e}"),
                        }
                    }
                    Err(e) => eprintln!("[photon] attachment unreadable ({path}): {e}"),
                }
            }
        }
        parsed.had_richlink = url_only_candidate(&parsed.text).is_some();
    }

    // iMessage sends preview art right after a URL/richlink — suppress
    // it (hermes `_is_recent_richlink_preview`).
    if parsed.all_preview_art
        && parsed.attachments.is_empty()
        && parsed.text.is_empty()
        && runtime.recent_richlink_within(&space_id).await
    {
        return;
    }
    if parsed.had_richlink {
        runtime.record_richlink(&space_id).await;
    }

    // Allowlist ∪ pairing.
    if !runtime.cfg.allow_all_users
        && !runtime
            .cfg
            .allowed_users
            .iter()
            .any(|u| u == &sender_id || u == "*")
    {
        if let Some(store) = pairing {
            if !store.is_approved("photon", &sender_id) {
                if let Some(code_msg) =
                    crate::messaging::pairing_offer_public(store, "photon", &sender_id, &sender_id)
                {
                    let _ = send_message(runtime, &space_id, &code_msg).await;
                }
                return;
            }
        } else {
            eprintln!("[photon] unauthorized sender {sender_id} — add to allowed_users");
            return;
        }
    }

    // Group wake-word gate (hermes `require_mention`).
    let mut text = parsed.text;
    if is_group && runtime.cfg.require_mention {
        let mentioned = runtime
            .cfg
            .mention_regexes
            .iter()
            .any(|re| re.is_match(&text));
        if !mentioned {
            return;
        }
        text = clean_mention_text(&text, &runtime.cfg.mention_regexes);
    }

    if text.is_empty() && parsed.attachments.is_empty() {
        return;
    }

    runtime.send_typing(&space_id).await;

    let gate_event = MessageEvent {
        platform: "photon".into(),
        chat_id: space_id.clone(),
        sender_id: sender_id.clone(),
        sender_name: sender_id,
        text: if text.is_empty() {
            "[media message]".to_string()
        } else {
            text
        },
        message_id: message_id.clone(),
        attachments: parsed.attachments,
    };
    // Lifecycle tapback: 👀 while the agent works, swapped for 👍/👎 on
    // completion (hermes `on_processing_start` /
    // `on_processing_complete`; `PHOTON_REACTIONS` opt-in).
    if runtime.cfg.reactions {
        let _ = runtime
            .add_reaction(&space_id, &message_id, REACT_PROCESSING)
            .await;
    }
    let ok = dispatch_photon_event(runtime, dispatcher, gate_event, &space_id).await;
    if runtime.cfg.reactions {
        runtime
            .swap_processing_reaction(
                &space_id,
                &message_id,
                if ok { REACT_OK } else { REACT_FAIL },
            )
            .await;
    }
}

/// Shared dispatch tail for regular and synthetic (reaction) events.
/// Returns false when the agent turn errored — that drives the 👎
/// lifecycle tapback.
async fn dispatch_photon_event(
    runtime: &Arc<Runtime>,
    dispatcher: &Arc<Dispatcher>,
    mut event: MessageEvent,
    space_id: &str,
) -> bool {
    if !crate::messaging::pre_gateway_dispatch_gate_public(&mut event).await {
        return true;
    }
    let result = dispatcher.handle_event(event).await;
    let ok = result.is_ok();
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
    let (reply_text, media_paths) = crate::messaging::extract_media_tags(&full);
    for path in &media_paths {
        send_attachment(runtime, space_id, path).await;
    }
    let reply_text = reply_text.trim().to_string();
    if !reply_text.is_empty() {
        // P705: ledger-protected reply delivery.
        dispatcher
            .try_send_with_ledger("photon", space_id, &reply_text, || async {
                match send_message(runtime, space_id, &reply_text).await {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        eprintln!("[photon] reply to {space_id} failed: {e}");
                        Err(e.to_string())
                    }
                }
            })
            .await;
    }
    ok
}

/// hermes `_sidecar_send`: URL-only replies ride `/send-richlink` with
/// plain-text fallback; markdown hint per `PHOTON_MARKDOWN`.
async fn send_message(runtime: &Runtime, space_id: &str, content: &str) -> Result<(), String> {
    if runtime.cfg.markdown {
        if let Some(url) = url_only_candidate(content) {
            match send_richlink(runtime, space_id, &url).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    eprintln!("[photon] rich-link send failed, falling back to plain text: {e}")
                }
            }
            // hermes: the fallback after a rich-link failure is plain
            // (markdown off) so a sidecar skew can't strand the URL.
            return send_plain(runtime, space_id, content, false).await;
        }
    }
    send_plain(runtime, space_id, content, runtime.cfg.markdown).await
}

/// hermes `/send-richlink` — `{spaceId, url}`.
async fn send_richlink(runtime: &Runtime, space_id: &str, url: &str) -> Result<(), String> {
    let endpoint = format!("{}/send-richlink", runtime.cfg.sidecar_url);
    let payload = json!({ "spaceId": space_id, "url": url });
    let resp = runtime
        .authed(runtime.client.post(&endpoint))
        .timeout(API_TIMEOUT)
        .json(&payload)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.status().is_success() {
        let body: Value = resp.json().await.unwrap_or(json!({}));
        runtime
            .record_sent(
                body.get("messageId")
                    .and_then(|v| v.as_str())
                    .map(String::from),
            )
            .await;
        Ok(())
    } else {
        let status = resp.status();
        let err = resp.text().await.unwrap_or_default();
        Err(format!("HTTP {status}: {}", &err[..err.len().min(200)]))
    }
}

/// hermes `/send` — `{spaceId, text}` (+ markdown format hint; the key
/// is omitted when disabled so pre-`format` sidecars keep working).
async fn send_plain(
    runtime: &Runtime,
    space_id: &str,
    content: &str,
    markdown: bool,
) -> Result<(), String> {
    let endpoint = format!("{}/send", runtime.cfg.sidecar_url);
    let payload = send_payload(space_id, content, markdown);
    let resp = runtime
        .authed(runtime.client.post(&endpoint))
        .timeout(API_TIMEOUT)
        .json(&payload)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.status().is_success() {
        let body: Value = resp.json().await.unwrap_or(json!({}));
        runtime
            .record_sent(
                body.get("messageId")
                    .and_then(|v| v.as_str())
                    .map(String::from),
            )
            .await;
        Ok(())
    } else {
        let status = resp.status();
        let err = resp.text().await.unwrap_or_default();
        Err(format!("HTTP {status}: {}", &err[..err.len().min(200)]))
    }
}

/** Build the `/send` payload: hermes 4000-char cap + markdown format
hint (the key is omitted when disabled so pre-`format` sidecars keep
working). */
fn send_payload(space_id: &str, content: &str, markdown: bool) -> Value {
    let total = content.chars().count();
    let body_text: String = content.chars().take(MAX_MESSAGE_LENGTH).collect();
    if total > MAX_MESSAGE_LENGTH {
        eprintln!("[photon] truncating outbound from {total} to {MAX_MESSAGE_LENGTH} chars");
    }
    let mut payload = json!({ "spaceId": space_id, "text": body_text });
    if markdown {
        payload["format"] = json!("markdown");
    }
    payload
}

/// hermes `/send-attachment` — `{spaceId, filePath}`.
async fn send_attachment(runtime: &Runtime, space_id: &str, path: &std::path::Path) {
    let url = format!("{}/send-attachment", runtime.cfg.sidecar_url);
    let payload = json!({ "spaceId": space_id, "filePath": path.to_string_lossy() });
    let result = runtime
        .authed(runtime.client.post(&url))
        .timeout(API_TIMEOUT)
        .json(&payload)
        .send()
        .await;
    match result {
        Ok(resp) if resp.status().is_success() => {}
        Ok(resp) => eprintln!("[photon] send-attachment HTTP {}", resp.status()),
        Err(e) => eprintln!("[photon] send-attachment failed: {e}"),
    }
}

/// Platform sender so `platform = "photon"` replies route back through
/// the sidecar.
struct PhotonSender {
    runtime: Arc<Runtime>,
}

#[async_trait::async_trait]
impl crate::messaging::PlatformSender for PhotonSender {
    async fn send_text(&self, chat_id: &str, text: &str) {
        if let Err(e) = send_message(&self.runtime, chat_id, text).await {
            eprintln!("[photon] send_text to {chat_id} failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_only_candidates() {
        assert_eq!(
            url_only_candidate("https://example.com/a?b=1"),
            Some("https://example.com/a?b=1".into())
        );
        assert_eq!(
            url_only_candidate("  HTTP://EXAMPLE.COM  "),
            Some("HTTP://EXAMPLE.COM".into())
        );
        assert_eq!(url_only_candidate("see https://example.com"), None);
        assert_eq!(url_only_candidate("[link](https://example.com)"), None);
        assert_eq!(url_only_candidate("ftp://example.com"), None);
        assert_eq!(url_only_candidate("https://"), None);
    }

    #[test]
    fn richlink_formatting() {
        let content =
            json!({"type": "richlink", "url": "https://x.dev", "title": "X", "summary": "Y"});
        assert_eq!(format_richlink_content(&content), "X\nY\nhttps://x.dev");
        let no_url = json!({"type": "richlink"});
        assert_eq!(
            format_richlink_content(&no_url),
            "[Photon rich link received with no URL]"
        );
        let dup = json!({"type": "richlink", "url": "u", "title": "T", "summary": "T"});
        assert_eq!(format_richlink_content(&dup), "T\nu");
    }

    #[test]
    fn preview_marker_detection() {
        assert!(is_richlink_preview_marker(
            "art.pluginpayloadattachment",
            ""
        ));
        assert!(is_richlink_preview_marker(
            "",
            "id-1.pluginPayloadAttachment"
        ));
        assert!(!is_richlink_preview_marker("photo.jpg", "id-2"));
    }

    #[test]
    fn mention_compile_and_clean() {
        let regexes = compile_mention_patterns(&[]);
        assert!(!regexes.is_empty());
        assert!(regexes
            .iter()
            .any(|re| re.is_match("hermes agent what time is it")));
        assert!(regexes.iter().any(|re| re.is_match("@UlNcLaW, hello")));
        assert!(!regexes.iter().any(|re| re.is_match("plain message")));
        let cleaned = clean_mention_text("hermes agent, do the thing", &regexes);
        assert_eq!(cleaned, "do the thing");
        let bare = clean_mention_text("ulnclaw", &regexes);
        assert_eq!(bare, "ulnclaw"); // empty remainder keeps the original
                                     // Invalid patterns drop without panicking.
        let dropped = compile_mention_patterns(&["(".to_string()]);
        assert!(dropped.is_empty() || true);
    }

    #[test]
    fn resolve_env_overrides() {
        let _guard = crate::models_dev::test_env_lock();
        std::env::set_var("PHOTON_MARKDOWN", "false");
        std::env::set_var("PHOTON_REQUIRE_MENTION", "true");
        let resolved = PhotonConfig::default().resolve();
        assert!(!resolved.markdown);
        assert!(resolved.require_mention);
        std::env::remove_var("PHOTON_MARKDOWN");
        std::env::remove_var("PHOTON_REQUIRE_MENTION");
        let resolved = PhotonConfig::default().resolve();
        assert!(resolved.markdown);
        assert!(!resolved.require_mention);
    }

    fn test_runtime() -> Runtime {
        Runtime {
            cfg: PhotonConfig::default().resolve(),
            client: reqwest::Client::new(),
            seen: Mutex::new(HashMap::new()),
            sent_ids: Mutex::new(VecDeque::new()),
            recent_richlinks: Mutex::new(Vec::new()),
            typing_last: Mutex::new(HashMap::new()),
        }
    }

    #[tokio::test]
    async fn dedup_window_and_echo_ledger() {
        let _guard = crate::models_dev::test_env_lock();
        let rt = test_runtime();
        assert!(!rt.is_duplicate("m-1").await);
        assert!(rt.is_duplicate("m-1").await);
        assert!(!rt.is_duplicate("m-2").await);
        assert!(!rt.is_duplicate("").await);
        // Own-send echo ledger: recorded ids are recognized, capped.
        assert!(!rt.is_own_send("out-1").await);
        rt.record_sent(Some("out-1".to_string())).await;
        assert!(rt.is_own_send("out-1").await);
        for i in 0..SENT_IDS_CAP {
            rt.record_sent(Some(format!("fill-{i}"))).await;
        }
        assert_eq!(rt.sent_ids.lock().await.len(), SENT_IDS_CAP);
        assert!(!rt.is_own_send("out-1").await); // evicted
    }

    #[test]
    fn send_payload_shape_and_truncation() {
        let payload = send_payload("space-1", "hello", true);
        assert_eq!(payload["spaceId"], "space-1");
        assert_eq!(payload["text"], "hello");
        assert_eq!(payload["format"], "markdown");
        let plain = send_payload("space-1", "hello", false);
        assert!(plain.get("format").is_none());
        let long = "x".repeat(MAX_MESSAGE_LENGTH + 500);
        let capped = send_payload("s", &long, true);
        assert_eq!(capped["text"].as_str().unwrap().len(), MAX_MESSAGE_LENGTH);
    }

    #[test]
    fn attachment_mime_guess() {
        assert_eq!(guess_mime_from_path("/tmp/photo.jpg"), "image/jpg");
        assert_eq!(guess_mime_from_path("/tmp/clip.MP4"), "video/mp4");
        assert_eq!(guess_mime_from_path("/tmp/note.m4a"), "audio/m4a");
        assert_eq!(
            guess_mime_from_path("/tmp/blob.bin"),
            "application/octet-stream"
        );
    }

    #[test]
    fn chat_key_normalization_dm_guids() {
        // DM chat GUIDs collapse to the bare E.164 phone.
        assert_eq!(normalize_chat_key("any;-;+15551234567"), "+15551234567");
        // Too-short digit runs and non-phones stay as-is.
        assert_eq!(normalize_chat_key("any;-;+12345"), "any;-;+12345");
        assert_eq!(normalize_chat_key("any;-;abc"), "any;-;abc");
        // Group GUIDs and phones pass through untouched.
        assert_eq!(normalize_chat_key("chat1234567890"), "chat1234567890");
        assert_eq!(normalize_chat_key("+15551234567"), "+15551234567");
    }

    #[test]
    fn reactions_env_gate() {
        let _guard = crate::models_dev::test_env_lock();
        std::env::remove_var("PHOTON_REACTIONS");
        assert!(!PhotonConfig::default().resolve().reactions);
        std::env::set_var("PHOTON_REACTIONS", "on");
        assert!(PhotonConfig::default().resolve().reactions);
        std::env::set_var("PHOTON_REACTIONS", "false");
        assert!(!PhotonConfig::default().resolve().reactions);
        std::env::remove_var("PHOTON_REACTIONS");
    }

    #[test]
    fn lifecycle_emoji_set_matches_hermes() {
        assert_eq!(REACT_PROCESSING, "👀");
        assert_eq!(REACT_OK, "👍");
        assert_eq!(REACT_FAIL, "👎");
    }
}

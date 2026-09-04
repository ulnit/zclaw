//! Signal platform adapter — port of hermes `gateway/platforms/signal.py`
//! (+ `signal_format.py`, `signal_rate_limit.py` essentials) @ v2026.8.3.
//!
//! Talks to a signal-cli daemon in HTTP mode: inbound messages arrive on
//! an SSE stream (`GET /api/v1/events?account=…`), outbound messages and
//! attachment fetches use JSON-RPC 2.0 (`POST /api/v1/rpc`). Requires
//! `signal-cli daemon --http 127.0.0.1:8080` plus the account number
//! (`[messaging.signal] http_url/account` or `SIGNAL_HTTP_URL` /
//! `SIGNAL_ACCOUNT`).
//!
//! Known differences: outbound markdown→textStyle body ranges are not
//! emitted (plain text only), and phone→UUID recipient resolution is
//! passthrough (signal-cli accepts numbers).

use crate::error::{AgentError, Result};
use crate::messaging::{Dispatcher, MediaAttachment, MessageEvent};
use crate::pairing::PairingStore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

// hermes signal.py constants.
const SIGNAL_MAX_ATTACHMENT_SIZE: u64 = 100 * 1024 * 1024; // 100 MB
const SSE_RETRY_DELAY_INITIAL_SECS: f64 = 2.0;
const SSE_RETRY_DELAY_MAX_SECS: f64 = 60.0;
const HEALTH_CHECK_INTERVAL_SECS: u64 = 30;
const HEALTH_CHECK_STALE_THRESHOLD_SECS: u64 = 120;
/// hermes SIGNAL_RATE_LIMIT_MAX_ATTEMPTS.
const RATE_LIMIT_MAX_ATTEMPTS: u32 = 3;
/// Echo-back filter: outbound timestamps stay dedup-eligible this long.
const RECENT_SENT_TTL_SECS: u64 = 600;
const MAX_RECENT_TIMESTAMPS: usize = 128;

fn default_true() -> bool {
    true
}

/// `[messaging.signal]` — signal-cli HTTP daemon adapter (hermes
/// `platforms.signal` + SIGNAL_* env config).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SignalConfig {
    pub enabled: bool,
    /// signal-cli daemon base URL (fallback `SIGNAL_HTTP_URL`).
    pub http_url: String,
    /// Account phone number (fallback `SIGNAL_ACCOUNT`).
    pub account: String,
    /// Chat ids allowed to talk to the bot: phone numbers for DMs,
    /// `group:<groupId>` for groups (empty = refuse all, hermes pairing
    /// semantics shared with the other adapters).
    pub allowed_chat_ids: Vec<String>,
    /// Group gate (hermes SIGNAL_GROUP_ALLOWED_USERS): empty disables
    /// groups entirely, `*` allows every group, otherwise an explicit
    /// group-id list.
    pub group_allowed_users: Vec<String>,
    /// In groups, only answer messages that @mention the bot.
    #[serde(default = "default_true")]
    pub require_mention: bool,
    #[serde(default = "default_true")]
    pub ignore_stories: bool,
    pub ignore_attachments: bool,
}

impl Default for SignalConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            http_url: String::new(),
            account: String::new(),
            allowed_chat_ids: Vec::new(),
            group_allowed_users: Vec::new(),
            require_mention: true,
            ignore_stories: true,
            ignore_attachments: false,
        }
    }
}

pub fn resolve_http_url(cfg: &SignalConfig) -> Option<String> {
    let trimmed = cfg.http_url.trim();
    if !trimmed.is_empty() {
        return Some(trimmed.trim_end_matches('/').to_string());
    }
    std::env::var("SIGNAL_HTTP_URL")
        .ok()
        .map(|v| v.trim().trim_end_matches('/').to_string())
        .filter(|v| !v.is_empty())
}

pub fn resolve_account(cfg: &SignalConfig) -> Option<String> {
    let trimmed = cfg.account.trim();
    if !trimmed.is_empty() {
        return Some(trimmed.to_string());
    }
    std::env::var("SIGNAL_ACCOUNT")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Redact a phone number for logs (hermes `redact_phone`): keep the
/// country-code prefix and last two digits, mask the middle.
pub fn redact_phone(value: &str) -> String {
    let digits: String = value.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() < 6 {
        return "*".repeat(digits.len().max(1));
    }
    format!(
        "+{}***{}",
        &digits[..digits.len() - 4],
        &digits[digits.len() - 2..]
    )
}

// ---------------------------------------------------------------------------
// JSON-RPC 2.0 (hermes `_rpc`)
// ---------------------------------------------------------------------------

/// One JSON-RPC round-trip against `{http_url}/api/v1/rpc`. The `result`
/// field is returned; a JSON-RPC `error` becomes Err.
pub async fn signal_rpc(
    client: &reqwest::Client,
    http_url: &str,
    method: &str,
    params: Value,
) -> Result<Value> {
    let payload = json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
        "id": format!("{}_{}", method, now_millis()),
    });
    let response = client
        .post(format!("{}/api/v1/rpc", http_url))
        .json(&payload)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| AgentError::Tool(format!("signal rpc {method}: {e}")))?;
    let status = response.status();
    let data: Value = response
        .json()
        .await
        .map_err(|e| AgentError::Tool(format!("signal rpc {method} parse: {e}")))?;
    if !status.is_success() {
        return Err(AgentError::Tool(format!(
            "signal rpc {method}: HTTP {status}"
        )));
    }
    if let Some(err) = data.get("error") {
        let message = err
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error");
        return Err(AgentError::Tool(format!("signal rpc {method}: {message}")));
    }
    Ok(data.get("result").cloned().unwrap_or(Value::Null))
}

fn now_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// True when a signal-cli RPC error is a rate limit (hermes
/// `_is_signal_rate_limit_error`): `[429]` / RateLimitException markers.
pub fn is_rate_limit_error(err: &AgentError) -> Option<u64> {
    let message = err.to_string();
    if !(message.contains("[429]")
        || message.contains("RateLimitException")
        || message.contains("429"))
    {
        return None;
    }
    // "retry after N seconds" / "Retry-After: N" best-effort parse.
    let lower = message.to_lowercase();
    if let Some(idx) = lower.find("retry") {
        let tail = &message[idx..];
        let number: String = tail
            .chars()
            .skip_while(|c| !c.is_ascii_digit())
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if let Ok(secs) = number.parse::<u64>() {
            return Some(secs.max(1));
        }
    }
    Some(5)
}

/// Validate signal-cli send `results` (hermes `_validate_send_result`).
pub fn validate_send_result(result: &Value) -> std::result::Result<(), String> {
    let Some(results) = result.get("results").and_then(|v| v.as_array()) else {
        return Ok(());
    };
    for item in results {
        let Some(item) = item.as_object() else {
            continue;
        };
        if let Some(rtype) = item.get("type").and_then(|v| v.as_str()) {
            if rtype != "SUCCESS" {
                return Err(rtype.to_string());
            }
        }
        if let Some(false) = item.get("success").and_then(|v| v.as_bool()) {
            return Err(item
                .get("failure")
                .map(|v| v.to_string())
                .unwrap_or_else(|| "Recipient delivery failed".into()));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Outbound send (hermes `send`)
// ---------------------------------------------------------------------------

/// Build the JSON-RPC `send` params for a chat (hermes `send` minus the
/// textStyle ranges). `group:<id>` chats address the group; everything
/// else is a DM recipient list.
pub fn build_send_params(account: &str, chat_id: &str, text: &str) -> Value {
    let mut params = json!({
        "account": account,
        "message": text,
    });
    if let Some(group_id) = chat_id.strip_prefix("group:") {
        params["groupId"] = json!(group_id);
    } else {
        params["recipient"] = json!([chat_id]);
    }
    params
}

/// Send a text message with rate-limit retry (hermes retry loop capped at
/// SIGNAL_RATE_LIMIT_MAX_ATTEMPTS).
pub async fn signal_send_text(
    client: &reqwest::Client,
    cfg: &SignalConfig,
    http_url: &str,
    chat_id: &str,
    text: &str,
) -> Result<()> {
    if text.trim().is_empty() {
        return Ok(());
    }
    let params = build_send_params(&resolve_account(cfg).unwrap_or_default(), chat_id, text);
    let mut attempts = 0u32;
    loop {
        attempts += 1;
        match signal_rpc(client, http_url, "send", params.clone()).await {
            Ok(result) => {
                validate_send_result(&result)
                    .map_err(|e| AgentError::Tool(format!("signal send: {e}")))?;
                return Ok(());
            }
            Err(e) => {
                if attempts < RATE_LIMIT_MAX_ATTEMPTS {
                    if let Some(retry_after) = is_rate_limit_error(&e) {
                        eprintln!(
                            "[signal] rate limited — retrying in {}s (attempt {}/{})",
                            retry_after,
                            attempts + 1,
                            RATE_LIMIT_MAX_ATTEMPTS
                        );
                        tokio::time::sleep(Duration::from_secs(retry_after)).await;
                        continue;
                    }
                }
                return Err(e);
            }
        }
    }
}

/// Send text plus local files as base64 attachments (hermes
/// `send_image_file` / media send path).
pub async fn signal_send_with_attachments(
    client: &reqwest::Client,
    cfg: &SignalConfig,
    http_url: &str,
    chat_id: &str,
    text: &str,
    paths: &[std::path::PathBuf],
) -> Result<()> {
    let mut attachments = Vec::new();
    for path in paths {
        let bytes = tokio::fs::read(path)
            .await
            .map_err(|e| AgentError::Tool(format!("signal attachment read: {e}")))?;
        attachments.push(base64_encode(&bytes));
    }
    let account = resolve_account(cfg).unwrap_or_default();
    let mut params = build_send_params(&account, chat_id, text);
    params["base64Attachments"] = json!(attachments);
    let result = signal_rpc(client, http_url, "send", params).await?;
    validate_send_result(&result).map_err(|e| AgentError::Tool(format!("signal send: {e}")))
}

fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((input.len() + 2) / 3 * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((triple >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((triple >> 12) & 0x3F) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((triple >> 6) & 0x3F) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(triple & 0x3F) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn base64_decode(input: &str) -> std::result::Result<Vec<u8>, String> {
    let mut output = Vec::with_capacity(input.len() / 4 * 3);
    let mut buffer: u32 = 0;
    let mut bits = 0;
    for ch in input.chars() {
        if ch == '=' || ch.is_whitespace() {
            continue;
        }
        let value = match ch {
            'A'..='Z' => ch as u32 - 'A' as u32,
            'a'..='z' => ch as u32 - 'a' as u32 + 26,
            '0'..='9' => ch as u32 - '0' as u32 + 52,
            '+' => 62,
            '/' => 63,
            other => return Err(format!("invalid base64 char: {}", other)),
        };
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push(((buffer >> bits) & 0xFF) as u8);
        }
    }
    Ok(output)
}

// ---------------------------------------------------------------------------
// Inbound envelopes (hermes `_handle_envelope`)
// ---------------------------------------------------------------------------

/// Replace Signal mention placeholders (U+FFFC) with readable
/// @identifiers (hermes `_render_mentions`): mentions carry start/length
/// plus number/uuid metadata; replace from the end so indices stay valid.
pub fn render_mentions(text: &str, mentions: &[Value]) -> String {
    if mentions.is_empty() || !text.contains('\u{FFFC}') {
        return text.to_string();
    }
    let mut indexed: Vec<(usize, usize, String)> = Vec::new();
    for mention in mentions {
        let start = mention.get("start").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let length = mention.get("length").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
        let identifier = mention
            .get("number")
            .and_then(|v| v.as_str())
            .or_else(|| mention.get("uuid").and_then(|v| v.as_str()))
            .unwrap_or("user");
        indexed.push((start, length, format!("@{}", identifier)));
    }
    indexed.sort_by(|a, b| b.0.cmp(&a.0));
    let mut chars: Vec<char> = text.chars().collect();
    for (start, length, replacement) in indexed {
        if start < chars.len() {
            let end = (start + length).min(chars.len());
            chars.splice(start..end, replacement.chars());
        }
    }
    chars.into_iter().collect()
}

/// Parsed inbound envelope ready for dispatch decisions.
#[derive(Debug, Clone)]
pub struct SignalInbound {
    pub chat_id: String,
    pub sender_id: String,
    pub sender_name: String,
    pub text: String,
    pub is_group: bool,
    /// Attachment ids to fetch via `getAttachment` before dispatch.
    pub attachment_ids: Vec<(String, u64, String)>, // (id, size, contentType)
}

/// Echo-suppression state: outbound message timestamps whose syncMessage
/// echo must be dropped (hermes `_recent_sent_timestamps`).
#[derive(Debug, Default)]
pub struct SentTimestamps {
    entries: HashMap<u64, Instant>,
}

impl SentTimestamps {
    pub fn track(&mut self, ts: u64) {
        self.entries.insert(ts, Instant::now());
        self.prune();
    }

    /// Pop a timestamp if it matches one we sent (echo of our own reply).
    pub fn consume(&mut self, ts: u64) -> bool {
        let found = self.entries.remove(&ts).is_some();
        self.prune();
        found
    }

    fn prune(&mut self) {
        let cutoff = Duration::from_secs(RECENT_SENT_TTL_SECS);
        self.entries.retain(|_, at| at.elapsed() < cutoff);
        while self.entries.len() > MAX_RECENT_TIMESTAMPS {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, at)| *at)
                .map(|(ts, _)| *ts);
            match oldest {
                Some(ts) => {
                    self.entries.remove(&ts);
                }
                None => break,
            }
        }
    }
}

/// Parse one signal-cli SSE envelope into dispatchable content (hermes
/// `_handle_envelope` up to the dispatch boundary). Returns None for
/// filtered envelopes (sync noise, stories, self-echo, empty content,
/// disallowed groups).
pub fn parse_envelope(
    cfg: &SignalConfig,
    account: &str,
    sent: &mut SentTimestamps,
    envelope: &Value,
) -> Option<SignalInbound> {
    let account = account.trim();
    let envelope_data = envelope
        .get("envelope")
        .cloned()
        .unwrap_or_else(|| envelope.clone());

    // syncMessage: keep Note-to-Self, drop every other sync event, and
    // consume echoes of our own outbound sends.
    let mut data_holder = envelope_data.clone();
    let mut promoted_note_to_self = false;
    if let Some(sync_msg) = envelope_data.get("syncMessage") {
        let sent_msg = sync_msg.get("sentMessage");
        let mut is_note_to_self = false;
        if let Some(sent_msg) = sent_msg {
            let dest = sent_msg
                .get("destinationNumber")
                .or_else(|| sent_msg.get("destination"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let group_id = sent_msg
                .pointer("/groupInfo/groupId")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if dest == account || !group_id.is_empty() {
                if let Some(ts) = sent_msg.get("timestamp").and_then(|v| v.as_u64()) {
                    if sent.consume(ts) {
                        return None;
                    }
                }
                is_note_to_self = true;
                promoted_note_to_self = true;
                if let Some(obj) = data_holder.as_object_mut() {
                    obj.insert("dataMessage".to_string(), sent_msg.clone());
                }
            }
        }
        if !is_note_to_self {
            return None;
        }
    }

    // Sender.
    let sender = envelope_data
        .get("sourceNumber")
        .or_else(|| envelope_data.get("sourceUuid"))
        .or_else(|| envelope_data.get("source"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if sender.is_empty() {
        return None;
    }
    let sender_name = envelope_data
        .get("sourceName")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    // Self-message filtering prevents reply loops (Note to Self exempt).
    if !account.is_empty() && sender == account && !promoted_note_to_self {
        return None;
    }
    if cfg.ignore_stories && envelope_data.get("storyMessage").is_some() {
        return None;
    }

    // dataMessage — also inside editMessage for edits (the promoted
    // Note-to-Self content lives in data_holder).
    let data_message = data_holder
        .get("dataMessage")
        .or_else(|| data_holder.pointer("/editMessage/dataMessage"));
    let Some(data_message) = data_message else {
        return None;
    };

    // Groups: gated by group_allowed_users (hermes SIGNAL_GROUP_ALLOWED_USERS).
    let group_id = data_message
        .pointer("/groupInfo/groupId")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let is_group = !group_id.is_empty();
    if is_group {
        if cfg.group_allowed_users.is_empty() {
            return None;
        }
        if !cfg.group_allowed_users.iter().any(|g| g == "*")
            && !cfg.group_allowed_users.iter().any(|g| g == group_id)
        {
            return None;
        }
    }

    let chat_id = if is_group {
        format!("group:{}", group_id)
    } else {
        sender.clone()
    };

    // Text + mentions.
    let mut text = data_message
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let mentions = data_message
        .get("mentions")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if !text.is_empty() && !mentions.is_empty() {
        text = render_mentions(&text, &mentions);
    }
    // Group mention gate + self-mention stripping (hermes ordering).
    if is_group {
        if cfg.require_mention {
            let mentioned = (!account.is_empty() && text.contains(&format!("@{}", account)))
                || mentions.iter().any(|m| {
                    m.get("number").and_then(|v| v.as_str()) == Some(account)
                        || m.get("uuid").and_then(|v| v.as_str()) == Some(account)
                });
            if !mentioned {
                return None;
            }
        }
        if !account.is_empty() {
            text = text.replace(&format!("@{}", account), "");
            text = text.replace("  ", " ");
            text = text.trim().to_string();
        }
    }

    // Attachments (ids; fetched after the auth gate).
    let mut attachment_ids = Vec::new();
    if !cfg.ignore_attachments {
        if let Some(items) = data_message.get("attachments").and_then(|v| v.as_array()) {
            for att in items {
                let Some(id) = att.get("id").and_then(|v| v.as_str()) else {
                    continue;
                };
                let size = att.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
                if size > SIGNAL_MAX_ATTACHMENT_SIZE {
                    eprintln!("[signal] attachment too large ({} bytes), skipping", size);
                    continue;
                }
                let mime = att
                    .get("contentType")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                attachment_ids.push((id.to_string(), size, mime));
            }
        }
    }

    // Skip metadata-only envelopes (profile key updates, empty messages).
    if text.trim().is_empty() && attachment_ids.is_empty() {
        return None;
    }

    Some(SignalInbound {
        chat_id,
        sender_id: sender,
        sender_name,
        text,
        is_group,
        attachment_ids,
    })
}

// ---------------------------------------------------------------------------
// Attachments (hermes `_fetch_attachment`)
// ---------------------------------------------------------------------------

/// Sniff bytes → mime (hermes `_guess_extension` essentials).
pub fn sniff_mime(data: &[u8]) -> &'static str {
    if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return "image/jpeg";
    }
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        return "image/png";
    }
    if data.starts_with(b"GIF8") {
        return "image/gif";
    }
    if data.len() > 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        return "image/webp";
    }
    if data.len() > 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"WAVE" {
        return "audio/wav";
    }
    if data.len() > 11 && &data[4..8] == b"ftyp" {
        return "video/mp4";
    }
    if data.starts_with(b"OggS") {
        return "audio/ogg";
    }
    if data.starts_with(b"ID3") || (data.len() > 1 && data[0] == 0xFF && (data[1] & 0xE6) == 0xE2) {
        return "audio/mpeg";
    }
    if data.starts_with(b"%PDF") {
        return "application/pdf";
    }
    if data.starts_with(&[0x50, 0x4B, 0x03, 0x04]) {
        return "application/zip";
    }
    "application/octet-stream"
}

/// Android Signal voice notes are raw ADTS AAC; most STT providers want
/// an MP4 container. Lossless `ffmpeg -c:a copy` remux (hermes
/// `_remux_aac_to_m4a`), best-effort — absent ffmpeg the bytes stay raw.
pub fn remux_aac_to_m4a(data: &[u8]) -> Option<Vec<u8>> {
    let dir = std::env::temp_dir();
    let stamp = now_millis();
    let src = dir.join(format!("ulnclaw-signal-{}.aac", stamp));
    let dst = dir.join(format!("ulnclaw-signal-{}.m4a", stamp));
    std::fs::write(&src, data).ok()?;
    let status = std::process::Command::new("ffmpeg")
        .args(["-y", "-i", src.to_str()?, "-c:a", "copy", dst.to_str()?])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .ok()?;
    let remuxed = if status.success() {
        std::fs::read(&dst).ok()
    } else {
        None
    };
    std::fs::remove_file(&src).ok();
    std::fs::remove_file(&dst).ok();
    remuxed
}

/// Fetch one attachment by id (JSON-RPC `getAttachment` → base64 `data`)
/// and cache it under `<home>/media-cache/` (hermes `_fetch_attachment`).
pub async fn fetch_attachment(
    client: &reqwest::Client,
    http_url: &str,
    account: &str,
    home: &std::path::Path,
    attachment_id: &str,
    declared_mime: &str,
) -> Option<MediaAttachment> {
    let result = signal_rpc(
        client,
        http_url,
        "getAttachment",
        json!({"account": account, "id": attachment_id}),
    )
    .await
    .ok()?;
    let b64 = match &result {
        Value::Object(obj) => obj.get("data").and_then(|v| v.as_str())?,
        Value::String(s) => s.as_str(),
        _ => return None,
    };
    let mut raw = base64_decode(b64).ok()?;
    let mut mime = if declared_mime.trim().is_empty() {
        sniff_mime(&raw).to_string()
    } else {
        declared_mime.to_string()
    };
    if mime == "audio/aac"
        || (mime.starts_with("audio/") && raw.starts_with(&[0xFF]))
            && sniff_mime(&raw) == "audio/mpeg"
    {
        // ADTS stream — remux into m4a for STT friendliness.
        if let Some(remuxed) = remux_aac_to_m4a(&raw) {
            raw = remuxed;
            mime = "audio/mp4".to_string();
        }
    }
    let path = crate::media_cache::cache_media_bytes(home, &raw, &mime, "").ok()?;
    Some(MediaAttachment {
        path,
        mime,
        bytes: raw.len() as u64,
        original_name: String::new(),
    })
}

// ---------------------------------------------------------------------------
// SSE stream (hermes `_sse_listener`)
// ---------------------------------------------------------------------------

/// Split an SSE text chunk into complete `data:` payloads (hermes SSE
/// parsing): keepalive comment lines (`:`) are reported as activity,
/// partial trailing lines stay in the buffer.
pub fn parse_sse_chunk(buffer: &mut String, chunk: &str) -> Vec<String> {
    buffer.push_str(chunk);
    let mut payloads = Vec::new();
    while let Some(pos) = buffer.find('\n') {
        let line: String = buffer.drain(..=pos).collect();
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with(':') {
            // keepalive comment — activity only
            continue;
        }
        if let Some(data) = line.strip_prefix("data:") {
            let data = data.trim();
            if !data.is_empty() {
                payloads.push(data.to_string());
            }
        }
    }
    payloads
}

struct Sender {
    client: reqwest::Client,
    cfg: SignalConfig,
    http_url: String,
}

#[async_trait::async_trait]
impl crate::messaging::PlatformSender for Sender {
    async fn send_text(&self, chat_id: &str, text: &str) {
        if let Err(e) =
            signal_send_text(&self.client, &self.cfg, &self.http_url, chat_id, text).await
        {
            eprintln!("[signal] send failed: {e}");
        }
    }
}

/// Run the Signal adapter: health check → sender registration → SSE
/// listen/dispatch loop with exponential-backoff reconnect + stale-stream
/// health monitor (hermes `connect`/`_sse_listener`/`_health_monitor`).
pub async fn run(
    cfg: SignalConfig,
    dispatcher: Arc<Dispatcher>,
    pairing: Option<Arc<PairingStore>>,
) {
    let Some(http_url) = resolve_http_url(&cfg) else {
        eprintln!("[signal] disabled: no http_url configured (set messaging.signal.http_url or SIGNAL_HTTP_URL)");
        return;
    };
    let Some(account) = resolve_account(&cfg) else {
        eprintln!("[signal] disabled: no account configured (set messaging.signal.account or SIGNAL_ACCOUNT)");
        return;
    };
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap_or_default();

    // Health check — verify the signal-cli daemon is reachable (hermes
    // connect gate).
    match client
        .get(format!("{}/api/v1/check", http_url))
        .timeout(Duration::from_secs(10))
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => {
            eprintln!(
                "[signal] connected to {} as {}",
                http_url,
                redact_phone(&account)
            );
        }
        Ok(response) => {
            eprintln!(
                "[signal] health check failed (status {}) — {}",
                response.status(),
                http_url
            );
            return;
        }
        Err(e) => {
            eprintln!("[signal] cannot reach signal-cli at {}: {e}", http_url);
            return;
        }
    }

    crate::messaging::register_platform_sender(
        "signal",
        Arc::new(Sender {
            client: client.clone(),
            cfg: cfg.clone(),
            http_url: http_url.clone(),
        }),
    );

    let events_url = format!(
        "{}/api/v1/events?account={}",
        http_url,
        urlencoding(&account)
    );
    let mut backoff = SSE_RETRY_DELAY_INITIAL_SECS;
    let mut sent_timestamps = SentTimestamps::default();
    loop {
        let stream_result = stream_events(
            &client,
            &events_url,
            &cfg,
            &account,
            &http_url,
            &dispatcher,
            pairing.as_ref(),
            &mut sent_timestamps,
        )
        .await;
        if let Err(e) = stream_result {
            eprintln!("[signal] SSE error: {e}");
        }
        // Exponential backoff with 20% jitter (hermes reconnect policy).
        let jitter = backoff * 0.2 * fastrand();
        tokio::time::sleep(Duration::from_secs_f64(backoff + jitter)).await;
        backoff = (backoff * 2.0).min(SSE_RETRY_DELAY_MAX_SECS);
    }
}

fn fastrand() -> f64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    (nanos % 1000) as f64 / 1000.0
}

fn urlencoding(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{:02X}", byte)),
        }
    }
    out
}

/// One SSE connection: stream envelopes until the connection drops or
/// goes stale (health monitor forces a reconnect by returning).
#[allow(clippy::too_many_arguments)]
async fn stream_events(
    client: &reqwest::Client,
    events_url: &str,
    cfg: &SignalConfig,
    account: &str,
    http_url: &str,
    dispatcher: &Arc<Dispatcher>,
    pairing: Option<&Arc<PairingStore>>,
    sent_timestamps: &mut SentTimestamps,
) -> Result<()> {
    use futures::StreamExt;
    let stream_client = reqwest::Client::new(); // no global timeout on the stream
    let response = stream_client
        .get(events_url)
        .header("Accept", "text/event-stream")
        .send()
        .await
        .map_err(|e| AgentError::Tool(format!("signal SSE connect: {e}")))?;
    if !response.status().is_success() {
        return Err(AgentError::Tool(format!(
            "signal SSE status {}",
            response.status()
        )));
    }
    eprintln!("[signal] SSE connected");
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut last_activity = Instant::now();
    loop {
        let next = tokio::time::timeout(
            Duration::from_secs(HEALTH_CHECK_INTERVAL_SECS),
            stream.next(),
        )
        .await;
        match next {
            Ok(Some(Ok(chunk))) => {
                last_activity = Instant::now();
                let payloads = parse_sse_chunk(&mut buffer, &String::from_utf8_lossy(&chunk));
                for payload in payloads {
                    let Ok(envelope) = serde_json::from_str::<Value>(&payload) else {
                        continue;
                    };
                    if let Some(inbound) = parse_envelope(cfg, account, sent_timestamps, &envelope)
                    {
                        handle_inbound(
                            client, cfg, account, http_url, dispatcher, pairing, inbound,
                        )
                        .await;
                    }
                }
            }
            Ok(Some(Err(e))) => {
                return Err(AgentError::Tool(format!("signal SSE stream: {e}")));
            }
            Ok(None) => {
                return Err(AgentError::Tool("signal SSE stream ended".into()));
            }
            Err(_) => {
                // No data for a whole interval — check daemon health; a
                // healthy daemon just means quiet traffic, a dead one
                // forces a reconnect (hermes _health_monitor).
                if last_activity.elapsed().as_secs() > HEALTH_CHECK_STALE_THRESHOLD_SECS {
                    let healthy = client
                        .get(format!("{}/api/v1/check", http_url))
                        .timeout(Duration::from_secs(10))
                        .send()
                        .await
                        .map(|r| r.status().is_success())
                        .unwrap_or(false);
                    if healthy {
                        last_activity = Instant::now();
                    } else {
                        return Err(AgentError::Tool(
                            "signal daemon unhealthy while SSE idle — reconnecting".into(),
                        ));
                    }
                }
            }
        }
    }
}

/// Auth gate (allowlist∪pairing, hermes semantics shared with the other
/// adapters) → attachment fetch → dispatch → reply.
async fn handle_inbound(
    client: &reqwest::Client,
    cfg: &SignalConfig,
    account: &str,
    http_url: &str,
    dispatcher: &Arc<Dispatcher>,
    pairing: Option<&Arc<PairingStore>>,
    inbound: SignalInbound,
) {
    let authorized = cfg
        .allowed_chat_ids
        .iter()
        .any(|allowed| *allowed == inbound.chat_id || *allowed == inbound.sender_id)
        || pairing
            .map(|store| store.is_approved("signal", &inbound.sender_id))
            .unwrap_or(false);
    if !authorized {
        eprintln!(
            "[signal] refusing message from {} — add it to messaging.signal.allowed_chat_ids \
             (or `group:<id>`) or approve a pairing code",
            redact_phone(&inbound.sender_id)
        );
        if let Some(store) = pairing {
            if let Some(reply) = crate::messaging::pairing_offer_public(
                store,
                "signal",
                &inbound.sender_id,
                &inbound.sender_name,
            ) {
                if let Err(e) =
                    signal_send_text(client, cfg, http_url, &inbound.chat_id, &reply).await
                {
                    eprintln!("[signal] pairing reply failed: {e}");
                }
            }
        }
        return;
    }

    let mut event = MessageEvent {
        platform: "signal".into(),
        chat_id: inbound.chat_id.clone(),
        sender_id: inbound.sender_id.clone(),
        sender_name: inbound.sender_name.clone(),
        text: inbound.text.clone(),
        message_id: String::new(),
        attachments: Vec::new(),
    };
    for (id, _size, mime) in &inbound.attachment_ids {
        if let Some(attachment) = fetch_attachment(
            client,
            http_url,
            account,
            &crate::config::ulnclaw_home(),
            id,
            mime,
        )
        .await
        {
            event.attachments.push(attachment);
        } else {
            eprintln!("[signal] failed to fetch attachment {}", id);
        }
    }
    if !crate::messaging::pre_gateway_dispatch_gate_public(&mut event).await {
        return;
    }
    let outcome = match dispatcher.handle_event(event).await {
        Ok(outcome) => outcome,
        Err(e) => crate::messaging::DispatchOutcome {
            reply: format!("error: {e}"),
            transcript_echoes: Vec::new(),
        },
    };
    for echo in &outcome.transcript_echoes {
        if let Err(e) = signal_send_text(client, cfg, http_url, &inbound.chat_id, echo).await {
            eprintln!("[signal] transcript echo failed: {e}");
        }
    }
    let (reply_text, media_paths) = crate::messaging::extract_media_tags(&outcome.reply);
    if !reply_text.trim().is_empty() {
        // P704: ledger-protected reply delivery.
        dispatcher
            .try_send_with_ledger("signal", &inbound.chat_id, &reply_text, || async {
                match signal_send_text(client, cfg, http_url, &inbound.chat_id, &reply_text).await {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        eprintln!("[signal] reply failed: {e}");
                        Err(e.to_string())
                    }
                }
            })
            .await;
    }
    if !media_paths.is_empty() {
        if let Err(e) =
            signal_send_with_attachments(client, cfg, http_url, &inbound.chat_id, "", &media_paths)
                .await
        {
            eprintln!("[signal] media reply failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> SignalConfig {
        SignalConfig {
            enabled: true,
            http_url: "http://127.0.0.1:8080".into(),
            account: "+15550001111".into(),
            allowed_chat_ids: vec!["+15552223333".into()],
            ..Default::default()
        }
    }

    #[test]
    fn render_mentions_replaces_placeholders() {
        let text = "hello \u{FFFC} and \u{FFFC}";
        let mentions = vec![
            json!({"start": 6, "length": 1, "number": "+15551112222"}),
            json!({"start": 12, "length": 1, "uuid": "abcd-1234"}),
        ];
        assert_eq!(
            render_mentions(text, &mentions),
            "hello @+15551112222 and @abcd-1234"
        );
        // No placeholders → unchanged.
        assert_eq!(render_mentions("plain", &mentions), "plain");
    }

    fn dm_envelope(text: &str) -> Value {
        json!({
            "envelope": {
                "sourceNumber": "+15552223333",
                "sourceName": "Alice",
                "timestamp": 1700000000000u64,
                "dataMessage": {"message": text},
            }
        })
    }

    #[test]
    fn parses_dm_text_message() {
        let mut sent = SentTimestamps::default();
        let inbound =
            parse_envelope(&cfg(), "+15550001111", &mut sent, &dm_envelope("hi")).unwrap();
        assert_eq!(inbound.chat_id, "+15552223333");
        assert_eq!(inbound.sender_name, "Alice");
        assert_eq!(inbound.text, "hi");
        assert!(!inbound.is_group);
    }

    #[test]
    fn skips_empty_and_story_envelopes() {
        let mut sent = SentTimestamps::default();
        assert!(parse_envelope(&cfg(), "+15550001111", &mut sent, &dm_envelope("")).is_none());
        let story = json!({
            "envelope": {
                "sourceNumber": "+15552223333",
                "storyMessage": {"text": "story"},
            }
        });
        assert!(parse_envelope(&cfg(), "+15550001111", &mut sent, &story).is_none());
    }

    #[test]
    fn self_messages_filtered_but_note_to_self_passes() {
        let mut sent = SentTimestamps::default();
        // Plain self-DM → filtered.
        let self_dm = json!({
            "envelope": {
                "sourceNumber": "+15550001111",
                "dataMessage": {"message": "loop risk"},
            }
        });
        assert!(parse_envelope(&cfg(), "+15550001111", &mut sent, &self_dm).is_none());
        // Note to Self via syncMessage.sentMessage → promoted.
        let note = json!({
            "envelope": {
                "sourceNumber": "+15550001111",
                "syncMessage": {"sentMessage": {
                    "destinationNumber": "+15550001111",
                    "timestamp": 42u64,
                    "message": "remember milk",
                }},
            }
        });
        let inbound = parse_envelope(&cfg(), "+15550001111", &mut sent, &note).unwrap();
        assert_eq!(inbound.text, "remember milk");
    }

    #[test]
    fn own_outbound_echo_consumed() {
        let mut sent = SentTimestamps::default();
        sent.track(99);
        let echo = json!({
            "envelope": {
                "sourceNumber": "+15550001111",
                "syncMessage": {"sentMessage": {
                    "destinationNumber": "+15552223333",
                    "timestamp": 99u64,
                    "message": "my own reply",
                }},
            }
        });
        // Echo of our own DM reply to someone else: dropped by the sync
        // filter (hermes semantics — consume only runs for self/group
        // destinations), so the timestamp stays tracked.
        assert!(parse_envelope(&cfg(), "+15550001111", &mut sent, &echo).is_none());
        assert!(sent.consume(99), "DM-echo timestamps are not consumed");
        // Group send echo: destination carries groupInfo → consumed once.
        sent.track(100);
        let group_echo = json!({
            "envelope": {
                "sourceNumber": "+15550001111",
                "syncMessage": {"sentMessage": {
                    "groupInfo": {"groupId": "Z3JvdXA="},
                    "timestamp": 100u64,
                    "message": "my group reply",
                }},
            }
        });
        assert!(parse_envelope(&cfg(), "+15550001111", &mut sent, &group_echo).is_none());
        assert!(!sent.consume(100), "group echo timestamp consumed by parse");
        // Note-to-Self addressed to our own number is promoted.
        let note = json!({
            "envelope": {
                "sourceNumber": "+15550001111",
                "syncMessage": {"sentMessage": {
                    "destinationNumber": "+15550001111",
                    "timestamp": 101u64,
                    "message": "note",
                }},
            }
        });
        assert!(parse_envelope(&cfg(), "+15550001111", &mut sent, &note).is_some());
    }

    #[test]
    fn group_gating() {
        let group = json!({
            "envelope": {
                "sourceNumber": "+15552223333",
                "dataMessage": {
                    "message": "hey bot",
                    "groupInfo": {"groupId": "Z3JvdXA="},
                },
            }
        });
        let mut sent = SentTimestamps::default();
        // Default: groups disabled.
        assert!(parse_envelope(&cfg(), "+15550001111", &mut sent, &group).is_none());
        // Allowed group id + mention present (require_mention default).
        let mut c = cfg();
        c.group_allowed_users = vec!["Z3JvdXA=".into()];
        let inbound = parse_envelope(&c, "+15550001111", &mut sent, &group);
        assert!(
            inbound.is_none(),
            "require_mention filters unmentioned group msg"
        );
        let mention = json!({
            "envelope": {
                "sourceNumber": "+15552223333",
                "dataMessage": {
                    "message": "\u{FFFC} hello",
                    "mentions": [{"start": 0, "length": 1, "number": "+15550001111"}],
                    "groupInfo": {"groupId": "Z3JvdXA="},
                },
            }
        });
        let inbound = parse_envelope(&c, "+15550001111", &mut sent, &mention).unwrap();
        assert_eq!(inbound.chat_id, "group:Z3JvdXA=");
        assert_eq!(inbound.text, "hello"); // self-mention stripped
                                           // Wildcard allows any group.
        let mut c2 = cfg();
        c2.group_allowed_users = vec!["*".into()];
        c2.require_mention = false;
        assert!(parse_envelope(&c2, "+15550001111", &mut sent, &group).is_some());
    }

    #[test]
    fn sse_chunk_parsing() {
        let mut buffer = String::new();
        let mut out = parse_sse_chunk(&mut buffer, ": keepalive\ndata: {\"a\":1}\n");
        assert_eq!(out, vec!["{\"a\":1}".to_string()]);
        // Partial line stays buffered until completed.
        out = parse_sse_chunk(&mut buffer, "data: {\"b\"");
        assert!(out.is_empty());
        out = parse_sse_chunk(&mut buffer, ":2}\n\n");
        assert_eq!(out, vec!["{\"b\":2}".to_string()]);
    }

    #[test]
    fn send_params_dm_and_group() {
        let dm = build_send_params("+15550001111", "+15552223333", "hi");
        assert_eq!(dm["recipient"], json!(["+15552223333"]));
        assert_eq!(dm["message"], "hi");
        let group = build_send_params("+15550001111", "group:Z3JvdXA=", "yo");
        assert_eq!(group["groupId"], "Z3JvdXA=");
        assert!(group.get("recipient").is_none());
    }

    #[test]
    fn send_result_validation() {
        assert!(validate_send_result(&json!({})).is_ok());
        assert!(validate_send_result(&json!({"results": [{"type": "SUCCESS"}]})).is_ok());
        let failure = json!({"results": [{"type": "RATE_LIMIT_FAILURE"}]});
        assert_eq!(
            validate_send_result(&failure).unwrap_err(),
            "RATE_LIMIT_FAILURE"
        );
        let flag = json!({"results": [{"success": false, "failure": "UNREGISTERED"}]});
        assert!(validate_send_result(&flag).is_err());
    }

    #[test]
    fn rate_limit_detection_parses_retry_after() {
        let err = AgentError::Tool(
            "signal rpc send: [429] RateLimitException: retry after 7 seconds".into(),
        );
        assert_eq!(is_rate_limit_error(&err), Some(7));
        let plain = AgentError::Tool("signal rpc send: connection refused".into());
        assert_eq!(is_rate_limit_error(&plain), None);
    }

    #[test]
    fn phone_redaction() {
        assert_eq!(redact_phone("+15551234567"), "+1555123***67");
        assert_eq!(redact_phone("123"), "***");
    }

    #[test]
    fn mime_sniffing() {
        assert_eq!(sniff_mime(&[0xFF, 0xD8, 0xFF, 0xE0]), "image/jpeg");
        assert_eq!(sniff_mime(b"\x89PNG\r\n\x1a\nrest"), "image/png");
        assert_eq!(sniff_mime(b"OggSxxxx"), "audio/ogg");
        assert_eq!(sniff_mime(b"%PDF-1.7"), "application/pdf");
        assert_eq!(sniff_mime(b"????"), "application/octet-stream");
    }

    #[test]
    fn base64_roundtrip() {
        let data: Vec<u8> = (0..=255).collect();
        assert_eq!(base64_decode(&base64_encode(&data)).unwrap(), data);
    }
}

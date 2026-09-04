//! WhatsApp (Baileys bridge) platform adapter — port of hermes
//! `plugins/platforms/whatsapp` @ v2026.8.3 (adapter.py).
//!
//! The hermes adapter supervises a Node.js Baileys bridge subprocess
//! and talks to it over localhost HTTP: `GET /health` (connection /
//! pairing status), `GET /messages` (inbound queue, polled every
//! second), `POST /read` (read receipts), `POST /send`
//! (`{chatId, message, replyTo?}`), `POST /send-media`
//! (`{to, path, mediaType, caption}`), `POST /send-poll`
//! (`{chatId, question, options, selectableCount}` — native polls,
//! inbound votes surface as the selected option's text), `POST
//! /send-location` (`{chatId, latitude, longitude, name?, address?}`),
//! and `POST /edit` (`{chatId, messageId, message}` — edit a sent
//! message). This port keeps the exact wire protocol; the bundled
//! bridge implements all of it (see the lifecycle note below), and any
//! external Baileys bridge speaking the hermes endpoints also works —
//! point `[messaging.whatsapp] bridge_url` at it.
//!
//! Intake mirrors hermes: messages from self and status broadcasts are
//! dropped, DMs are allowlist∪pairing gated, groups honor
//! `allowed_channels` + require-mention (bot JID from `/health`,
//! fallback to any `@mention`) with `free_response_channels` opt-outs.
//! Inbound media (bridge-cached local paths or URLs) download into the
//! content-addressed media cache; outbound `MEDIA:` tags ride
//! `/send-media` with an extension-derived `mediaType`.
//!
//! Bridge lifecycle mirrors hermes: the Baileys bridge
//! (`scripts/whatsapp-bridge/bridge.js`) is bundled into the binary and
//! supervised by `whatsapp_bridge` — npm dependency install with a
//! package-hash stamp, pidfile/port stale cleanup, spawn with
//! `bridge.log` capture, two-phase readiness wait, and the `scriptHash`
//! staleness handshake for reuse. Auto-spawn applies only when
//! `bridge_url` targets `127.0.0.1:<bridge_port>`; any other URL keeps
//! the external-bridge behavior.

use crate::messaging::{Dispatcher, MediaAttachment, MessageEvent};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

/// hermes poll cadence (`await asyncio.sleep(1)`).
const DEFAULT_POLL_INTERVAL_MS: u64 = 1000;
/// Outbound text chunk limit (WhatsApp allows ~65k; hermes chunks long
/// replies — keep the conservative 4000 used by other adapters).
const MAX_MESSAGE_LENGTH: usize = 4000;
const API_TIMEOUT: Duration = Duration::from_secs(30);
/// hermes `_OWNER_REPLY_PREFIX` — owner-typed self-chat marker.
const OWNER_REPLY_PREFIX: &str = "[owner reply] ";

/// `[messaging.whatsapp]` — Baileys-bridge adapter (hermes
/// `platforms.whatsapp` plugin config + `WHATSAPP_*` env vars).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WhatsappConfig {
    pub enabled: bool,
    /// Base URL of the Baileys HTTP bridge (fallback
    /// `WHATSAPP_BRIDGE_URL`, default `http://127.0.0.1:3000`).
    pub bridge_url: String,
    /// Sender JIDs/phone numbers allowed in DMs (fallback
    /// `WHATSAPP_ALLOWED_USERS`).
    pub allowed_users: Vec<String>,
    /// When non-empty, the bot ONLY answers in these group JIDs.
    pub allowed_channels: Vec<String>,
    /// Require an @mention of the bot in groups (hermes behavior).
    pub require_mention: bool,
    /// Group JIDs exempt from the mention requirement.
    pub free_response_channels: Vec<String>,
    /// Cron/notification delivery chat (hermes `WHATSAPP_HOME_CHANNEL`).
    pub home_channel: String,
    /// Inbound poll interval in milliseconds.
    pub poll_interval_ms: u64,
    /// Send read receipts for accepted messages (hermes
    /// `send_read_receipts`).
    pub read_receipts: bool,
    /// Spawn and supervise the bundled Node Baileys bridge (hermes
    /// adapter lifecycle). Only effective when `bridge_url` targets
    /// `127.0.0.1:<bridge_port>`; other URLs stay external bridges
    /// (fallback `WHATSAPP_AUTO_SPAWN`).
    pub auto_spawn: bool,
    /// Bundled bridge port (hermes `bridge_port`; fallback
    /// `WHATSAPP_BRIDGE_PORT`, default 3000).
    pub bridge_port: u16,
    /// Bundled bridge session directory (hermes `session_path`, default
    /// `<home>/platforms/whatsapp/session`; fallback
    /// `WHATSAPP_SESSION_PATH`).
    pub session_path: String,
    /// Override path to `bridge.js` (hermes `bridge_script`; default is
    /// the bundled script synced under `<home>/scripts/whatsapp-bridge`).
    pub bridge_script: String,
    /// Bridge mode (hermes `WHATSAPP_MODE`, default `self-chat`).
    pub mode: String,
}

impl Default for WhatsappConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bridge_url: String::new(),
            allowed_users: Vec::new(),
            allowed_channels: Vec::new(),
            require_mention: true,
            free_response_channels: Vec::new(),
            home_channel: String::new(),
            poll_interval_ms: DEFAULT_POLL_INTERVAL_MS,
            read_receipts: true,
            auto_spawn: true,
            bridge_port: 3000,
            session_path: String::new(),
            bridge_script: String::new(),
            mode: "self-chat".to_string(),
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
pub struct ResolvedWhatsapp {
    pub bridge_url: String,
    pub allowed_users: Vec<String>,
    pub allowed_channels: Vec<String>,
    pub require_mention: bool,
    pub free_response_channels: Vec<String>,
    pub home_channel: String,
    pub poll_interval_ms: u64,
    pub read_receipts: bool,
    pub auto_spawn: bool,
    pub bridge_port: u16,
    pub session_path: String,
    pub bridge_script: String,
    pub mode: String,
}

impl WhatsappConfig {
    pub fn resolve(&self) -> ResolvedWhatsapp {
        ResolvedWhatsapp {
            bridge_url: env_trim("WHATSAPP_BRIDGE_URL")
                .unwrap_or_else(|| self.bridge_url.clone())
                .trim_end_matches('/')
                .to_string(),
            allowed_users: env_list("WHATSAPP_ALLOWED_USERS")
                .unwrap_or_else(|| self.allowed_users.clone()),
            allowed_channels: env_list("WHATSAPP_ALLOWED_CHANNELS")
                .unwrap_or_else(|| self.allowed_channels.clone()),
            require_mention: env_trim("WHATSAPP_REQUIRE_MENTION")
                .map(|v| !matches!(v.to_lowercase().as_str(), "false" | "0" | "no"))
                .unwrap_or(self.require_mention),
            free_response_channels: env_list("WHATSAPP_FREE_RESPONSE_CHANNELS")
                .unwrap_or_else(|| self.free_response_channels.clone()),
            home_channel: env_trim("WHATSAPP_HOME_CHANNEL")
                .unwrap_or_else(|| self.home_channel.clone()),
            poll_interval_ms: env_trim("WHATSAPP_POLL_INTERVAL_MS")
                .and_then(|v| v.parse().ok())
                .unwrap_or(self.poll_interval_ms)
                .max(100),
            read_receipts: env_trim("WHATSAPP_READ_RECEIPTS")
                .map(|v| !matches!(v.to_lowercase().as_str(), "false" | "0" | "no"))
                .unwrap_or(self.read_receipts),
            auto_spawn: env_trim("WHATSAPP_AUTO_SPAWN")
                .map(|v| !matches!(v.to_lowercase().as_str(), "false" | "0" | "no" | "off"))
                .unwrap_or(self.auto_spawn),
            bridge_port: env_trim("WHATSAPP_BRIDGE_PORT")
                .and_then(|v| v.parse().ok())
                .unwrap_or(self.bridge_port),
            session_path: env_trim("WHATSAPP_SESSION_PATH")
                .unwrap_or_else(|| self.session_path.clone()),
            bridge_script: env_trim("WHATSAPP_BRIDGE_SCRIPT")
                .unwrap_or_else(|| self.bridge_script.clone()),
            mode: env_trim("WHATSAPP_MODE")
                .unwrap_or_else(|| self.mode.clone())
                .to_lowercase(),
        }
    }
}

struct Runtime {
    cfg: ResolvedWhatsapp,
    client: reqwest::Client,
    /// Bot JID reported by `/health` (e.g. `12345@s.whatsapp.net`),
    /// used for group mention detection.
    bot_jid: tokio::sync::Mutex<String>,
    /// Supervised bundled bridge child, when auto-spawned (hermes
    /// `_bridge_process`).
    bridge: tokio::sync::Mutex<Option<crate::whatsapp_bridge::BridgeProcess>>,
}

/// hermes `_should_process_message` — drop broadcast pseudo-chats and
/// from-self echoes. In self-chat mode the bridge flags owner-typed
/// fromMe messages with `fromOwner` (linked-device sends that are not
/// echoes of our own `/send`); those pass intake and get the `[owner
/// reply]` prefix downstream (hermes `_OWNER_REPLY_PREFIX`).
pub fn should_process(data: &Value) -> bool {
    let chat_id = data.get("chatId").and_then(|v| v.as_str()).unwrap_or("");
    if chat_id.is_empty()
        || chat_id == "status@broadcast"
        || chat_id.ends_with("@broadcast")
        || chat_id.ends_with("@newsletter")
    {
        return false;
    }
    let from_me = data
        .get("fromMe")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let from_owner = data
        .get("fromOwner")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if from_me && !from_owner {
        return false;
    }
    true
}

/// Map a bridge `mediaType` string onto a coarse kind used for caching
/// (mirrors hermes `_build_message_event` type detection).
pub fn media_kind_for(media_type: &str) -> &'static str {
    if media_type.contains("image") {
        "image"
    } else if media_type.contains("video") || media_type == "gif" {
        "video"
    } else if media_type.contains("ptt") || media_type.contains("voice") {
        "audio"
    } else if media_type.contains("audio") {
        "audio"
    } else if media_type == "sticker" {
        "image"
    } else if media_type.is_empty() {
        "text"
    } else {
        "document"
    }
}

/// Extension-derived `mediaType` for `/send-media` (hermes
/// `_map_local_media_type`).
pub fn send_media_type(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" | "png" | "gif" => "image",
        "webp" => "sticker",
        "mp4" | "mov" | "avi" | "mkv" => "video",
        "mp3" | "ogg" | "opus" | "m4a" | "wav" | "aac" => "audio",
        _ => "document",
    }
}

/// Group mention detection: `@<bot-user-part>` when the bot JID is
/// known, otherwise any `@` mention token.
pub fn text_mentions_bot(text: &str, bot_jid: &str) -> bool {
    if let Some(user_part) = bot_jid.split('@').next().filter(|u| !u.is_empty()) {
        return text.contains(&format!("@{user_part}"));
    }
    text.split_whitespace()
        .any(|word| word.starts_with('@') && word.len() > 1)
}

/// Entry point spawned by `run_messaging`.
pub async fn run(
    cfg: WhatsappConfig,
    dispatcher: Arc<Dispatcher>,
    pairing: Option<Arc<crate::pairing::PairingStore>>,
) {
    let mut resolved = cfg.resolve();
    if resolved.bridge_url.is_empty() {
        resolved.bridge_url = format!("http://127.0.0.1:{}", resolved.bridge_port);
    }
    let home = crate::config::ulnclaw_home();
    let session_path = session_dir_for(&resolved);
    let spawn_enabled = resolved.auto_spawn
        && crate::whatsapp_bridge::is_local_bridge_target(
            &resolved.bridge_url,
            resolved.bridge_port,
        );
    let runtime = Arc::new(Runtime {
        client: reqwest::Client::builder()
            .timeout(API_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new()),
        cfg: resolved,
        bot_jid: tokio::sync::Mutex::new(String::new()),
        bridge: tokio::sync::Mutex::new(None),
    });
    crate::messaging::register_platform_sender(
        "whatsapp",
        Arc::new(WhatsappSender {
            runtime: runtime.clone(),
        }),
    );

    let mut delay = 5u64;
    loop {
        // hermes connect(): (re)spawn the bundled bridge when we manage
        // it and it is absent or has exited since the last session.
        if spawn_enabled {
            let mut guard = runtime.bridge.lock().await;
            let needs_spawn = match guard.as_mut() {
                Some(bridge) => bridge.exited(),
                None => true,
            };
            if needs_spawn {
                if let Some(dead) = guard.take() {
                    eprintln!("[whatsapp] bridge pid {} exited; respawning", dead.pid);
                }
                *guard = crate::whatsapp_bridge::ensure_and_spawn(
                    &home,
                    runtime.cfg.bridge_port,
                    &session_path,
                    &runtime.cfg.mode,
                    runtime.cfg.read_receipts,
                    &runtime.cfg.bridge_script,
                )
                .await;
            }
        }
        match run_session(&runtime, &dispatcher, &pairing).await {
            Ok(()) => delay = 5,
            Err(msg) => eprintln!("[whatsapp] session error: {msg}"),
        }
        tokio::time::sleep(Duration::from_secs(delay)).await;
        delay = (delay * 2).min(60);
    }
}

async fn run_session(
    runtime: &Arc<Runtime>,
    dispatcher: &Arc<Dispatcher>,
    pairing: &Option<Arc<crate::pairing::PairingStore>>,
) -> Result<(), String> {
    // Wait for the bridge to report a connected state (hermes connect
    // loop: health probes until `status == "connected"`, pairing QR
    // states are surfaced and retried).
    loop {
        let url = format!("{}/health", runtime.cfg.bridge_url);
        match runtime.client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                let data: Value = resp.json().await.unwrap_or(json!({}));
                let status = data
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                if status == "connected" {
                    if let Some(jid) = data
                        .get("botJid")
                        .or_else(|| data.get("jid"))
                        .and_then(|v| v.as_str())
                    {
                        *runtime.bot_jid.lock().await = jid.to_string();
                    }
                    eprintln!("[whatsapp] bridge connected at {}", runtime.cfg.bridge_url);
                    break;
                }
                if status == "qr" || status.contains("pairing") {
                    eprintln!(
                        "[whatsapp] bridge waiting for QR pairing — scan the QR code in bridge.log (next to the session directory) or the bridge console"
                    );
                } else {
                    eprintln!("[whatsapp] bridge status: {status} — waiting for connection");
                }
            }
            Ok(resp) => {
                return Err(format!("health check HTTP {}", resp.status()));
            }
            Err(e) => {
                return Err(format!(
                    "bridge unreachable at {}: {e}",
                    runtime.cfg.bridge_url
                ));
            }
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    // Poll loop (hermes `_poll_messages`).
    let interval = Duration::from_millis(runtime.cfg.poll_interval_ms);
    loop {
        let url = format!("{}/messages", runtime.cfg.bridge_url);
        let resp = runtime
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("poll failed: {e}"))?;
        if resp.status().is_success() {
            let messages: Vec<Value> = resp.json().await.unwrap_or_default();
            for data in &messages {
                handle_bridge_message(runtime, dispatcher, pairing, data).await;
            }
        } else {
            eprintln!("[whatsapp] poll HTTP {}", resp.status());
        }
        tokio::time::sleep(interval).await;
    }
}

/// Effective bridge session directory (P719: the lid-mapping lookup
/// for identity canonicalisation shares the exact path the bridge is
/// spawned with).
fn session_dir_for(cfg: &ResolvedWhatsapp) -> std::path::PathBuf {
    if cfg.session_path.is_empty() {
        crate::config::ulnclaw_home()
            .join("platforms")
            .join("whatsapp")
            .join("session")
    } else {
        std::path::PathBuf::from(&cfg.session_path)
    }
}

/// Process one bridge message object (hermes `_build_message_event` +
/// dispatch + read receipt).
async fn handle_bridge_message(
    runtime: &Arc<Runtime>,
    dispatcher: &Arc<Dispatcher>,
    pairing: &Option<Arc<crate::pairing::PairingStore>>,
    data: &Value,
) {
    if !should_process(data) {
        return;
    }
    let raw_chat_id = data
        .get("chatId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let is_group = data
        .get("isGroup")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let raw_sender_id = data
        .get("senderId")
        .and_then(|v| v.as_str())
        .unwrap_or(&raw_chat_id)
        .to_string();
    // P719: canonical WhatsApp identity (hermes whatsapp_identity) —
    // collapse phone-JID/LID aliases so session keys, allowlists and
    // pairing agree on one stable identity per human. Group/broadcast
    // addresses are chat addresses, not user identities, and pass
    // through untouched.
    let session_dir = session_dir_for(&runtime.cfg);
    let chat_id = if crate::whatsapp_identity::is_user_identifier(&raw_chat_id) {
        crate::whatsapp_identity::canonical_whatsapp_identifier(&raw_chat_id, &session_dir)
    } else {
        raw_chat_id.clone()
    };
    let sender_id = if crate::whatsapp_identity::is_user_identifier(&raw_sender_id) {
        crate::whatsapp_identity::canonical_whatsapp_identifier(&raw_sender_id, &session_dir)
    } else {
        raw_sender_id.clone()
    };
    let sender_name = data
        .get("senderName")
        .and_then(|v| v.as_str())
        .unwrap_or(&sender_id)
        .to_string();
    let message_id = data
        .get("messageId")
        .or_else(|| data.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let mut text = data
        .get("text")
        .or_else(|| data.get("body"))
        .or_else(|| data.get("caption"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    // hermes `_OWNER_REPLY_PREFIX`: owner-typed fromMe messages (self-chat
    // mode, linked-device sends) carry the marker into transcripts.
    let from_owner = data
        .get("fromOwner")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if from_owner && !text.starts_with(OWNER_REPLY_PREFIX) {
        text = format!("{OWNER_REPLY_PREFIX}{text}");
    }

    if is_group {
        if !runtime.cfg.allowed_channels.is_empty()
            && !runtime
                .cfg
                .allowed_channels
                .iter()
                .any(|c| c == &chat_id || c == "*")
        {
            return;
        }
        let exempt = runtime
            .cfg
            .free_response_channels
            .iter()
            .any(|c| c == &chat_id);
        if runtime.cfg.require_mention && !exempt {
            let bot_jid = runtime.bot_jid.lock().await.clone();
            if !text_mentions_bot(&text, &bot_jid) {
                return;
            }
        }
    } else if !sender_allowed(runtime, pairing, &sender_id, &sender_name, &chat_id).await {
        return;
    }

    // Media: bridge-cached local paths or URLs → media cache.
    let mut attachments = Vec::new();
    let has_media = data
        .get("hasMedia")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if has_media {
        let media_type = data.get("mediaType").and_then(|v| v.as_str()).unwrap_or("");
        let mime = data
            .get("mime")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let urls = data
            .get("mediaUrls")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        for url_value in urls {
            let Some(url) = url_value.as_str() else {
                continue;
            };
            if let Some(att) = fetch_media(runtime, url, &mime, media_type).await {
                attachments.push(att);
            }
        }
    }
    if text.is_empty() && attachments.is_empty() {
        return;
    }
    if text.is_empty() {
        text = "[media message]".to_string();
    }

    let mut event = MessageEvent {
        platform: "whatsapp".into(),
        chat_id: chat_id.clone(),
        sender_id,
        sender_name,
        text,
        message_id,
        attachments,
    };
    if !crate::messaging::pre_gateway_dispatch_gate_public(&mut event).await {
        return;
    }
    // Fire-and-forget read receipt (hermes pattern: never block dispatch).
    if runtime.cfg.read_receipts {
        if let Some(key) = data.get("readReceiptKey") {
            if key.is_object() {
                let runtime = runtime.clone();
                let key = key.clone();
                tokio::spawn(async move {
                    send_read_receipt(&runtime, &key).await;
                });
            }
        }
    }
    let outcome = match dispatcher.handle_event(event).await {
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
        send_media(runtime, &chat_id, path, "").await;
    }
    let reply_text = reply_text.trim().to_string();
    if !reply_text.is_empty() {
        // P705: ledger-protected reply delivery (all chunks).
        dispatcher
            .try_send_with_ledger("whatsapp", &chat_id, &reply_text, || async {
                let mut result: Result<(), String> = Ok(());
                for chunk in crate::messaging::chunk_text(&reply_text, MAX_MESSAGE_LENGTH) {
                    if let Err(e) = send_text(runtime, &chat_id, &chunk).await {
                        eprintln!("[whatsapp] reply to {chat_id} failed: {e}");
                        if result.is_ok() {
                            result = Err(e.to_string());
                        }
                    }
                }
                result
            })
            .await;
    }
}

/// Download (URL) or read (bridge-cached absolute path) inbound media
/// into the content-addressed cache.
async fn fetch_media(
    runtime: &Arc<Runtime>,
    url: &str,
    mime: &str,
    media_type: &str,
) -> Option<MediaAttachment> {
    let home = crate::config::ulnclaw_home();
    let kind = media_kind_for(media_type);
    let fallback_mime = match kind {
        "image" => "image/jpeg",
        "video" => "video/mp4",
        "audio" => "audio/ogg",
        _ => "application/octet-stream",
    };
    let mime = if mime.is_empty() { fallback_mime } else { mime };
    if url.starts_with("http://") || url.starts_with("https://") {
        match runtime.client.get(url).send().await {
            Ok(resp) if resp.status().is_success() => match resp.bytes().await {
                Ok(bytes) => {
                    let len = bytes.len() as u64;
                    match crate::media_cache::cache_media_bytes(&home, &bytes, mime, "") {
                        Ok(path) => Some(MediaAttachment {
                            path,
                            mime: mime.to_string(),
                            bytes: len,
                            original_name: String::new(),
                        }),
                        Err(e) => {
                            eprintln!("[whatsapp] media cache failed: {e}");
                            None
                        }
                    }
                }
                Err(e) => {
                    eprintln!("[whatsapp] media download read failed: {e}");
                    None
                }
            },
            Ok(resp) => {
                eprintln!("[whatsapp] media download HTTP {}", resp.status());
                None
            }
            Err(e) => {
                eprintln!("[whatsapp] media download failed: {e}");
                None
            }
        }
    } else if std::path::Path::new(url).is_absolute() {
        // Bridge already downloaded the media to a local file.
        match tokio::fs::read(url).await {
            Ok(bytes) => {
                let len = bytes.len() as u64;
                let name = std::path::Path::new(url)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                match crate::media_cache::cache_media_bytes(&home, &bytes, mime, &name) {
                    Ok(path) => Some(MediaAttachment {
                        path,
                        mime: mime.to_string(),
                        bytes: len,
                        original_name: name,
                    }),
                    Err(e) => {
                        eprintln!("[whatsapp] media cache failed: {e}");
                        None
                    }
                }
            }
            Err(e) => {
                eprintln!("[whatsapp] bridge media path unreadable ({url}): {e}");
                None
            }
        }
    } else {
        eprintln!("[whatsapp] rejecting non-absolute bridge media path: {url}");
        None
    }
}

async fn send_read_receipt(runtime: &Arc<Runtime>, key: &Value) {
    let url = format!("{}/read", runtime.cfg.bridge_url);
    let result = runtime
        .client
        .post(&url)
        .json(&json!({ "key": key }))
        .send()
        .await;
    match result {
        Ok(resp) if resp.status().is_success() => {}
        Ok(resp) => eprintln!("[whatsapp] read receipt HTTP {}", resp.status()),
        Err(e) => eprintln!("[whatsapp] read receipt failed: {e}"),
    }
}

/// hermes `/send` — `{chatId, message}`.
async fn send_text(runtime: &Arc<Runtime>, chat_id: &str, text: &str) -> Result<(), String> {
    let url = format!("{}/send", runtime.cfg.bridge_url);
    // P719: bare phone numbers crash Baileys' jidDecode — send with a
    // fully-qualified JID (hermes to_whatsapp_jid).
    let target = crate::whatsapp_identity::to_whatsapp_jid(chat_id);
    let payload = json!({ "chatId": target, "message": text });
    let resp = runtime
        .client
        .post(&url)
        .json(&payload)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.status().is_success() {
        Ok(())
    } else {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        Err(format!("HTTP {status}: {body}"))
    }
}

/// hermes `/send-media` — `{to, path, mediaType, caption}`.
async fn send_media(runtime: &Arc<Runtime>, chat_id: &str, path: &std::path::Path, caption: &str) {
    let url = format!("{}/send-media", runtime.cfg.bridge_url);
    let payload = json!({
        "to": crate::whatsapp_identity::to_whatsapp_jid(chat_id),
        "path": path.to_string_lossy(),
        "mediaType": send_media_type(&path.to_string_lossy()),
        "caption": caption,
    });
    match runtime.client.post(&url).json(&payload).send().await {
        Ok(resp) if resp.status().is_success() => {}
        Ok(resp) => eprintln!("[whatsapp] send-media HTTP {}", resp.status()),
        Err(e) => eprintln!("[whatsapp] send-media failed: {e}"),
    }
}

/// hermes `send_poll` payload (bridge `/send-poll`).
pub fn send_poll_payload(
    chat_id: &str,
    question: &str,
    options: &[String],
    selectable_count: u32,
) -> Value {
    json!({
        "chatId": chat_id,
        "question": question,
        "options": options,
        "selectableCount": selectable_count,
    })
}

/// hermes `send_location` payload (bridge `/send-location`).
pub fn send_location_payload(
    chat_id: &str,
    latitude: f64,
    longitude: f64,
    name: &str,
    address: &str,
) -> Value {
    let mut payload = json!({
        "chatId": chat_id,
        "latitude": latitude,
        "longitude": longitude,
    });
    if !name.is_empty() {
        payload["name"] = json!(name);
    }
    if !address.is_empty() {
        payload["address"] = json!(address);
    }
    payload
}

/// hermes `edit_message` payload (bridge `/edit`).
pub fn edit_message_payload(chat_id: &str, message_id: &str, message: &str) -> Value {
    json!({
        "chatId": chat_id,
        "messageId": message_id,
        "message": message,
    })
}

/// hermes `send_poll`: native WhatsApp poll via the bridge. Low-level
/// transport primitive — clarify prompts map onto it through
/// `send_clarify`; approval UX stays gateway-owned.
async fn send_poll(
    runtime: &Arc<Runtime>,
    chat_id: &str,
    question: &str,
    options: &[String],
    selectable_count: u32,
) -> Result<String, String> {
    let url = format!("{}/send-poll", runtime.cfg.bridge_url);
    let resp = runtime
        .client
        .post(&url)
        .json(&send_poll_payload(
            chat_id,
            question,
            options,
            selectable_count,
        ))
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("HTTP {status}: {body}"));
    }
    let value: Value = resp.json().await.unwrap_or(json!({}));
    Ok(value
        .get("messageId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string())
}

/// hermes `send_location`: native WhatsApp location pin via the bridge.
#[allow(dead_code)] // transport primitive; no agent tool surface yet
async fn send_location(
    runtime: &Arc<Runtime>,
    chat_id: &str,
    latitude: f64,
    longitude: f64,
    name: &str,
    address: &str,
) -> Result<String, String> {
    let url = format!("{}/send-location", runtime.cfg.bridge_url);
    let resp = runtime
        .client
        .post(&url)
        .json(&send_location_payload(
            chat_id, latitude, longitude, name, address,
        ))
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("HTTP {status}: {body}"));
    }
    let value: Value = resp.json().await.unwrap_or(json!({}));
    Ok(value
        .get("messageId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string())
}

/// hermes `edit_message`: edit a previously sent message via the
/// bridge. Best-effort transport primitive.
#[allow(dead_code)] // transport primitive; no streaming-edit surface yet
async fn edit_message(
    runtime: &Arc<Runtime>,
    chat_id: &str,
    message_id: &str,
    content: &str,
) -> Result<(), String> {
    let url = format!("{}/edit", runtime.cfg.bridge_url);
    let resp = runtime
        .client
        .post(&url)
        .json(&edit_message_payload(chat_id, message_id, content))
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.status().is_success() {
        Ok(())
    } else {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        Err(format!("HTTP {status}: {body}"))
    }
}

/// hermes `send_clarify` choice cleaning: trim, drop blanks.
pub fn clarify_clean_choices(choices: &[String]) -> Vec<String> {
    choices
        .iter()
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
        .collect()
}

/// hermes `send_clarify` poll eligibility: 2–12 clean choices.
pub fn clarify_poll_eligible(clean_count: usize) -> bool {
    (2..=12).contains(&clean_count)
}
/// DM allowlist∪pairing gate (hermes authorization semantics).
async fn sender_allowed(
    runtime: &Arc<Runtime>,
    pairing: &Option<Arc<crate::pairing::PairingStore>>,
    sender_id: &str,
    sender_name: &str,
    chat_id: &str,
) -> bool {
    // P719: match any known alias of the sender (phone-JID vs LID)
    // against the allowlist / pairing records (hermes
    // `expand_whatsapp_aliases` authorisation semantics).
    let aliases = crate::whatsapp_identity::expand_whatsapp_aliases(
        sender_id,
        &session_dir_for(&runtime.cfg),
    );
    let alias_match = |candidate: &str| {
        let normalized = crate::whatsapp_identity::normalize_whatsapp_identifier(candidate);
        aliases.contains(candidate) || (!normalized.is_empty() && aliases.contains(&normalized))
    };
    if runtime
        .cfg
        .allowed_users
        .iter()
        .any(|u| u == "*" || alias_match(u))
    {
        return true;
    }
    if let Some(store) = pairing {
        if aliases
            .iter()
            .any(|alias| store.is_approved("whatsapp", alias))
        {
            return true;
        }
        if let Some(code_msg) =
            crate::messaging::pairing_offer_public(store, "whatsapp", sender_id, sender_name)
        {
            let _ = send_text(runtime, chat_id, &code_msg).await;
        } else {
            eprintln!(
                "[whatsapp] unauthorized DM from {sender_id} — add to allowed_users or approve pairing"
            );
        }
        return false;
    }
    eprintln!("[whatsapp] unauthorized DM from {sender_id} — add to allowed_users");
    false
}

struct WhatsappSender {
    runtime: Arc<Runtime>,
}

#[async_trait::async_trait]
impl crate::messaging::PlatformSender for WhatsappSender {
    async fn send_text(&self, chat_id: &str, text: &str) {
        for chunk in crate::messaging::chunk_text(text, MAX_MESSAGE_LENGTH) {
            if let Err(e) = send_text(&self.runtime, chat_id, &chunk).await {
                eprintln!("[whatsapp] send_text to {chat_id} failed: {e}");
            }
        }
    }

    /// hermes `send_clarify`: multiple-choice clarifies render as a
    /// native WhatsApp poll (2–12 clean choices, single-select); the
    /// selected option later arrives as plain message text and the
    /// normal clarify text-intercept resolves the pending question.
    /// Failures and open-ended prompts fall back to numbered text.
    async fn send_clarify(
        &self,
        chat_id: &str,
        _clarify_id: &str,
        question: &str,
        choices: &[String],
    ) -> bool {
        let clean = clarify_clean_choices(choices);
        if !clarify_poll_eligible(clean.len()) {
            return false;
        }
        match send_poll(&self.runtime, chat_id, question.trim(), &clean, 1).await {
            Ok(_) => true,
            Err(e) => {
                eprintln!("[whatsapp] native clarify poll failed; falling back to text: {e}");
                false
            }
        }
    }
}

// ---------------------------------------------------------------------------
// `ulnclaw whatsapp status` — read-only bridge diagnostics (lean parity
// for hermes `hermes whatsapp`, whose wizard role is filled by the
// gateway-supervised auto-spawn + QR-on-startup flow)
// ---------------------------------------------------------------------------

/// One-line summary of a bridge `/health` payload (status + identity).
pub fn health_summary(health: &Value) -> String {
    let status = health
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    match status {
        "connected" => {
            let jid = health
                .get("botJid")
                .or_else(|| health.get("jid"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown JID");
            format!("connected as {jid}")
        }
        "qr" => "waiting for QR pairing — scan the QR in bridge.log".to_string(),
        s if s.contains("pairing") => {
            format!("{s} — finish pairing in bridge.log")
        }
        other => format!("status: {other}"),
    }
}

/// Probe the bridge `/health` endpoint with a short timeout.
async fn probe_health(bridge_url: &str) -> Result<Value, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .get(format!("{}/health", bridge_url.trim_end_matches('/')))
        .send()
        .await
        .map_err(|e| format!("unreachable: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    resp.json().await.map_err(|e| e.to_string())
}

/// Full status report for `ulnclaw whatsapp status`.
pub async fn whatsapp_status(config: &crate::config::UlncLawConfig) -> String {
    use crate::whatsapp_bridge as wb;
    let home = crate::config::ulnclaw_home();
    let cfg = config.messaging.whatsapp.resolve();
    let mut out = String::new();
    out.push_str("WhatsApp (Baileys bridge)\n");

    // Platform state.
    if config.messaging.whatsapp.enabled {
        out.push_str(&format!(
            "  Platform:   enabled (mode: {})\n",
            if cfg.mode.is_empty() {
                "self-chat"
            } else {
                cfg.mode.as_str()
            }
        ));
    } else {
        out.push_str(
            "  Platform:   disabled — `ulnclaw config set messaging.whatsapp.enabled true`\n",
        );
    }

    // Bridge target.
    let bridge_url = if cfg.bridge_url.is_empty() {
        format!("http://127.0.0.1:{}", cfg.bridge_port)
    } else {
        cfg.bridge_url.clone()
    };
    let local = wb::is_local_bridge_target(&bridge_url, cfg.bridge_port);
    out.push_str(&format!(
        "  Bridge URL: {} ({})\n",
        bridge_url,
        if local && cfg.auto_spawn {
            "bundled, auto-spawned by the gateway"
        } else if local {
            "local, external (auto_spawn off)"
        } else {
            "external"
        }
    ));

    // Node + script installation (only meaningful for the bundled bridge).
    if local {
        match wb::find_node_executable("node") {
            Some(node) => out.push_str(&format!("  Node.js:    {}\n", node.display())),
            None => out.push_str("  Node.js:    NOT FOUND — install Node 18+ to run the bridge\n"),
        }
        // NB: wb::resolve_bridge_dir() syncs the script on call — the
        // status command stays read-only and just inspects the path.
        let bridge_dir = home.join("scripts").join("whatsapp-bridge");
        if bridge_dir.join("bridge.js").exists() {
            let fresh = wb::deps_fresh(&bridge_dir);
            out.push_str(&format!(
                "  Script dir: {} (deps {})\n",
                bridge_dir.display(),
                if fresh {
                    "fresh"
                } else {
                    "stale — gateway reinstalls on next start"
                }
            ));
        } else {
            out.push_str(&format!(
                "  Script dir: {} (not installed yet — gateway syncs it on first start)\n",
                bridge_dir.display()
            ));
        }
        // Process state from the pidfile.
        let session_dir = if cfg.session_path.is_empty() {
            home.join("platforms").join("whatsapp").join("session")
        } else {
            std::path::PathBuf::from(&cfg.session_path)
        };
        match wb::read_bridge_pidfile(&session_dir) {
            Some((pid, start)) if wb::pid_exists(pid) => {
                let ours = wb::bridge_pid_is_ours(pid, &session_dir, start);
                out.push_str(&format!(
                    "  Process:    running (pid {}{})\n",
                    pid,
                    if ours { "" } else { ", foreign pidfile" }
                ));
            }
            Some((pid, _)) => {
                out.push_str(&format!(
                    "  Process:    stale pidfile (pid {pid} not running)\n"
                ));
            }
            None => out.push_str("  Process:    not running (gateway starts it on demand)\n"),
        }
    }

    // Live health probe.
    out.push_str(&format!(
        "  Health:     {}\n",
        match probe_health(&bridge_url).await {
            Ok(health) => health_summary(&health),
            Err(e) => format!("bridge {e}"),
        }
    ));

    // Next steps.
    if !config.messaging.whatsapp.enabled {
        out.push_str("\n  Next: enable the platform, then start `ulnclaw gateway` — the\n");
        out.push_str("  bridge installs and launches automatically; scan the QR code\n");
        out.push_str("  printed in bridge.log to pair your WhatsApp account.\n");
    } else {
        out.push_str("\n  Next: start `ulnclaw gateway` (bridge auto-spawns when local);\n");
        out.push_str("  watch bridge.log next to the session directory for the QR code.\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(json_str: &str) -> Value {
        serde_json::from_str(json_str).unwrap()
    }

    #[test]
    fn health_summary_variants() {
        assert_eq!(
            health_summary(&msg(
                r#"{"status":"connected","botJid":"123@s.whatsapp.net"}"#
            )),
            "connected as 123@s.whatsapp.net"
        );
        assert!(health_summary(&msg(r#"{"status":"connected"}"#)).contains("unknown JID"));
        assert!(health_summary(&msg(r#"{"status":"qr"}"#)).contains("QR"));
        assert!(health_summary(&msg(r#"{"status":"pairing-code"}"#)).contains("pairing"));
        assert!(health_summary(&msg(r#"{"status":"starting"}"#)).contains("starting"));
        assert!(health_summary(&msg(r#"{}"#)).contains("unknown"));
    }

    #[test]
    fn process_drops_from_me_and_status() {
        assert!(!should_process(&msg(
            r#"{"chatId":"123@s.whatsapp.net","fromMe":true}"#
        )));
        assert!(!should_process(&msg(r#"{"chatId":"status@broadcast"}"#)));
        assert!(!should_process(&msg(r#"{"chatId":"abc@newsletter"}"#)));
        assert!(!should_process(&msg(r#"{"chatId":"xyz@broadcast"}"#)));
        assert!(!should_process(&msg(r#"{"chatId":""}"#)));
        assert!(should_process(&msg(r#"{"chatId":"123@s.whatsapp.net"}"#)));
        assert!(should_process(&msg(
            r#"{"chatId":"456@g.us","isGroup":true}"#
        )));
    }

    #[test]
    fn owner_flagged_from_me_passes_intake() {
        // hermes self-chat: bridge flags owner-typed fromMe messages
        // (not echoes of our own /send) with fromOwner.
        assert!(should_process(&msg(
            r#"{"chatId":"123@s.whatsapp.net","fromMe":true,"fromOwner":true}"#
        )));
        assert!(!should_process(&msg(
            r#"{"chatId":"123@s.whatsapp.net","fromMe":true,"fromOwner":false}"#
        )));
        assert!(!should_process(&msg(
            r#"{"chatId":"status@broadcast","fromMe":true,"fromOwner":true}"#
        )));
    }

    #[test]
    fn media_kind_mapping() {
        assert_eq!(media_kind_for("image"), "image");
        assert_eq!(media_kind_for("video"), "video");
        assert_eq!(media_kind_for("ptt"), "audio");
        assert_eq!(media_kind_for("audio"), "audio");
        assert_eq!(media_kind_for("document"), "document");
        assert_eq!(media_kind_for("sticker"), "image");
        assert_eq!(media_kind_for(""), "text");
    }

    #[test]
    fn send_media_type_by_extension() {
        assert_eq!(send_media_type("/tmp/photo.jpg"), "image");
        assert_eq!(send_media_type("/tmp/sticker.webp"), "sticker");
        assert_eq!(send_media_type("/tmp/clip.mp4"), "video");
        assert_eq!(send_media_type("/tmp/voice.ogg"), "audio");
        assert_eq!(send_media_type("/tmp/report.pdf"), "document");
        assert_eq!(send_media_type("noext"), "document");
    }

    #[test]
    fn mention_detection_with_bot_jid() {
        let jid = "15551234567@s.whatsapp.net";
        assert!(text_mentions_bot("hey @15551234567 help", jid));
        assert!(!text_mentions_bot("hey everyone", jid));
        assert!(!text_mentions_bot("hey @9998887777", jid));
    }

    #[test]
    fn mention_detection_without_jid_falls_back_to_any_mention() {
        assert!(text_mentions_bot("hey @someone help", ""));
        assert!(!text_mentions_bot("no mentions here", ""));
        assert!(!text_mentions_bot("email me at me@", ""));
    }

    #[test]
    fn resolve_defaults_and_env() {
        let _guard = crate::models_dev::test_env_lock();
        let cfg = WhatsappConfig::default();
        let resolved = cfg.resolve();
        assert_eq!(resolved.bridge_url, "");
        assert!(resolved.require_mention);
        assert_eq!(resolved.poll_interval_ms, DEFAULT_POLL_INTERVAL_MS);
        assert!(resolved.auto_spawn);
        assert_eq!(resolved.bridge_port, 3000);
        assert_eq!(resolved.mode, "self-chat");
        assert!(resolved.session_path.is_empty());
        assert!(resolved.bridge_script.is_empty());

        std::env::set_var("WHATSAPP_BRIDGE_URL", "http://10.0.0.5:3000/");
        std::env::set_var("WHATSAPP_REQUIRE_MENTION", "false");
        let resolved = cfg.resolve();
        assert_eq!(resolved.bridge_url, "http://10.0.0.5:3000");
        assert!(!resolved.require_mention);
        std::env::remove_var("WHATSAPP_BRIDGE_URL");
        std::env::remove_var("WHATSAPP_REQUIRE_MENTION");
    }

    #[test]
    fn poll_interval_has_a_floor() {
        let _guard = crate::models_dev::test_env_lock();
        std::env::set_var("WHATSAPP_POLL_INTERVAL_MS", "5");
        let resolved = WhatsappConfig::default().resolve();
        assert_eq!(resolved.poll_interval_ms, 100);
        std::env::remove_var("WHATSAPP_POLL_INTERVAL_MS");
    }

    #[test]
    fn text_extraction_prefers_text_then_body_then_caption() {
        let data = msg(r#"{"text":"hello","body":"ignored"}"#);
        let text = data
            .get("text")
            .or_else(|| data.get("body"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert_eq!(text, "hello");
        let data = msg(r#"{"body":"from body"}"#);
        let text = data
            .get("text")
            .or_else(|| data.get("body"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert_eq!(text, "from body");
    }

    #[test]
    fn chunk_limit_matches_hermes_conservative_bound() {
        assert_eq!(MAX_MESSAGE_LENGTH, 4000);
        assert_eq!(DEFAULT_POLL_INTERVAL_MS, 1000);
    }

    #[test]
    fn poll_payload_matches_bridge_contract() {
        let payload = send_poll_payload(
            "123@s.whatsapp.net",
            "Pick one",
            &["A".to_string(), "B".to_string()],
            1,
        );
        assert_eq!(payload["chatId"], "123@s.whatsapp.net");
        assert_eq!(payload["question"], "Pick one");
        assert_eq!(payload["options"], json!(["A", "B"]));
        assert_eq!(payload["selectableCount"], 1);
    }

    #[test]
    fn location_payload_omits_blank_name_address() {
        let payload = send_location_payload("c@g.us", 31.23, 121.47, "", "");
        assert_eq!(payload["chatId"], "c@g.us");
        assert_eq!(payload["latitude"], 31.23);
        assert_eq!(payload["longitude"], 121.47);
        assert!(payload.get("name").is_none());
        assert!(payload.get("address").is_none());
        let full = send_location_payload("c@g.us", 1.0, 2.0, "Home", "Some street");
        assert_eq!(full["name"], "Home");
        assert_eq!(full["address"], "Some street");
    }

    #[test]
    fn edit_payload_matches_bridge_contract() {
        let payload = edit_message_payload("c@g.us", "MSGID1", "fixed text");
        assert_eq!(payload["chatId"], "c@g.us");
        assert_eq!(payload["messageId"], "MSGID1");
        assert_eq!(payload["message"], "fixed text");
    }

    #[test]
    fn clarify_poll_eligibility_matches_hermes_bounds() {
        // hermes send_clarify: 2 <= len(choices) <= 12 ride the native
        // poll; anything else falls back to numbered text.
        let clean = clarify_clean_choices(&[
            " A ".to_string(),
            String::new(),
            "   ".to_string(),
            "B".to_string(),
        ]);
        assert_eq!(clean, vec!["A".to_string(), "B".to_string()]);
        assert!(!clarify_poll_eligible(1));
        assert!(clarify_poll_eligible(2));
        assert!(clarify_poll_eligible(12));
        assert!(!clarify_poll_eligible(13));
    }
}

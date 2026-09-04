//! Mattermost platform adapter — port of hermes
//! `plugins/platforms/mattermost` @ v2026.8.3 (adapter.py).
//!
//! Connects to a self-hosted (or cloud) Mattermost instance via the REST
//! API v4 + WebSocket event stream. No external Mattermost client: REST
//! goes through reqwest with a bearer token, the WS session authenticates
//! with an `authentication_challenge` action and listens for `posted`
//! events.
//!
//! Intake gating mirrors hermes: DMs always pass, channels are gated by
//! `allowed_channels` (whitelist), `require_mention` (@bot username/id)
//! and `free_response_channels`; mentions are stripped from the prompt.
//! Replies chunk at 4000 chars with `disable_mentions` props so the bot
//! never pings anyone; `reply_mode = "thread"` nests replies under the
//! root post. Inbound file attachments download through the authenticated
//! files API into the media cache; outbound `MEDIA:<path>` tags upload
//! via the multipart files endpoint and attach `file_ids` to the post.
//!
//! Known differences: message editing (`edit_message`) and the
//! interactive `hermes gateway setup` wizard are not ported; typing
//! indicators use the REST `users/<id>/typing` endpoint on a best-effort
//! basis.

use crate::messaging::{Dispatcher, MediaAttachment, MessageEvent};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

/// hermes `MAX_POST_LENGTH`.
const MAX_POST_LENGTH: usize = 4000;
const API_TIMEOUT: Duration = Duration::from_secs(30);
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(60);
const WS_PING_INTERVAL: Duration = Duration::from_secs(30);

/// hermes reconnect parameters (exponential backoff + jitter).
const RECONNECT_BASE_DELAY: u64 = 2;
const RECONNECT_MAX_DELAY: u64 = 60;

const DEDUP_WINDOW_SECS: u64 = 300;
const DEDUP_MAX_SIZE: usize = 1000;

/// `[messaging.mattermost]` — Mattermost adapter (hermes
/// `platforms.mattermost` plugin config + `MATTERMOST_*` env vars).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MattermostConfig {
    pub enabled: bool,
    /// Server URL, e.g. `https://mm.example.com` (fallback
    /// `MATTERMOST_URL`).
    pub url: String,
    /// Bot token or personal-access token (fallback `MATTERMOST_TOKEN`).
    pub token: String,
    /// User ids allowed to talk to the bot in DMs (hermes pairing /
    /// allowlist gate). Empty = refuse all and log the ids that try.
    pub allowed_users: Vec<String>,
    /// When non-empty, the bot ONLY responds in these channels (DMs
    /// bypass; hermes `allowed_channels`).
    pub allowed_channels: Vec<String>,
    /// Require @mention in non-DM channels (hermes default true).
    pub require_mention: bool,
    /// Channel ids exempt from the mention requirement.
    pub free_response_channels: Vec<String>,
    /// `thread` nests replies under the triggering post's root; `off`
    /// posts flat (hermes `reply_mode`).
    pub reply_mode: String,
    /// Channel for cron/notification delivery (hermes
    /// `MATTERMOST_HOME_CHANNEL`).
    pub home_channel: String,
}

impl Default for MattermostConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            url: String::new(),
            token: String::new(),
            allowed_users: Vec::new(),
            allowed_channels: Vec::new(),
            require_mention: true,
            free_response_channels: Vec::new(),
            reply_mode: "off".into(),
            home_channel: String::new(),
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

fn env_bool_default_true(name: &str) -> Option<bool> {
    env_trim(name).map(|v| !matches!(v.to_lowercase().as_str(), "false" | "0" | "no"))
}

/// Resolved runtime settings (env > config, hermes precedence).
#[derive(Debug, Clone)]
pub struct ResolvedMattermost {
    pub url: String,
    pub token: String,
    pub allowed_users: Vec<String>,
    pub allowed_channels: Vec<String>,
    pub require_mention: bool,
    pub free_response_channels: Vec<String>,
    pub reply_mode: String,
    pub home_channel: String,
}

impl MattermostConfig {
    pub fn resolve(&self) -> ResolvedMattermost {
        ResolvedMattermost {
            url: env_trim("MATTERMOST_URL")
                .unwrap_or_else(|| self.url.trim().to_string())
                .trim_end_matches('/')
                .to_string(),
            token: env_trim("MATTERMOST_TOKEN").unwrap_or_else(|| self.token.trim().to_string()),
            allowed_users: env_list("MATTERMOST_ALLOWED_USERS")
                .unwrap_or_else(|| self.allowed_users.clone()),
            allowed_channels: env_list("MATTERMOST_ALLOWED_CHANNELS")
                .unwrap_or_else(|| self.allowed_channels.clone()),
            require_mention: env_bool_default_true("MATTERMOST_REQUIRE_MENTION")
                .unwrap_or(self.require_mention),
            free_response_channels: env_list("MATTERMOST_FREE_RESPONSE_CHANNELS")
                .unwrap_or_else(|| self.free_response_channels.clone()),
            reply_mode: env_trim("MATTERMOST_REPLY_MODE")
                .unwrap_or_else(|| self.reply_mode.clone())
                .to_lowercase(),
            home_channel: env_trim("MATTERMOST_HOME_CHANNEL")
                .unwrap_or_else(|| self.home_channel.clone()),
        }
    }
}

/// hermes `_CHANNEL_TYPE_MAP`.
fn channel_type_name(code: &str) -> &'static str {
    match code {
        "D" => "dm",
        "G" => "group",
        "P" => "group",
        _ => "channel",
    }
}

struct Runtime {
    cfg: ResolvedMattermost,
    client: reqwest::Client,
    bot_user_id: Mutex<String>,
    bot_username: Mutex<String>,
    dedup: Mutex<HashMap<String, u64>>,
}

impl Runtime {
    fn api_url(&self, path: &str) -> String {
        format!("{}/api/v4/{}", self.cfg.url, path.trim_start_matches('/'))
    }

    fn auth_header(&self) -> (String, String) {
        ("Authorization".into(), format!("Bearer {}", self.cfg.token))
    }

    async fn api_get(&self, path: &str) -> Result<Value, String> {
        if path.contains("..") {
            return Err("path traversal blocked".into());
        }
        let resp = self
            .client
            .get(self.api_url(path))
            .header(self.auth_header().0, self.auth_header().1)
            .timeout(API_TIMEOUT)
            .send()
            .await
            .map_err(|e| format!("GET {path}: {e}"))?;
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        if status >= 400 {
            return Err(format!("GET {path} → {status}: {}", truncate(&body, 200)));
        }
        serde_json::from_str(&body).map_err(|e| format!("GET {path}: bad JSON: {e}"))
    }

    async fn api_post(&self, path: &str, payload: &Value) -> Result<Value, String> {
        if path.contains("..") {
            return Err("path traversal blocked".into());
        }
        let resp = self
            .client
            .post(self.api_url(path))
            .header(self.auth_header().0, self.auth_header().1)
            .json(payload)
            .timeout(API_TIMEOUT)
            .send()
            .await
            .map_err(|e| format!("POST {path}: {e}"))?;
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        if status >= 400 {
            return Err(format!("POST {path} → {status}: {}", truncate(&body, 200)));
        }
        serde_json::from_str(&body).map_err(|e| format!("POST {path}: bad JSON: {e}"))
    }

    /// hermes `_upload_file`: multipart upload, returns the file id.
    async fn upload_file(
        &self,
        channel_id: &str,
        data: Vec<u8>,
        filename: &str,
        mime: &str,
    ) -> Result<String, String> {
        let part = reqwest::multipart::Part::bytes(data)
            .file_name(filename.to_string())
            .mime_str(mime)
            .map_err(|e| e.to_string())?;
        let form = reqwest::multipart::Form::new()
            .text("channel_id", channel_id.to_string())
            .part("files", part);
        let resp = self
            .client
            .post(self.api_url("files"))
            .header(self.auth_header().0, self.auth_header().1)
            .multipart(form)
            .timeout(UPLOAD_TIMEOUT)
            .send()
            .await
            .map_err(|e| format!("file upload: {e}"))?;
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        if status >= 400 {
            return Err(format!("file upload → {status}: {}", truncate(&body, 200)));
        }
        let value: Value =
            serde_json::from_str(&body).map_err(|e| format!("upload bad JSON: {e}"))?;
        value
            .pointer("/file_infos/0/id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| "upload returned no file_infos".into())
    }

    /// hermes `send()`: chunked post with `disable_mentions` props and
    /// optional thread root.
    async fn send_post(
        &self,
        channel_id: &str,
        content: &str,
        root_id: Option<&str>,
    ) -> Result<(), String> {
        let chunks = crate::messaging::chunk_text(content, MAX_POST_LENGTH);
        for chunk in chunks {
            let mut payload = json!({
                "channel_id": channel_id,
                "message": chunk,
                "props": {"disable_mentions": true},
            });
            if let Some(root) = root_id {
                if !root.is_empty() {
                    payload["root_id"] = json!(root);
                }
            }
            self.api_post("posts", &payload).await?;
        }
        Ok(())
    }

    /// Outbound media: upload then post with `file_ids` (+ optional
    /// caption).
    async fn send_media(&self, channel_id: &str, path: &std::path::Path, caption: &str) {
        let data = match std::fs::read(path) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("[mattermost] media read {} failed: {e}", path.display());
                return;
            }
        };
        let mime = crate::media_cache::mime_for_ext(path);
        let filename = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "attachment".into());
        match self.upload_file(channel_id, data, &filename, &mime).await {
            Ok(file_id) => {
                let payload = json!({
                    "channel_id": channel_id,
                    "message": caption,
                    "file_ids": [file_id],
                    "props": {"disable_mentions": true},
                });
                if let Err(e) = self.api_post("posts", &payload).await {
                    eprintln!("[mattermost] media post failed: {e}");
                }
            }
            Err(e) => eprintln!("[mattermost] media upload failed: {e}"),
        }
    }

    async fn send_typing(&self, channel_id: &str) {
        let bot = self.bot_user_id.lock().await.clone();
        if bot.is_empty() {
            return;
        }
        let _ = self
            .api_post(
                &format!("users/{bot}/typing"),
                &json!({"channel_id": channel_id}),
            )
            .await;
    }

    /// Download one inbound file attachment into the media cache.
    async fn download_file(&self, file_id: &str) -> Option<MediaAttachment> {
        let info = self.api_get(&format!("files/{file_id}/info")).await.ok()?;
        let fname = info
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("file")
            .to_string();
        let mime = info
            .get("mime_type")
            .and_then(|v| v.as_str())
            .unwrap_or("application/octet-stream")
            .to_string();
        let resp = self
            .client
            .get(self.api_url(&format!("files/{file_id}")))
            .header(self.auth_header().0, self.auth_header().1)
            .timeout(API_TIMEOUT)
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            eprintln!(
                "[mattermost] file download {file_id} failed: HTTP {}",
                resp.status()
            );
            return None;
        }
        let bytes = resp.bytes().await.ok()?.to_vec();
        let path = crate::media_cache::cache_media_bytes(
            &crate::config::ulnclaw_home(),
            &bytes,
            &mime,
            &fname,
        )
        .ok()?;
        Some(MediaAttachment {
            path,
            mime,
            bytes: bytes.len() as u64,
            original_name: fname,
        })
    }

    /// hermes `MessageDeduplicator`: bounded post-id window.
    async fn is_duplicate(&self, post_id: &str) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut dedup = self.dedup.lock().await;
        dedup.retain(|_, ts| now.saturating_sub(*ts) < DEDUP_WINDOW_SECS);
        if dedup.contains_key(post_id) {
            return true;
        }
        if dedup.len() >= DEDUP_MAX_SIZE {
            // Drop the oldest half when the cap is hit.
            let mut entries: Vec<(String, u64)> = dedup.drain().collect();
            entries.sort_by_key(|(_, ts)| *ts);
            entries.truncate(entries.len() / 2);
            *dedup = entries.into_iter().collect();
        }
        dedup.insert(post_id.to_string(), now);
        false
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

/// Entry point spawned by `run_messaging`.
pub async fn run(
    cfg: MattermostConfig,
    dispatcher: Arc<Dispatcher>,
    pairing: Option<Arc<crate::pairing::PairingStore>>,
) {
    let resolved = cfg.resolve();
    if resolved.url.is_empty() || resolved.token.is_empty() {
        eprintln!(
            "[mattermost] disabled: url/token not configured (set [messaging.mattermost] or MATTERMOST_URL/MATTERMOST_TOKEN)"
        );
        return;
    }
    let runtime = Arc::new(Runtime {
        cfg: resolved,
        client: reqwest::Client::new(),
        bot_user_id: Mutex::new(String::new()),
        bot_username: Mutex::new(String::new()),
        dedup: Mutex::new(HashMap::new()),
    });
    crate::messaging::register_platform_sender(
        "mattermost",
        Arc::new(MattermostSender {
            runtime: runtime.clone(),
        }),
    );

    let mut delay = RECONNECT_BASE_DELAY;
    loop {
        match run_session(&runtime, &dispatcher, &pairing).await {
            Ok(()) => delay = RECONNECT_BASE_DELAY,
            Err(SessionError::Permanent(msg)) => {
                eprintln!("[mattermost] permanent error: {msg} — stopping reconnect");
                return;
            }
            Err(SessionError::Transient(msg)) => {
                eprintln!("[mattermost] session error: {msg} — reconnecting in {delay}s");
            }
        }
        // hermes jitter: delay + up to 20% extra.
        let jitter = (delay as f64 * 0.2 * rand_fraction()) as u64;
        tokio::time::sleep(Duration::from_secs(delay + jitter)).await;
        delay = (delay * 2).min(RECONNECT_MAX_DELAY);
    }
}

/// Cheap deterministic-ish jitter source (no rand crate dependency).
fn rand_fraction() -> f64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    (nanos % 1000) as f64 / 1000.0
}

enum SessionError {
    Transient(String),
    Permanent(String),
}

async fn run_session(
    runtime: &Arc<Runtime>,
    dispatcher: &Arc<Dispatcher>,
    pairing: &Option<Arc<crate::pairing::PairingStore>>,
) -> Result<(), SessionError> {
    use futures::{SinkExt, StreamExt};

    // Verify credentials + capture bot identity (hermes `connect()`).
    let me = runtime
        .api_get("users/me")
        .await
        .map_err(|e| SessionError::Transient(format!("auth check failed: {e}")))?;
    let Some(bot_id) = me.get("id").and_then(|v| v.as_str()) else {
        return Err(SessionError::Permanent(
            "users/me returned no id — check MATTERMOST_TOKEN/MATTERMOST_URL".into(),
        ));
    };
    *runtime.bot_user_id.lock().await = bot_id.to_string();
    *runtime.bot_username.lock().await = me
        .get("username")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    eprintln!(
        "[mattermost] authenticated as @{} on {}",
        runtime.bot_username.lock().await,
        runtime.cfg.url
    );

    let ws_url = ws_url_from(&runtime.cfg.url);
    let (ws, _) = tokio_tungstenite::connect_async(&ws_url)
        .await
        .map_err(|e| {
            let msg = e.to_string();
            if msg.contains("401")
                || msg.contains("403")
                || msg.to_lowercase().contains("unauthorized")
            {
                SessionError::Permanent(format!("WS auth failed: {msg}"))
            } else {
                SessionError::Transient(format!("WS connect: {msg}"))
            }
        })?;
    eprintln!("[mattermost] websocket connected");

    let (mut sink, mut stream) = ws.split();
    let auth = json!({
        "seq": 1,
        "action": "authentication_challenge",
        "data": {"token": runtime.cfg.token},
    });
    sink.send(tokio_tungstenite::tungstenite::Message::Text(
        auth.to_string(),
    ))
    .await
    .map_err(|e| SessionError::Transient(format!("WS auth send: {e}")))?;

    let mut ping_due = tokio::time::Instant::now() + WS_PING_INTERVAL;

    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(ping_due) => {
                let _ = sink
                    .send(tokio_tungstenite::tungstenite::Message::Ping(Vec::new()))
                    .await;
                ping_due = tokio::time::Instant::now() + WS_PING_INTERVAL;
            }
            message = stream.next() => {
                let Some(message) = message else {
                    return Err(SessionError::Transient("websocket closed".into()));
                };
                let message = match message {
                    Ok(m) => m,
                    Err(e) => return Err(SessionError::Transient(format!("websocket: {e}"))),
                };
                let text = match message {
                    tokio_tungstenite::tungstenite::Message::Text(t) => t,
                    tokio_tungstenite::tungstenite::Message::Binary(b) => {
                        match String::from_utf8(b) {
                            Ok(s) => s,
                            Err(_) => continue,
                        }
                    }
                    tokio_tungstenite::tungstenite::Message::Close(_) => {
                        return Err(SessionError::Transient("websocket closed by server".into()));
                    }
                    _ => continue,
                };
                let Ok(event) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                handle_ws_event(runtime, dispatcher, pairing, &event).await;
            }
        }
    }
}

/// `https://` → `wss://`, `http://` → `ws://` + `/api/v4/websocket`.
pub fn ws_url_from(base_url: &str) -> String {
    let ws_base = if let Some(rest) = base_url.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base_url.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        base_url.to_string()
    };
    format!("{ws_base}/api/v4/websocket")
}

/// hermes `_handle_ws_event` — only `posted` events produce messages.
async fn handle_ws_event(
    runtime: &Arc<Runtime>,
    dispatcher: &Arc<Dispatcher>,
    pairing: &Option<Arc<crate::pairing::PairingStore>>,
    event: &Value,
) {
    if event.get("event").and_then(|v| v.as_str()) != Some("posted") {
        return;
    }
    let data = event.get("data").cloned().unwrap_or(json!({}));
    let Some(post_str) = data.get("post").and_then(|v| v.as_str()) else {
        return;
    };
    let Ok(post) = serde_json::from_str::<Value>(post_str) else {
        return;
    };
    let bot_id = runtime.bot_user_id.lock().await.clone();
    if post.get("user_id").and_then(|v| v.as_str()) == Some(bot_id.as_str()) {
        return;
    }
    // System posts carry a non-empty `type`.
    if post
        .get("type")
        .and_then(|v| v.as_str())
        .map(|t| !t.is_empty())
        .unwrap_or(false)
    {
        return;
    }
    let post_id = post
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if post_id.is_empty() || runtime.is_duplicate(&post_id).await {
        return;
    }

    let channel_id = post
        .get("channel_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let channel_type_raw = data
        .get("channel_type")
        .and_then(|v| v.as_str())
        .unwrap_or("O");
    let chat_type = channel_type_name(channel_type_raw);
    let mut message_text = post
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Mention gating for non-DM channels.
    if channel_type_raw != "D" {
        if !runtime.cfg.allowed_channels.is_empty()
            && !runtime.cfg.allowed_channels.contains(&channel_id)
        {
            return;
        }
        let bot_username = runtime.bot_username.lock().await.clone();
        let patterns: Vec<String> = vec![format!("@{bot_username}"), format!("@{bot_id}")];
        let lower = message_text.to_lowercase();
        let has_mention = patterns.iter().any(|p| lower.contains(&p.to_lowercase()));
        let is_free = runtime.cfg.free_response_channels.contains(&channel_id);
        if runtime.cfg.require_mention && !is_free && !has_mention {
            return;
        }
        if has_mention {
            for pattern in &patterns {
                message_text = strip_pattern_ci(&message_text, pattern);
            }
            message_text = message_text.trim().to_string();
        }
    }

    let sender_id = post
        .get("user_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let sender_name = data
        .get("sender_name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim_start_matches('@')
        .to_string();
    let sender_name = if sender_name.is_empty() {
        sender_id.clone()
    } else {
        sender_name
    };

    // Thread routing: real thread root wins; in `thread` reply mode a
    // top-level channel post becomes its own root.
    let mut thread_id = post
        .get("root_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    if thread_id.is_none() && runtime.cfg.reply_mode == "thread" && channel_type_raw != "D" {
        thread_id = Some(post_id.clone());
    }

    // DM intake gate (allowlist ∪ pairing, hermes authorization).
    if chat_type == "dm"
        && !user_gate_allows(runtime, pairing, &channel_id, &sender_id, &sender_name).await
    {
        return;
    }

    // Download attachments (auth-gated URLs cannot be handed downstream).
    let mut attachments: Vec<MediaAttachment> = Vec::new();
    if let Some(file_ids) = post.get("file_ids").and_then(|v| v.as_array()) {
        for fid in file_ids {
            let Some(fid) = fid.as_str() else { continue };
            if let Some(att) = runtime.download_file(fid).await {
                attachments.push(att);
            }
        }
    }

    let mut event = MessageEvent {
        platform: "mattermost".into(),
        chat_id: channel_id.clone(),
        sender_id: sender_id.clone(),
        sender_name,
        text: message_text,
        message_id: post_id,
        attachments,
    };
    if !crate::messaging::pre_gateway_dispatch_gate_public(&mut event).await {
        return;
    }
    runtime.send_typing(&channel_id).await;
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
        runtime.send_media(&channel_id, path, "").await;
    }
    if !reply_text.trim().is_empty() {
        // P704: ledger-protected reply delivery.
        dispatcher
            .try_send_with_ledger("mattermost", &channel_id, &reply_text, || async {
                match runtime
                    .send_post(&channel_id, &reply_text, thread_id.as_deref())
                    .await
                {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        eprintln!("[mattermost] reply failed: {e}");
                        Err(e.to_string())
                    }
                }
            })
            .await;
    }
}

/// Allowlist∪pairing gate for DM senders (channel access is governed by
/// `allowed_channels`/mention rules instead).
async fn user_gate_allows(
    runtime: &Arc<Runtime>,
    pairing: &Option<Arc<crate::pairing::PairingStore>>,
    channel_id: &str,
    sender_id: &str,
    sender_name: &str,
) -> bool {
    if runtime
        .cfg
        .allowed_users
        .iter()
        .any(|u| u == sender_id || u == "*")
    {
        return true;
    }
    if let Some(store) = pairing {
        if store.is_approved("mattermost", sender_id) {
            return true;
        }
        if let Some(code_msg) = crate::messaging::pairing_offer_public(
            store.as_ref(),
            "mattermost",
            sender_id,
            sender_name,
        ) {
            let _ = runtime.send_post(channel_id, &code_msg, None).await;
        } else {
            eprintln!(
                "[mattermost] unauthorized DM from {sender_id} — add to allowed_users or approve pairing"
            );
        }
        return false;
    }
    eprintln!("[mattermost] unauthorized DM from {sender_id} — add to allowed_users");
    false
}

/// Case-insensitive literal-pattern strip (hermes `re.sub(re.escape(p))`).
pub fn strip_pattern_ci(text: &str, pattern: &str) -> String {
    let lower_text = text.to_lowercase();
    let lower_pattern = pattern.to_lowercase();
    let mut out = String::new();
    let mut i = 0;
    while let Some(rel) = lower_text[i..].find(&lower_pattern) {
        out.push_str(&text[i..i + rel]);
        i += rel + pattern.len();
    }
    out.push_str(&text[i..]);
    out
}

struct MattermostSender {
    runtime: Arc<Runtime>,
}

#[async_trait::async_trait]
impl crate::messaging::PlatformSender for MattermostSender {
    async fn send_text(&self, chat_id: &str, text: &str) {
        if let Err(e) = self.runtime.send_post(chat_id, text, None).await {
            eprintln!("[mattermost] send_text to {chat_id} failed: {e}");
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
    fn channel_type_mapping() {
        assert_eq!(channel_type_name("D"), "dm");
        assert_eq!(channel_type_name("G"), "group");
        assert_eq!(channel_type_name("P"), "group");
        assert_eq!(channel_type_name("O"), "channel");
        assert_eq!(channel_type_name("?"), "channel");
    }

    #[test]
    fn ws_url_scheme_mapping() {
        assert_eq!(
            ws_url_from("https://mm.example.com"),
            "wss://mm.example.com/api/v4/websocket"
        );
        assert_eq!(
            ws_url_from("http://localhost:8065/"),
            "ws://localhost:8065//api/v4/websocket"
        );
    }

    #[test]
    fn mention_strip_case_insensitive() {
        let text = "Hey @MyBot do the thing @mybot!";
        let stripped = strip_pattern_ci(text, "@mybot");
        // hermes re.sub leaves interior double spaces; only ends trim.
        assert_eq!(stripped.trim(), "Hey  do the thing !");
    }

    #[test]
    fn config_env_precedence() {
        let cfg = MattermostConfig {
            url: "https://cfg.example.com/".into(),
            require_mention: true,
            ..Default::default()
        };
        // No env vars set in the test process for MATTERMOST_* — config
        // values survive, trailing slash trimmed.
        let resolved = cfg.resolve();
        assert_eq!(resolved.url, "https://cfg.example.com");
        assert!(resolved.require_mention);
        assert_eq!(resolved.reply_mode, "off");
    }

    #[test]
    fn chunking_respects_post_limit() {
        let long = "x".repeat(MAX_POST_LENGTH * 2 + 10);
        let chunks = crate::messaging::chunk_text(&long, MAX_POST_LENGTH);
        assert!(chunks.len() >= 3);
        for chunk in &chunks {
            assert!(chunk.chars().count() <= MAX_POST_LENGTH);
        }
    }

    #[test]
    fn truncate_helper() {
        assert_eq!(truncate("hello world", 5), "hello");
        assert_eq!(truncate("hi", 5), "hi");
    }

    #[tokio::test]
    async fn dedup_window_behavior() {
        let runtime = Runtime {
            cfg: ResolvedMattermost {
                url: String::new(),
                token: String::new(),
                allowed_users: Vec::new(),
                allowed_channels: Vec::new(),
                require_mention: true,
                free_response_channels: Vec::new(),
                reply_mode: "off".into(),
                home_channel: String::new(),
            },
            client: reqwest::Client::new(),
            bot_user_id: Mutex::new(String::new()),
            bot_username: Mutex::new(String::new()),
            dedup: Mutex::new(HashMap::new()),
        };
        assert!(!runtime.is_duplicate("p1").await);
        assert!(runtime.is_duplicate("p1").await);
        assert!(!runtime.is_duplicate("p2").await);
    }
}

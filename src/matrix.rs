//! Matrix platform adapter — port of hermes `plugins/platforms/matrix`
//! @ v2026.8.3 (adapter.py core behaviors).
//!
//! Talks directly to the Matrix Client-Server API (no SDK): password
//! login (`/_matrix/client/v3/login`) or a supplied access token, a
//! long-polling `/_matrix/client/v3/sync` loop for inbound
//! `m.room.message` events, transactional `PUT .../send/m.room.message`
//! for replies, and the media API (`/_matrix/media/v3/upload` +
//! `download`) for attachments.
//!
//! Intake parity: `allowed_users` / `allowed_rooms` gates,
//! `require_mention` in group rooms (display-name + @user-id patterns,
//! stripped from the prompt), `free_response_rooms` exemptions,
//! `process_notices` for `m.notice`, reply-fallback stripping,
//! appservice/ignore patterns, and the 16000-char outbound chunk size
//! (`MATRIX_MAX_MESSAGE_LENGTH`, clamped 500..65535).
//!
//! Known differences: end-to-end encryption is NOT supported (hermes
//! rides mautrix+libolm; encrypted rooms are detected via
//! `m.room.encrypted` events and skipped with a warning), thread
//! bookkeeping/session-scoping policies, reactions lifecycle, presence,
//! read receipts, and the interactive approval/model picker UX are not
//! ported; display names resolve to the sender's MXID localpart.

use crate::messaging::{Dispatcher, MediaAttachment, MessageEvent};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

const DEFAULT_MAX_MESSAGE_LENGTH: usize = 16_000;
const MAX_MESSAGE_LENGTH_CEILING: usize = 65_535;
const SYNC_TIMEOUT_MS: u64 = 30_000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// `[messaging.matrix]` — Matrix CS-API adapter (hermes
/// `platforms.matrix` plugin config + `MATRIX_*` env vars).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MatrixConfig {
    pub enabled: bool,
    /// Homeserver URL, e.g. `https://matrix.example.org` (fallback
    /// `MATRIX_HOMESERVER`).
    pub homeserver: String,
    /// Access token — preferred auth (fallback `MATRIX_ACCESS_TOKEN`).
    pub access_token: String,
    /// Full user id `@bot:server` for password login (fallback
    /// `MATRIX_USER_ID`).
    pub user_id: String,
    /// Password login alternative (fallback `MATRIX_PASSWORD`).
    pub password: String,
    /// Stable device id persisted across restarts (fallback
    /// `MATRIX_DEVICE_ID`).
    pub device_id: String,
    /// Matrix user ids allowed to trigger turns (`*` = anyone).
    pub allowed_users: Vec<String>,
    /// Room ids allowed to trigger turns (empty = all rooms).
    pub allowed_rooms: Vec<String>,
    /// Accept any sender (hermes `MATRIX_ALLOW_ALL_USERS`).
    pub allow_all_users: bool,
    /// Require @mention in group rooms (hermes default true).
    pub require_mention: bool,
    /// Room ids exempt from the mention requirement.
    pub free_response_rooms: Vec<String>,
    /// Also process inbound `m.notice` events (hermes default false).
    pub process_notices: bool,
    /// Room for cron/notification delivery (hermes `MATRIX_HOME_ROOM`).
    pub home_room: String,
    /// Outbound chunk size in chars (hermes default 16000, clamped
    /// 500..65535).
    pub max_message_length: usize,
    /// Regex patterns for senders to ignore (appservices/bridges).
    pub ignore_user_patterns: Vec<String>,
}

impl Default for MatrixConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            homeserver: String::new(),
            access_token: String::new(),
            user_id: String::new(),
            password: String::new(),
            device_id: String::new(),
            allowed_users: Vec::new(),
            allowed_rooms: Vec::new(),
            allow_all_users: false,
            require_mention: true,
            free_response_rooms: Vec::new(),
            process_notices: false,
            home_room: String::new(),
            max_message_length: DEFAULT_MAX_MESSAGE_LENGTH,
            ignore_user_patterns: Vec::new(),
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

fn env_bool(name: &str) -> Option<bool> {
    env_trim(name).map(|v| matches!(v.to_lowercase().as_str(), "true" | "1" | "yes"))
}

fn env_bool_default_true(name: &str) -> Option<bool> {
    env_trim(name).map(|v| !matches!(v.to_lowercase().as_str(), "false" | "0" | "no"))
}

/// hermes `resolve_max_message_length` clamp.
pub fn clamp_max_message_length(raw: usize) -> usize {
    if raw == 0 {
        return DEFAULT_MAX_MESSAGE_LENGTH;
    }
    raw.clamp(500, MAX_MESSAGE_LENGTH_CEILING)
}

#[derive(Debug, Clone)]
pub struct ResolvedMatrix {
    pub homeserver: String,
    pub access_token: String,
    pub user_id: String,
    pub password: String,
    pub device_id: String,
    pub allowed_users: Vec<String>,
    pub allowed_rooms: Vec<String>,
    pub allow_all_users: bool,
    pub require_mention: bool,
    pub free_response_rooms: Vec<String>,
    pub process_notices: bool,
    pub home_room: String,
    pub max_message_length: usize,
    pub ignore_user_patterns: Vec<String>,
}

impl MatrixConfig {
    pub fn resolve(&self) -> ResolvedMatrix {
        let max_len = env_trim("MATRIX_MAX_MESSAGE_LENGTH")
            .and_then(|v| v.parse::<usize>().ok())
            .map(clamp_max_message_length)
            .unwrap_or_else(|| clamp_max_message_length(self.max_message_length));
        ResolvedMatrix {
            homeserver: env_trim("MATRIX_HOMESERVER")
                .unwrap_or_else(|| self.homeserver.trim().to_string())
                .trim_end_matches('/')
                .to_string(),
            access_token: env_trim("MATRIX_ACCESS_TOKEN")
                .unwrap_or_else(|| self.access_token.trim().to_string()),
            user_id: env_trim("MATRIX_USER_ID").unwrap_or_else(|| self.user_id.trim().to_string()),
            password: env_trim("MATRIX_PASSWORD").unwrap_or_else(|| self.password.clone()),
            device_id: env_trim("MATRIX_DEVICE_ID")
                .unwrap_or_else(|| self.device_id.trim().to_string()),
            allowed_users: env_list("MATRIX_ALLOWED_USERS")
                .unwrap_or_else(|| self.allowed_users.clone()),
            allowed_rooms: env_list("MATRIX_ALLOWED_ROOMS")
                .unwrap_or_else(|| self.allowed_rooms.clone()),
            allow_all_users: env_bool("MATRIX_ALLOW_ALL_USERS")
                .or_else(|| env_bool("GATEWAY_ALLOW_ALL_USERS"))
                .unwrap_or(self.allow_all_users),
            require_mention: env_bool_default_true("MATRIX_REQUIRE_MENTION")
                .unwrap_or(self.require_mention),
            free_response_rooms: env_list("MATRIX_FREE_RESPONSE_ROOMS")
                .unwrap_or_else(|| self.free_response_rooms.clone()),
            process_notices: env_bool("MATRIX_PROCESS_NOTICES").unwrap_or(self.process_notices),
            home_room: env_trim("MATRIX_HOME_ROOM").unwrap_or_else(|| self.home_room.clone()),
            max_message_length: max_len,
            ignore_user_patterns: env_list("MATRIX_IGNORE_USER_PATTERNS")
                .unwrap_or_else(|| self.ignore_user_patterns.clone()),
        }
    }
}

struct Runtime {
    cfg: ResolvedMatrix,
    client: reqwest::Client,
    /// Set after login / from config.
    user_id: Mutex<String>,
    access_token: Mutex<String>,
    /// Own display name for mention detection (profile API, localpart
    /// fallback).
    display_name: Mutex<String>,
    /// Rooms seen with encrypted events — warn once each.
    encrypted_warned: Mutex<HashSet<String>>,
}

impl Runtime {
    fn client_url(&self, path: &str) -> String {
        format!(
            "{}/_matrix/client/v3/{}",
            self.cfg.homeserver,
            path.trim_start_matches('/')
        )
    }

    fn media_url(&self, path: &str) -> String {
        format!(
            "{}/_matrix/media/v3/{}",
            self.cfg.homeserver,
            path.trim_start_matches('/')
        )
    }

    async fn authed(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let token = self.access_token.lock().await.clone();
        req.bearer_auth(token).timeout(REQUEST_TIMEOUT)
    }

    async fn get_json(&self, url: &str) -> Result<Value, String> {
        let resp = self
            .authed(self.client.get(url))
            .await
            .send()
            .await
            .map_err(|e| format!("GET {url}: {e}"))?;
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        if status >= 400 {
            return Err(format!("GET {url} → {status}: {body}"));
        }
        serde_json::from_str(&body).map_err(|e| format!("GET {url}: bad JSON: {e}"))
    }

    async fn put_json(&self, url: &str, payload: &Value) -> Result<Value, String> {
        let resp = self
            .authed(self.client.put(url))
            .await
            .json(payload)
            .send()
            .await
            .map_err(|e| format!("PUT {url}: {e}"))?;
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        if status >= 400 {
            return Err(format!("PUT {url} → {status}: {body}"));
        }
        serde_json::from_str(&body).map_err(|e| format!("PUT {url}: bad JSON: {e}"))
    }

    /// Password login when no access token is configured (hermes
    /// mautrix login flow).
    async fn login_if_needed(&self) -> Result<(), String> {
        if !self.access_token.lock().await.is_empty() {
            return Ok(());
        }
        let user = self.cfg.user_id.clone();
        let password = self.cfg.password.clone();
        if user.is_empty() || password.is_empty() {
            return Err("no MATRIX_ACCESS_TOKEN and no MATRIX_USER_ID/MATRIX_PASSWORD pair".into());
        }
        let mut payload = json!({
            "type": "m.login.password",
            "identifier": {"type": "m.id.user", "user": user},
            "password": password,
        });
        if !self.cfg.device_id.is_empty() {
            payload["device_id"] = json!(self.cfg.device_id);
        }
        let resp = self
            .client
            .post(self.client_url("login"))
            .json(&payload)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|e| format!("login: {e}"))?;
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        if status >= 400 {
            return Err(format!("login → {status}: {body}"));
        }
        let value: Value = serde_json::from_str(&body).map_err(|e| format!("login JSON: {e}"))?;
        let token = value
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "login returned no access_token".to_string())?;
        *self.access_token.lock().await = token.to_string();
        if let Some(uid) = value.get("user_id").and_then(|v| v.as_str()) {
            *self.user_id.lock().await = uid.to_string();
        }
        Ok(())
    }

    async fn fetch_display_name(&self) {
        let user = self.user_id.lock().await.clone();
        if user.is_empty() {
            return;
        }
        let url = self.client_url(&format!("profile/{}", urlencoding(&user)));
        if let Ok(profile) = self.get_json(&url).await {
            if let Some(name) = profile.get("displayname").and_then(|v| v.as_str()) {
                if !name.trim().is_empty() {
                    *self.display_name.lock().await = name.to_string();
                    return;
                }
            }
        }
        *self.display_name.lock().await = localpart(&user);
    }

    /// hermes chunked `send()`: plain-text `m.text` events via
    /// transactional PUT.
    async fn send_text(&self, room_id: &str, content: &str) -> Result<(), String> {
        let max_len = self.cfg.max_message_length;
        for chunk in crate::messaging::chunk_text(content, max_len) {
            let txn = format!("ulnclaw-{}", uuid::Uuid::new_v4().simple());
            let url = self.client_url(&format!(
                "rooms/{}/send/m.room.message/{}",
                urlencoding(room_id),
                txn
            ));
            let payload = json!({"msgtype": "m.text", "body": chunk});
            self.put_json(&url, &payload).await?;
        }
        Ok(())
    }

    /// Upload bytes → `mxc://` URI (media v3 upload).
    async fn upload_media(
        &self,
        data: Vec<u8>,
        filename: &str,
        mime: &str,
    ) -> Result<String, String> {
        let url = format!(
            "{}?filename={}",
            self.media_url("upload"),
            urlencoding(filename)
        );
        let resp = self
            .authed(self.client.post(&url))
            .await
            .header("Content-Type", mime)
            .body(data)
            .send()
            .await
            .map_err(|e| format!("media upload: {e}"))?;
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        if status >= 400 {
            return Err(format!("media upload → {status}: {body}"));
        }
        let value: Value = serde_json::from_str(&body).map_err(|e| format!("upload JSON: {e}"))?;
        value
            .get("content_uri")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| "upload returned no content_uri".into())
    }

    /// Outbound `MEDIA:<path>`: upload then send an m.image/m.audio/
    /// m.video/m.file event.
    async fn send_media(&self, room_id: &str, path: &std::path::Path) {
        let data = match std::fs::read(path) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("[matrix] media read {} failed: {e}", path.display());
                return;
            }
        };
        let mime = crate::media_cache::mime_for_ext(path);
        let filename = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "attachment".into());
        match self.upload_media(data.clone(), &filename, &mime).await {
            Ok(content_uri) => {
                let msgtype = match crate::media_cache::media_kind(&mime) {
                    "image" => "m.image",
                    "audio" => "m.audio",
                    "video" => "m.video",
                    _ => "m.file",
                };
                let txn = format!("ulnclaw-{}", uuid::Uuid::new_v4().simple());
                let url = self.client_url(&format!(
                    "rooms/{}/send/m.room.message/{}",
                    urlencoding(room_id),
                    txn
                ));
                let payload = json!({
                    "msgtype": msgtype,
                    "body": filename,
                    "url": content_uri,
                    "info": {"mimetype": mime, "size": data.len()},
                });
                if let Err(e) = self.put_json(&url, &payload).await {
                    eprintln!("[matrix] media send failed: {e}");
                }
            }
            Err(e) => eprintln!("[matrix] media upload failed: {e}"),
        }
    }

    /// Best-effort typing indicator (hermes base-adapter typing
    /// heartbeat).
    async fn send_typing(&self, room_id: &str) {
        let user = self.user_id.lock().await.clone();
        if user.is_empty() {
            return;
        }
        let url = self.client_url(&format!(
            "rooms/{}/typing/{}",
            urlencoding(room_id),
            urlencoding(&user)
        ));
        let _ = self
            .put_json(&url, &json!({"typing": true, "timeout": 10_000}))
            .await;
    }

    /// Download an `mxc://` object into the media cache.
    async fn download_mxc(
        &self,
        mxc: &str,
        mime_hint: &str,
        filename_hint: &str,
    ) -> Option<MediaAttachment> {
        let (server, media_id) = parse_mxc(mxc)?;
        let url = self.media_url(&format!("download/{server}/{media_id}"));
        // Try authenticated first (MSC3916), then unauthenticated for
        // older homeservers.
        let mut resp = match self
            .client
            .get(&url)
            .bearer_auth(self.access_token.lock().await.clone())
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[matrix] media download failed: {e}");
                return None;
            }
        };
        if !resp.status().is_success() {
            resp = self
                .client
                .get(&url)
                .timeout(REQUEST_TIMEOUT)
                .send()
                .await
                .ok()?;
        }
        if !resp.status().is_success() {
            eprintln!("[matrix] media download {mxc}: HTTP {}", resp.status());
            return None;
        }
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                if mime_hint.is_empty() {
                    "application/octet-stream".into()
                } else {
                    mime_hint.to_string()
                }
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
}

/// `mxc://server/media-id` → (server, media-id).
pub fn parse_mxc(mxc: &str) -> Option<(String, String)> {
    let rest = mxc.strip_prefix("mxc://")?;
    let (server, media_id) = rest.split_once('/')?;
    if server.is_empty() || media_id.is_empty() || media_id.contains('/') {
        return None;
    }
    Some((server.to_string(), media_id.to_string()))
}

/// Percent-encode for URL path segments (MXIDs/room ids contain `!@:/`).
fn urlencoding(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
}

fn localpart(mxid: &str) -> String {
    let stripped = mxid.trim_start_matches('@');
    stripped
        .split_once(':')
        .map(|(local, _)| local.to_string())
        .unwrap_or_else(|| stripped.to_string())
}

/// Entry point spawned by `run_messaging`.
pub async fn run(
    cfg: MatrixConfig,
    dispatcher: Arc<Dispatcher>,
    pairing: Option<Arc<crate::pairing::PairingStore>>,
) {
    let resolved = cfg.resolve();
    if resolved.homeserver.is_empty() {
        eprintln!("[matrix] disabled: homeserver not configured (set [messaging.matrix] or MATRIX_HOMESERVER)");
        return;
    }
    if resolved.access_token.is_empty()
        && (resolved.user_id.is_empty() || resolved.password.is_empty())
    {
        eprintln!("[matrix] disabled: set MATRIX_ACCESS_TOKEN or MATRIX_USER_ID+MATRIX_PASSWORD");
        return;
    }
    let runtime = Arc::new(Runtime {
        cfg: resolved,
        client: reqwest::Client::new(),
        user_id: Mutex::new(String::new()),
        access_token: Mutex::new(String::new()),
        display_name: Mutex::new(String::new()),
        encrypted_warned: Mutex::new(HashSet::new()),
    });
    *runtime.user_id.lock().await = runtime.cfg.user_id.clone();
    *runtime.access_token.lock().await = runtime.cfg.access_token.clone();

    crate::messaging::register_platform_sender(
        "matrix",
        Arc::new(MatrixSender {
            runtime: runtime.clone(),
        }),
    );

    let mut backoff: u64 = 2;
    loop {
        match sync_loop(&runtime, &dispatcher, &pairing).await {
            Ok(()) => backoff = 2,
            Err(e) => {
                eprintln!("[matrix] sync loop ended: {e} — reconnecting in {backoff}s");
            }
        }
        tokio::time::sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(60);
    }
}

async fn sync_loop(
    runtime: &Arc<Runtime>,
    dispatcher: &Arc<Dispatcher>,
    pairing: &Option<Arc<crate::pairing::PairingStore>>,
) -> Result<(), String> {
    runtime.login_if_needed().await?;
    if runtime.user_id.lock().await.is_empty() {
        // Token-only auth: discover who we are via /account/whoami.
        let who = runtime
            .get_json(&runtime.client_url("account/whoami"))
            .await?;
        if let Some(uid) = who.get("user_id").and_then(|v| v.as_str()) {
            *runtime.user_id.lock().await = uid.to_string();
        }
    }
    runtime.fetch_display_name().await;
    eprintln!(
        "[matrix] connected as {} on {}",
        runtime.user_id.lock().await,
        runtime.cfg.homeserver
    );

    let mut since: Option<String> = None;
    let mut initial_sync = true;
    loop {
        let mut url = format!("{}?timeout={SYNC_TIMEOUT_MS}", runtime.client_url("sync"));
        if let Some(token) = &since {
            url.push_str(&format!("&since={}", urlencoding(token)));
        }
        let response = runtime.get_json(&url).await?;
        since = response
            .get("next_batch")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        if initial_sync {
            // hermes: the first sync backfills history — do not re-reply
            // to old messages.
            initial_sync = false;
            continue;
        }

        let Some(joined) = response.pointer("/rooms/join").and_then(|v| v.as_object()) else {
            continue;
        };
        for (room_id, room_data) in joined {
            // Detect encrypted rooms once (E2EE unsupported).
            if room_data
                .pointer("/state/events")
                .and_then(|v| v.as_array())
                .map(|events| {
                    events.iter().any(|e| {
                        e.get("type").and_then(|t| t.as_str()) == Some("m.room.encryption")
                    })
                })
                .unwrap_or(false)
            {
                let mut warned = runtime.encrypted_warned.lock().await;
                if !warned.contains(room_id) {
                    warned.insert(room_id.clone());
                    eprintln!(
                        "[matrix] room {room_id} is encrypted — E2EE is not supported, skipping its messages"
                    );
                }
            }
            let Some(events) = room_data
                .pointer("/timeline/events")
                .and_then(|v| v.as_array())
            else {
                continue;
            };
            let is_dm = room_data
                .pointer("/summary/m.heroes")
                .and_then(|v| v.as_array())
                .map(|heroes| heroes.len() == 1)
                .unwrap_or(false);
            for event in events {
                handle_room_event(runtime, dispatcher, pairing, room_id, is_dm, event).await;
            }
        }
    }
}

async fn handle_room_event(
    runtime: &Arc<Runtime>,
    dispatcher: &Arc<Dispatcher>,
    pairing: &Option<Arc<crate::pairing::PairingStore>>,
    room_id: &str,
    is_dm: bool,
    event: &Value,
) {
    let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if event_type == "m.room.encrypted" {
        let mut warned = runtime.encrypted_warned.lock().await;
        if !warned.contains(room_id) {
            warned.insert(room_id.to_string());
            eprintln!(
                "[matrix] room {room_id} delivers encrypted events — E2EE not supported, skipping"
            );
        }
        return;
    }
    if event_type != "m.room.message" {
        return;
    }
    let sender = event
        .get("sender")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let self_id = runtime.user_id.lock().await.clone();
    if sender.is_empty() || sender == self_id {
        return;
    }
    // Appservice/bridge ignore patterns.
    for pattern in &runtime.cfg.ignore_user_patterns {
        if let Ok(re) = regex::Regex::new(pattern) {
            if re.is_match(&sender) {
                return;
            }
        }
    }
    let event_id = event
        .get("event_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let content = event.get("content").cloned().unwrap_or(json!({}));
    let msgtype = content
        .get("msgtype")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    // Allowlist gating (users + rooms).
    if !room_gate_allows(runtime, pairing, room_id, is_dm, &sender).await {
        return;
    }

    match msgtype {
        "m.text" | "m.emote" => {}
        "m.notice" => {
            if !runtime.cfg.process_notices {
                return;
            }
        }
        "m.image" | "m.audio" | "m.video" | "m.file" => {
            handle_media_message(runtime, dispatcher, room_id, &sender, &event_id, &content).await;
            return;
        }
        _ => return,
    }

    let mut body = content
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if body.trim().is_empty() {
        return;
    }

    // Reply fallback stripping (hermes `_handle_text_message`).
    let relates_to = content.get("m.relates_to").cloned().unwrap_or(json!({}));
    let is_reply = relates_to
        .get("m.in_reply_to")
        .and_then(|v| v.get("event_id"))
        .is_some();
    if is_reply && body.starts_with("> ") {
        body = strip_reply_fallback(&body);
    }

    // Mention gating in group rooms.
    let mut mentioned = false;
    if !is_dm && runtime.cfg.require_mention {
        let free = runtime.cfg.free_response_rooms.iter().any(|r| r == room_id);
        if !free {
            let display = runtime.display_name.lock().await.clone();
            mentioned = is_mentioned(&body, &self_id, &display);
            if !mentioned {
                return;
            }
            body = strip_mention(&body, &self_id, &display);
            body = body.trim().to_string();
        }
    } else if !is_dm {
        let display = runtime.display_name.lock().await.clone();
        mentioned = is_mentioned(&body, &self_id, &display);
        if mentioned {
            body = strip_mention(&body, &self_id, &display);
            body = body.trim().to_string();
        }
    }
    let _ = mentioned;

    let mut msg_event = MessageEvent {
        platform: "matrix".into(),
        chat_id: room_id.to_string(),
        sender_id: sender.clone(),
        sender_name: localpart(&sender),
        text: body,
        message_id: event_id,
        attachments: Vec::new(),
    };
    if !crate::messaging::pre_gateway_dispatch_gate_public(&mut msg_event).await {
        return;
    }
    runtime.send_typing(room_id).await;
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
    let (reply_text, media_paths) = crate::messaging::extract_media_tags(&full);
    for path in &media_paths {
        runtime.send_media(room_id, path).await;
    }
    if !reply_text.trim().is_empty() {
        // P704: ledger-protected reply delivery.
        dispatcher
            .try_send_with_ledger("matrix", room_id, &reply_text, || async {
                match runtime.send_text(room_id, &reply_text).await {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        eprintln!("[matrix] reply to {room_id} failed: {e}");
                        Err(e.to_string())
                    }
                }
            })
            .await;
    }
}

async fn handle_media_message(
    runtime: &Arc<Runtime>,
    dispatcher: &Arc<Dispatcher>,
    room_id: &str,
    sender: &str,
    event_id: &str,
    content: &Value,
) {
    let url = content.get("url").and_then(|v| v.as_str()).unwrap_or("");
    if url.is_empty() || !url.starts_with("mxc://") {
        eprintln!("[matrix] rejecting inbound media {event_id} with non-MXC URL");
        return;
    }
    let info = content.get("info").cloned().unwrap_or(json!({}));
    let mime = info
        .get("mimetype")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let body = content
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or("attachment");
    let attachment = match runtime.download_mxc(url, &mime, body).await {
        Some(att) => att,
        None => return,
    };
    let mut event = MessageEvent {
        platform: "matrix".into(),
        chat_id: room_id.to_string(),
        sender_id: sender.to_string(),
        sender_name: localpart(sender),
        text: String::new(),
        message_id: event_id.to_string(),
        attachments: vec![attachment],
    };
    if !crate::messaging::pre_gateway_dispatch_gate_public(&mut event).await {
        return;
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
        runtime.send_media(room_id, path).await;
    }
    if !reply_text.trim().is_empty() {
        // P704: ledger-protected reply delivery.
        dispatcher
            .try_send_with_ledger("matrix", room_id, &reply_text, || async {
                match runtime.send_text(room_id, &reply_text).await {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        eprintln!("[matrix] reply to {room_id} failed: {e}");
                        Err(e.to_string())
                    }
                }
            })
            .await;
    }
}

/// Allowlist∪pairing gate (hermes MATRIX_ALLOWED_USERS /
/// MATRIX_ALLOWED_ROOMS semantics + ulnclaw pairing union).
async fn room_gate_allows(
    runtime: &Arc<Runtime>,
    pairing: &Option<Arc<crate::pairing::PairingStore>>,
    room_id: &str,
    is_dm: bool,
    sender: &str,
) -> bool {
    if !runtime.cfg.allowed_rooms.is_empty()
        && !runtime.cfg.allowed_rooms.contains(&room_id.to_string())
    {
        return false;
    }
    if runtime.cfg.allow_all_users {
        return true;
    }
    if runtime
        .cfg
        .allowed_users
        .iter()
        .any(|u| u == sender || u == "*")
    {
        return true;
    }
    if let Some(store) = pairing {
        if store.is_approved("matrix", sender) {
            return true;
        }
        // Pairing offers only make sense where we can DM back — the
        // sender's room itself.
        if is_dm {
            if let Some(code_msg) = crate::messaging::pairing_offer_public(
                store.as_ref(),
                "matrix",
                sender,
                &localpart(sender),
            ) {
                let _ = runtime.send_text(room_id, &code_msg).await;
            }
        }
    }
    false
}

/// Mention detection: `@user:server`, display name, or display-name
/// lowercased (hermes `_is_mentioned` core).
pub fn is_mentioned(body: &str, user_id: &str, display_name: &str) -> bool {
    let lower = body.to_lowercase();
    if lower.contains(&user_id.to_lowercase()) {
        return true;
    }
    if !display_name.is_empty() && lower.contains(&display_name.to_lowercase()) {
        return true;
    }
    false
}

/// Strip the mention tokens from the body (hermes `_strip_mention`).
pub fn strip_mention(body: &str, user_id: &str, display_name: &str) -> String {
    let mut out = body.to_string();
    out = crate::mattermost::strip_pattern_ci(&out, user_id);
    if !display_name.is_empty() {
        // Strip the @-prefixed form first so the bare name pass doesn't
        // strand a leading `@`.
        out = crate::mattermost::strip_pattern_ci(&out, &format!("@{display_name}"));
        out = crate::mattermost::strip_pattern_ci(&out, display_name);
    }
    // Collapse leftover double spaces from removals.
    while out.contains("  ") {
        out = out.replace("  ", " ");
    }
    out
}

/// hermes reply-fallback strip: leading `> ` quote block + blank
/// separator line.
pub fn strip_reply_fallback(body: &str) -> String {
    let mut stripped: Vec<&str> = Vec::new();
    let mut past_fallback = false;
    for line in body.split('\n') {
        if !past_fallback {
            if line.starts_with("> ") || line == ">" {
                continue;
            }
            if line.trim().is_empty() {
                past_fallback = true;
                continue;
            }
            past_fallback = true;
        }
        stripped.push(line);
    }
    if stripped.is_empty() {
        body.to_string()
    } else {
        stripped.join("\n")
    }
}

struct MatrixSender {
    runtime: Arc<Runtime>,
}

#[async_trait::async_trait]
impl crate::messaging::PlatformSender for MatrixSender {
    async fn send_text(&self, chat_id: &str, text: &str) {
        if let Err(e) = self.runtime.send_text(chat_id, text).await {
            eprintln!("[matrix] send_text to {chat_id} failed: {e}");
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
    fn mxc_parsing() {
        let (server, id) = parse_mxc("mxc://example.org/abc123").unwrap();
        assert_eq!(server, "example.org");
        assert_eq!(id, "abc123");
        assert!(parse_mxc("https://example.org/abc").is_none());
        assert!(parse_mxc("mxc://example.org").is_none());
        assert!(parse_mxc("mxc://example.org/a/b").is_none());
    }

    #[test]
    fn localpart_extraction() {
        assert_eq!(localpart("@alice:example.org"), "alice");
        assert_eq!(localpart("bob@example.org"), "bob@example.org");
    }

    #[test]
    fn mention_detection_and_strip() {
        assert!(is_mentioned(
            "hey @bot:example.org hi",
            "@bot:example.org",
            "Bot"
        ));
        assert!(is_mentioned("hey Bot hi", "@bot:example.org", "Bot"));
        assert!(!is_mentioned("hello world", "@bot:example.org", "Bot"));
        let stripped = strip_mention("@Bot do this", "@bot:example.org", "Bot");
        assert_eq!(stripped.trim(), "do this");
    }

    #[test]
    fn reply_fallback_stripped() {
        let body = "> <@alice:x> original text\n> more quote\n\nactual reply";
        assert_eq!(strip_reply_fallback(body), "actual reply");
        let no_fallback = "plain message";
        assert_eq!(strip_reply_fallback(no_fallback), "plain message");
    }

    #[test]
    fn max_message_length_clamp() {
        assert_eq!(clamp_max_message_length(0), DEFAULT_MAX_MESSAGE_LENGTH);
        assert_eq!(clamp_max_message_length(100), 500);
        assert_eq!(clamp_max_message_length(16000), 16000);
        assert_eq!(
            clamp_max_message_length(999_999),
            MAX_MESSAGE_LENGTH_CEILING
        );
    }

    #[test]
    fn urlencoding_roundtrip() {
        let encoded = urlencoding("!room:example.org");
        assert!(!encoded.contains('!'));
        assert!(!encoded.contains(':'));
    }

    #[test]
    fn config_resolve_defaults() {
        let cfg = MatrixConfig {
            homeserver: "https://matrix.example.org/".into(),
            ..Default::default()
        };
        let resolved = cfg.resolve();
        assert_eq!(resolved.homeserver, "https://matrix.example.org");
        assert_eq!(resolved.max_message_length, DEFAULT_MAX_MESSAGE_LENGTH);
        assert!(resolved.require_mention);
        assert!(!resolved.process_notices);
    }
}

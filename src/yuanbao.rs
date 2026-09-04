//! Yuanbao platform adapter — port of hermes
//! `gateway/platforms/yuanbao.py` (functional core) @ v2026.8.3.
//!
//! Connects to the Yuanbao WebSocket gateway: sign-token HTTP auth
//! (HMAC-SHA256 over nonce+timestamp+app_key+app_secret), AUTH_BIND
//! handshake with connectId extraction, 30s ping/pong heartbeats with
//! a two-miss reconnect threshold, protobuf `InboundMessagePush`
//! decode → ulnclaw dispatch, and `send_c2c_message` /
//! `send_group_message` text replies with 4000-char chunking.
//!
//! Outbound media is ported (hermes `yuanbao_media.py` +
//! MediaSendHandler): `MEDIA:` reply paths upload through
//! `genUploadInfo` temporary COS credentials + a signed
//! global-accelerate PUT, then dispatch as TIMImageElem (parsed
//! PNG/JPEG/GIF/WebP dimensions, md5 uuid, TIM `image_format`) or
//! TIMFileElem, caption appended as a TIMTextElem.
//!
//! Stickers are ported (hermes `yuanbao_sticker.py`): inbound
//! TIMFaceElem renders as `[emoji: <name>]` and `STICKER:<name>`
//! reply tags send catalog stickers as TIMFaceElem (fuzzy lookup).
//!
//! Inbound media is ported (hermes ExtractContentMiddleware +
//! MediaResolveMiddleware): image/file refs resolve through
//! `/api/resource/v1/download` (resourceId exchange, one 401 token
//! refresh retry), download into the media cache as attachments, and
//! `[kind|ybres:RID]` text anchors patch to local paths.
//!
//! Known differences: the inbound middleware pipeline collapses into a
//! direct decode→gate→dispatch path (recall guard and owner commands
//! not ported; forwarded WeChat chat records — elem_type 1009 — ARE
//! deep-parsed into the prompt, minus the WS RUNNING loading
//! heartbeat); anchor patching matches
//! by resourceId instead of zipping; quote/observed-media backfill
//! reads an adapter-side message-content cache + per-group observed
//! buffer instead of the hermes transcript store (ulnclaw transcripts
//! carry no platform message ids); the reply-to pointer skips the
//! "own message" variant; the resolve-concurrency config knob is fixed
//! at the hermes default
//! (6); the slow-response notifier and reply heartbeats are not
//! ported.

use crate::messaging::{Dispatcher, MessageEvent};
use crate::pairing::PairingStore;
use crate::yuanbao_proto as proto;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Sha256;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const DEFAULT_API_DOMAIN: &str = "https://bot.yuanbao.tencent.com";
const SIGN_TOKEN_PATH: &str = "/api/v5/robotLogic/sign-token";
const SIGN_RETRYABLE_CODE: i64 = 10099;
const SIGN_MAX_RETRIES: u32 = 3;
const SIGN_RETRY_DELAY_SECS: f64 = 1.0;
const CACHE_REFRESH_MARGIN_SECS: u64 = 60;

const CONNECT_TIMEOUT_SECS: u64 = 20;
const AUTH_TIMEOUT_SECS: u64 = 10;
const HEARTBEAT_INTERVAL_SECS: u64 = 30;
const PONG_TIMEOUT_SECS: u64 = 10;
const HEARTBEAT_TIMEOUT_THRESHOLD: u32 = 2;
const DEFAULT_SEND_TIMEOUT_SECS: u64 = 30;

// Observed-media backfill (hermes OBSERVED_MEDIA_BACKFILL_*): how many
// recent group messages to scan, and the per-turn resolve cap.
const OBSERVED_MEDIA_BACKFILL_LOOKBACK: usize = 50;
const OBSERVED_MEDIA_BACKFILL_MAX_RESOLVE_PER_TURN: usize = 12;
// Adapter-side message-content cache cap (hermes `_msg_content_cache`
// FIFO trim at 200 entries).
const MSG_CONTENT_CACHE_MAX: usize = 200;
// Reply-to pointer snippet length (hermes `reply_to_text[:500]`).
const QUOTE_SNIPPET_MAX_CHARS: usize = 500;
// hermes `_RESOLVABLE_MEDIA_KINDS` (voice is never re-resolved).
const RESOLVABLE_MEDIA_KINDS: &[&str] = &["image", "file", "video"];
// hermes `FORWARD_MSG_TEXT_MAX_CHARS` — per-record combined-text cap for
// forwarded chat records (record count is NOT capped, design §2.10.3).
const FORWARD_MSG_TEXT_MAX_CHARS: usize = 1000;

/// hermes MAX_TEXT_CHUNK — Yuanbao single-message character limit.
const MAX_TEXT_CHUNK: usize = 4000;
/// hermes `_DEBOUNCE_WINDOW` for multi-part inbound aggregation.
const DEBOUNCE_WINDOW_SECS: f64 = 1.5;
/// hermes NO_RECONNECT_CLOSE_CODES.
const NO_RECONNECT_CLOSE_CODES: &[u16] = &[4012, 4013, 4014, 4018, 4019, 4021];

/// hermes `yuanbao_media.UPLOAD_INFO_PATH` (COS upload credentials).
const UPLOAD_INFO_PATH: &str = "/api/resource/genUploadInfo";
/// hermes `MEDIA_MAX_SIZE_MB`.
const MEDIA_MAX_SIZE_MB: u64 = 50;
const COS_CREDS_TIMEOUT_SECS: u64 = 15;
const COS_UPLOAD_TIMEOUT_SECS: u64 = 120;

fn app_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// `[messaging.yuanbao]` — Yuanbao WS-gateway adapter (hermes
/// `platforms.yuanbao` extra config + YUANBAO_* env vars).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct YuanbaoConfig {
    pub enabled: bool,
    /// App key (fallback `YUANBAO_APP_ID`).
    pub app_id: String,
    /// App secret (fallback `YUANBAO_APP_SECRET`).
    pub app_secret: String,
    /// Bot id (optional; the sign-token response returns it — fallback
    /// `YUANBAO_BOT_ID`).
    pub bot_id: String,
    /// WebSocket gateway URL (fallback `YUANBAO_WS_URL`).
    pub ws_url: String,
    /// API domain for sign-token (fallback `YUANBAO_API_DOMAIN`).
    pub api_domain: String,
    /// Optional route-env header (fallback `YUANBAO_ROUTE_ENV`).
    pub route_env: String,
    /// DM intake policy: pairing (default) | allowlist | open | disabled.
    pub dm_policy: String,
    pub allow_from: Vec<String>,
    /// Group intake policy: disabled (default) | allowlist | open
    /// (hermes group pairing resolves to closed).
    pub group_policy: String,
    pub group_allow_from: Vec<String>,
}

impl Default for YuanbaoConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            app_id: String::new(),
            app_secret: String::new(),
            bot_id: String::new(),
            ws_url: String::new(),
            api_domain: String::new(),
            route_env: String::new(),
            dm_policy: "pairing".into(),
            allow_from: Vec::new(),
            group_policy: "disabled".into(),
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

pub fn resolve_app_id(cfg: &YuanbaoConfig) -> String {
    let trimmed = cfg.app_id.trim();
    if !trimmed.is_empty() {
        return trimmed.to_string();
    }
    env_or_none("YUANBAO_APP_ID").unwrap_or_default()
}

pub fn resolve_app_secret(cfg: &YuanbaoConfig) -> String {
    let trimmed = cfg.app_secret.trim();
    if !trimmed.is_empty() {
        return trimmed.to_string();
    }
    env_or_none("YUANBAO_APP_SECRET").unwrap_or_default()
}

pub fn resolve_bot_id(cfg: &YuanbaoConfig) -> String {
    let trimmed = cfg.bot_id.trim();
    if !trimmed.is_empty() {
        return trimmed.to_string();
    }
    env_or_none("YUANBAO_BOT_ID").unwrap_or_default()
}

pub fn resolve_ws_url(cfg: &YuanbaoConfig) -> String {
    let trimmed = cfg.ws_url.trim();
    if !trimmed.is_empty() {
        return trimmed.to_string();
    }
    env_or_none("YUANBAO_WS_URL").unwrap_or_default()
}

pub fn resolve_api_domain(cfg: &YuanbaoConfig) -> String {
    let trimmed = cfg.api_domain.trim();
    if !trimmed.is_empty() {
        return trimmed.trim_end_matches('/').to_string();
    }
    env_or_none("YUANBAO_API_DOMAIN")
        .map(|v| v.trim_end_matches('/').to_string())
        .unwrap_or_else(|| DEFAULT_API_DOMAIN.to_string())
}

pub fn resolve_route_env(cfg: &YuanbaoConfig) -> String {
    let trimmed = cfg.route_env.trim();
    if !trimmed.is_empty() {
        return trimmed.to_string();
    }
    env_or_none("YUANBAO_ROUTE_ENV").unwrap_or_default()
}

// ---------------------------------------------------------------------------
// SignManager (hermes `SignManager`)
// ---------------------------------------------------------------------------

/// hermes `compute_signature`: HMAC-SHA256(key=app_secret,
/// msg=nonce+timestamp+app_key+app_secret).
pub fn compute_signature(nonce: &str, timestamp: &str, app_key: &str, app_secret: &str) -> String {
    let plain = format!("{nonce}{timestamp}{app_key}{app_secret}");
    let mut mac = Hmac::<Sha256>::new_from_slice(app_secret.as_bytes()).expect("hmac key");
    mac.update(plain.as_bytes());
    let result = mac.finalize().into_bytes();
    result.iter().map(|b| format!("{b:02x}")).collect()
}

/// hermes `build_timestamp`: Beijing-time ISO-8601 without milliseconds.
pub fn build_timestamp() -> String {
    let bj = chrono::FixedOffset::east_opt(8 * 3600).expect("+08:00 offset");
    chrono::Utc::now()
        .with_timezone(&bj)
        .format("%Y-%m-%dT%H:%M:%S+08:00")
        .to_string()
}

#[derive(Debug, Clone)]
pub struct SignTokenEntry {
    token: String,
    bot_id: String,
    source: String,
    expire_ts: Instant,
}

pub struct SignManager {
    client: reqwest::Client,
    app_key: String,
    app_secret: String,
    api_domain: String,
    route_env: String,
    cache: Mutex<Option<SignTokenEntry>>,
}

impl SignManager {
    pub fn new(app_key: String, app_secret: String, api_domain: String, route_env: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            app_key,
            app_secret,
            api_domain,
            route_env,
            cache: Mutex::new(None),
        }
    }

    fn cache_valid(&self) -> Option<SignTokenEntry> {
        let cache = self.cache.lock().unwrap();
        cache.as_ref().and_then(|entry| {
            if entry.expire_ts > Instant::now() + Duration::from_secs(CACHE_REFRESH_MARGIN_SECS) {
                Some(entry.clone())
            } else {
                None
            }
        })
    }

    /// Cached token fetch (hermes `get_token`).
    pub async fn get_token(&self) -> std::result::Result<SignTokenEntry, String> {
        if let Some(entry) = self.cache_valid() {
            return Ok(entry);
        }
        let data = self.fetch().await?;
        let duration = data.get("duration").and_then(|v| v.as_u64()).unwrap_or(0);
        let ttl = if duration > 0 { duration } else { 3600 };
        let entry = SignTokenEntry {
            token: data
                .get("token")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            bot_id: data
                .get("bot_id")
                .map(|v| v.to_string().trim_matches('"').to_string())
                .unwrap_or_default(),
            source: data
                .get("source")
                .and_then(|v| v.as_str())
                .unwrap_or("bot")
                .to_string(),
            expire_ts: Instant::now() + Duration::from_secs(ttl),
        };
        if entry.token.is_empty() {
            return Err(format!("Sign token response missing token: {data}"));
        }
        *self.cache.lock().unwrap() = Some(entry.clone());
        Ok(entry)
    }

    /// hermes `force_refresh`.
    pub async fn force_refresh(&self) -> std::result::Result<SignTokenEntry, String> {
        self.cache.lock().unwrap().take();
        self.get_token().await
    }

    /// hermes `fetch` with retryable-code retries.
    async fn fetch(&self) -> std::result::Result<Value, String> {
        let url = format!("{}{}", self.api_domain, SIGN_TOKEN_PATH);
        let mut last_error = String::new();
        for attempt in 0..=SIGN_MAX_RETRIES {
            let mut nonce_bytes = [0u8; 16];
            getrandom_fill(&mut nonce_bytes);
            let nonce: String = nonce_bytes.iter().map(|b| format!("{b:02x}")).collect();
            let timestamp = build_timestamp();
            let signature = compute_signature(&nonce, &timestamp, &self.app_key, &self.app_secret);
            let payload = json!({
                "app_key": self.app_key,
                "nonce": nonce,
                "signature": signature,
                "timestamp": timestamp,
            });
            let mut request = self
                .client
                .post(&url)
                .timeout(Duration::from_secs(10))
                .header("Content-Type", "application/json")
                .header("X-AppVersion", app_version())
                .header("X-OperationSystem", std::env::consts::OS)
                .header("X-Instance-Id", proto::INSTANCE_ID.to_string())
                .header("X-Bot-Version", app_version());
            if !self.route_env.is_empty() {
                request = request.header("X-Route-Env", &self.route_env);
            }
            let response = request
                .json(&payload)
                .send()
                .await
                .map_err(|e| format!("sign-token request: {e}"))?;
            let status = response.status();
            let raw = response.text().await.unwrap_or_default();
            if status.as_u16() != 200 {
                return Err(format!(
                    "Sign token API returned {}: {}",
                    status.as_u16(),
                    truncate_str(&raw, 200)
                ));
            }
            let result: Value = serde_json::from_str(&raw)
                .map_err(|e| format!("Sign token response parse error: {e}"))?;
            let code = result.get("code").and_then(|v| v.as_i64());
            if code == Some(0) {
                let data = result
                    .get("data")
                    .filter(|v| v.is_object())
                    .cloned()
                    .ok_or_else(|| format!("Sign token response missing 'data' field: {result}"))?;
                return Ok(data);
            }
            if code == Some(SIGN_RETRYABLE_CODE) && attempt < SIGN_MAX_RETRIES {
                last_error = format!("sign-token retryable code={SIGN_RETRYABLE_CODE}");
                tokio::time::sleep(Duration::from_secs_f64(SIGN_RETRY_DELAY_SECS)).await;
                continue;
            }
            let msg = result.get("msg").and_then(|v| v.as_str()).unwrap_or("");
            return Err(format!("Sign token error: code={code:?}, msg={msg}"));
        }
        Err(format!(
            "Sign token failed: max retries exceeded ({last_error})"
        ))
    }
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

fn truncate_str(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        value.to_string()
    } else {
        value.chars().take(max).collect()
    }
}

// ---------------------------------------------------------------------------
// Adapter state
// ---------------------------------------------------------------------------

struct Runner {
    cfg: YuanbaoConfig,
    sign: Arc<SignManager>,
    dispatcher: Arc<Dispatcher>,
    pairing: Option<Arc<PairingStore>>,
    bot_id: Mutex<String>,
    dedup: Mutex<HashMap<String, Instant>>,
    connected: AtomicBool,
    /// Outstanding ping without pong (hermes heartbeat miss tracking).
    pong_pending: AtomicBool,
    /// Debounce buffer: sender key -> (raw frames, generation).
    inbound_buffer: Mutex<HashMap<String, (Vec<Vec<u8>>, u64)>>,
}

impl Runner {
    fn is_duplicate(&self, key: &str) -> bool {
        let mut seen = self.dedup.lock().unwrap();
        dedup_check(&mut seen, key)
    }
}

fn dedup_check(seen: &mut HashMap<String, Instant>, key: &str) -> bool {
    if seen.len() > 1024 {
        seen.retain(|_, at| at.elapsed() < Duration::from_secs(300));
    }
    if let Some(at) = seen.get(key) {
        if at.elapsed() < Duration::from_secs(300) {
            return true;
        }
    }
    seen.insert(key.to_string(), Instant::now());
    false
}

/// Start the Yuanbao adapter (hermes `YuanbaoAdapter.connect` +
/// ConnectionManager lifecycle).
pub async fn run(
    cfg: YuanbaoConfig,
    dispatcher: Arc<Dispatcher>,
    pairing: Option<Arc<PairingStore>>,
) {
    let app_id = resolve_app_id(&cfg);
    let app_secret = resolve_app_secret(&cfg);
    if app_id.is_empty() || app_secret.is_empty() {
        eprintln!("[yuanbao] disabled: YUANBAO_APP_ID and YUANBAO_APP_SECRET are required");
        return;
    }
    let ws_url = resolve_ws_url(&cfg);
    if ws_url.is_empty() {
        eprintln!("[yuanbao] disabled: no ws_url configured (set messaging.yuanbao.ws_url or YUANBAO_WS_URL)");
        return;
    }
    let sign = Arc::new(SignManager::new(
        app_id.clone(),
        app_secret,
        resolve_api_domain(&cfg),
        resolve_route_env(&cfg),
    ));
    let runner = Arc::new(Runner {
        cfg: cfg.clone(),
        sign,
        dispatcher,
        pairing,
        bot_id: Mutex::new(resolve_bot_id(&cfg)),
        dedup: Mutex::new(HashMap::new()),
        connected: AtomicBool::new(false),
        pong_pending: AtomicBool::new(false),
        inbound_buffer: Mutex::new(HashMap::new()),
    });

    register_sender(runner.clone(), ws_url.clone());

    let mut backoff_idx: usize = 0;
    loop {
        match run_session(&runner, &ws_url).await {
            Ok(()) => backoff_idx = 0,
            Err(code) => {
                if NO_RECONNECT_CLOSE_CODES.contains(&code) {
                    eprintln!("[yuanbao] close code {code} is non-recoverable, NOT reconnecting");
                    clear_active_handle();
                    return;
                }
                let delay = [2u64, 5, 10, 30, 60][backoff_idx.min(4)];
                eprintln!("[yuanbao] session ended (code={code}) — reconnecting in {delay}s");
                tokio::time::sleep(Duration::from_secs(delay)).await;
                backoff_idx = (backoff_idx + 1).min(4);
            }
        }
    }
}

/// One WS session: sign-token → connect → AUTH_BIND → heartbeat + recv.
/// Returns Err(close_code) on failure/close.
async fn run_session(runner: &Arc<Runner>, ws_url: &str) -> std::result::Result<(), u16> {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    // Step 1: sign token.
    let token_entry = match runner.sign.get_token().await {
        Ok(entry) => entry,
        Err(e) => {
            eprintln!("[yuanbao] sign-token failed: {e}");
            return Err(0);
        }
    };
    if !token_entry.bot_id.is_empty() {
        *runner.bot_id.lock().unwrap() = token_entry.bot_id.clone();
    }

    // Step 2: WS connect.
    let connect = tokio::time::timeout(
        Duration::from_secs(CONNECT_TIMEOUT_SECS + 10),
        tokio_tungstenite::connect_async(ws_url),
    )
    .await;
    let (ws, _) = match connect {
        Ok(Ok(pair)) => pair,
        Ok(Err(e)) => {
            eprintln!("[yuanbao] WS connect failed: {e}");
            return Err(0);
        }
        Err(_) => {
            eprintln!("[yuanbao] WS connect timed out");
            return Err(0);
        }
    };
    let (mut sink, mut stream) = ws.split();
    eprintln!("[yuanbao] WebSocket connected to {ws_url}");

    // Step 3: AUTH_BIND → BIND_ACK.
    let auth_msg_id = new_uuid();
    let auth_bytes = proto::encode_auth_bind(
        "ybBot",
        &runner.bot_id.lock().unwrap().clone(),
        if token_entry.source.is_empty() {
            "bot"
        } else {
            &token_entry.source
        },
        &token_entry.token,
        &auth_msg_id,
        &app_version(),
        std::env::consts::OS,
        &app_version(),
        &resolve_route_env(&runner.cfg),
    );
    if sink.send(WsMessage::Binary(auth_bytes)).await.is_err() {
        return Err(0);
    }

    let deadline = Instant::now() + Duration::from_secs(AUTH_TIMEOUT_SECS);
    let connect_id = loop {
        if Instant::now() >= deadline {
            eprintln!("[yuanbao] AUTH_BIND timeout waiting for BIND_ACK");
            return Err(0);
        }
        let next = tokio::time::timeout(
            deadline.saturating_duration_since(Instant::now()),
            stream.next(),
        )
        .await;
        let Ok(Some(Ok(message))) = next else {
            return Err(0);
        };
        let WsMessage::Binary(raw) = message else {
            continue;
        };
        let msg = proto::decode_conn_msg(&raw);
        if msg.head.cmd_type == proto::CMD_TYPE_RESPONSE && msg.head.cmd == proto::CMD_AUTH_BIND {
            match proto::decode_auth_bind_rsp(&msg.data) {
                Ok(connect_id) => break connect_id,
                Err(e) => {
                    eprintln!("[yuanbao] BIND_ACK error: {e}");
                    return Err(0);
                }
            }
        }
    };
    runner.connected.store(true, Ordering::SeqCst);
    eprintln!(
        "[yuanbao] connected connectId={connect_id} botId={}",
        runner.bot_id.lock().unwrap()
    );

    // Step 4: heartbeat + receive loops.
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    register_active_handle(Arc::new(YuanbaoHandle {
        runner: runner.clone(),
        out_tx: out_tx.clone(),
    }));
    let (hb_tx, mut hb_rx) = tokio::sync::mpsc::channel::<()>(4);
    let runner_hb = runner.clone();
    let heartbeat = tokio::spawn(async move {
        let mut missed: u32 = 0;
        loop {
            tokio::time::sleep(Duration::from_secs(HEARTBEAT_INTERVAL_SECS)).await;
            let msg_id = new_uuid();
            runner_hb.pong_pending.store(true, Ordering::SeqCst);
            if hb_tx.send(()).await.is_err() {
                return;
            }
            let _ = msg_id;
            tokio::time::sleep(Duration::from_secs(PONG_TIMEOUT_SECS)).await;
            if runner_hb.pong_pending.load(Ordering::SeqCst) {
                missed += 1;
                eprintln!("[yuanbao] PONG timeout ({missed}/{HEARTBEAT_TIMEOUT_THRESHOLD})");
                if missed >= HEARTBEAT_TIMEOUT_THRESHOLD {
                    eprintln!("[yuanbao] heartbeat threshold exceeded, triggering reconnect");
                    runner_hb.connected.store(false, Ordering::SeqCst);
                    return;
                }
            } else {
                missed = 0;
            }
        }
    });

    let result: std::result::Result<(), u16> = loop {
        tokio::select! {
            _ = hb_rx.recv() => {
                let msg_id = new_uuid();
                if sink.send(WsMessage::Binary(proto::encode_ping(&msg_id))).await.is_err() {
                    break Err(0);
                }
            }
            outbound = out_rx.recv() => {
                let Some(bytes) = outbound else { break Ok(()) };
                if sink.send(WsMessage::Binary(bytes)).await.is_err() {
                    break Err(0);
                }
            }
            incoming = stream.next() => {
                let Some(message) = incoming else { break Err(0) };
                let message = match message {
                    Ok(message) => message,
                    Err(e) => break Err(close_code_from_error(&e.to_string())),
                };
                let WsMessage::Binary(raw) = message else { continue };
                match handle_frame(runner, &raw, &out_tx).await {
                    FrameAction::Continue => {}
                    FrameAction::Close(code) => break Err(code),
                }
            }
        }
    };
    runner.connected.store(false, Ordering::SeqCst);
    heartbeat.abort();
    result
}

enum FrameAction {
    Continue,
    Close(u16),
}

/// hermes `_handle_frame` (core paths).
async fn handle_frame(
    runner: &Arc<Runner>,
    raw: &[u8],
    out_tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
) -> FrameAction {
    let msg = proto::decode_conn_msg(raw);
    let head = &msg.head;
    match head.cmd_type {
        proto::CMD_TYPE_RESPONSE if head.cmd == proto::CMD_PING => {
            runner.pong_pending.store(false, Ordering::SeqCst);
            FrameAction::Continue
        }
        proto::CMD_TYPE_RESPONSE
            if head.cmd == "send_group_heartbeat" || head.cmd == "send_private_heartbeat" =>
        {
            FrameAction::Continue
        }
        proto::CMD_TYPE_RESPONSE => {
            // Response to an outbound RPC (send_c2c/group_message): the
            // send path correlates by msg_id.
            pending_responses()
                .lock()
                .unwrap()
                .insert(head.msg_id.clone(), msg.data.clone());
            FrameAction::Continue
        }
        proto::CMD_TYPE_PUSH => {
            // Kickout is fatal — another login replaced this session.
            if head.cmd == "kickout" {
                eprintln!("[yuanbao] kickout received — session replaced, not reconnecting");
                return FrameAction::Close(4013);
            }
            if head.need_ack {
                let ack = proto::encode_push_ack(head);
                out_tx.send(ack).await.ok();
            }
            if !msg.data.is_empty() {
                buffer_inbound(runner, out_tx, msg.data.clone());
            }
            FrameAction::Continue
        }
        _ => FrameAction::Continue,
    }
}

fn close_code_from_error(text: &str) -> u16 {
    for code in NO_RECONNECT_CLOSE_CODES {
        if text.contains(&code.to_string()) {
            return *code;
        }
    }
    0
}

/// Outbound RPC response correlation (msg_id -> response payload).
static PENDING_RESPONSES: std::sync::OnceLock<Mutex<HashMap<String, Vec<u8>>>> =
    std::sync::OnceLock::new();

fn pending_responses() -> &'static Mutex<HashMap<String, Vec<u8>>> {
    PENDING_RESPONSES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn new_uuid() -> String {
    let mut bytes = [0u8; 16];
    getrandom_fill(&mut bytes);
    bytes[6] = (bytes[6] & 0x0F) | 0x40;
    bytes[8] = (bytes[8] & 0x3F) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

// ---------------------------------------------------------------------------
// Inbound (hermes pipeline essence: decode → gate → dispatch)
// ---------------------------------------------------------------------------

/// hermes `_push_to_inbound` debounce: aggregate frames per sender key
/// within a short window, then run one merged pipeline pass.
fn buffer_inbound(
    runner: &Arc<Runner>,
    out_tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
    data: Vec<u8>,
) {
    // Lightweight sender-key extraction (hermes `_extract_sender_key`).
    let key = proto::decode_inbound_push(&data)
        .map(|push| format!("{}:{}", push.from_account, push.group_code))
        .unwrap_or_else(|| format!("__unknown_{}", data.len()));
    let generation;
    {
        let mut buffer = runner.inbound_buffer.lock().unwrap();
        let entry = buffer.entry(key.clone()).or_insert_with(|| (Vec::new(), 0));
        entry.0.push(data);
        entry.1 += 1;
        generation = entry.1;
    }
    let runner = runner.clone();
    let out_tx = out_tx.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs_f64(DEBOUNCE_WINDOW_SECS)).await;
        let frames = {
            let mut buffer = runner.inbound_buffer.lock().unwrap();
            match buffer.get(&key) {
                Some((frames, gen)) if *gen == generation => {
                    buffer.remove(&key).map(|(frames, _)| frames)
                }
                _ => None,
            }
        };
        if let Some(frames) = frames {
            handle_inbound_frames(&runner, &frames, &out_tx).await;
        }
    });
}

/// Decode + gate + dispatch one debounced batch of push frames (hermes
/// pipeline essence).
async fn handle_inbound_frames(
    runner: &Arc<Runner>,
    frames: &[Vec<u8>],
    out_tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
) {
    let mut merged: Option<proto::InboundPush> = None;
    for data in frames {
        let Some(push) = proto::decode_inbound_push(data) else {
            continue;
        };
        match merged.as_mut() {
            Some(into) => {
                into.msg_body.extend(push.msg_body);
                if into.msg_id.is_empty() {
                    into.msg_id = push.msg_id;
                }
                if into.msg_key.is_empty() {
                    into.msg_key = push.msg_key;
                }
            }
            None => merged = Some(push),
        }
    }
    let Some(push) = merged else { return };
    if push.from_account.is_empty() {
        return;
    }
    let dedup_key = if push.msg_key.is_empty() {
        push.msg_id.clone()
    } else {
        push.msg_key.clone()
    };
    if !dedup_key.is_empty() && runner.is_duplicate(&dedup_key) {
        return;
    }

    let is_group = !push.group_code.is_empty();
    let (chat_id, gated) = if is_group {
        let allowed = match runner.cfg.group_policy.as_str() {
            "allowlist" => runner
                .cfg
                .group_allow_from
                .iter()
                .any(|g| g.trim() == push.group_code),
            "open" => open_opted_in(),
            _ => false, // disabled + pairing (hermes: group pairing closed)
        };
        (push.group_code.clone(), allowed)
    } else {
        let intake = match runner.cfg.dm_policy.as_str() {
            "disabled" => false,
            "allowlist" => runner
                .cfg
                .allow_from
                .iter()
                .any(|a| a.trim() == push.from_account),
            "pairing" => true,
            "open" => open_opted_in(),
            _ => false,
        };
        let authorized = runner
            .cfg
            .allow_from
            .iter()
            .any(|a| a.trim() == push.from_account)
            || runner
                .pairing
                .as_ref()
                .map(|s| s.is_approved("yuanbao", &push.from_account))
                .unwrap_or(false)
            || runner.cfg.dm_policy == "open";
        (push.from_account.clone(), intake && authorized)
    };

    if !gated {
        eprintln!(
            "[yuanbao] refusing message from {} — add it to messaging.yuanbao.allow_from/group_allow_from or approve a pairing code",
            push.from_account
        );
        if !is_group {
            if let Some(store) = &runner.pairing {
                if let Some(reply) = crate::messaging::pairing_offer_public(
                    store,
                    "yuanbao",
                    &push.from_account,
                    &push.sender_nickname,
                ) {
                    send_text_via(runner, out_tx, &chat_id, "", &reply).await;
                }
            }
        }
        return;
    }

    // Extract text + media refs (hermes ExtractContentMiddleware), then
    // resolve the refs into the media cache (hermes
    // MediaResolveMiddleware) and patch `[kind|ybres:RID]` anchors.
    let (raw_text, media_refs) = extract_inbound_text(&push.msg_body);
    // hermes ForwardedRecordsParseMiddleware: deep-parse forwarded WeChat
    // chat records (elem_type 1009) into the prompt. Runs before the
    // empty-text guard — a bare forward carries no caption of its own.
    let (raw_text, media_refs) = match extract_forwarded_records(&push.msg_body) {
        Some(forward) => {
            let mut forward_refs: Vec<MediaRef> = Vec::new();
            let forward_text = build_forward_text(
                &forward,
                &push.sender_nickname,
                &raw_text,
                &mut forward_refs,
            );
            let mut all_refs = media_refs;
            all_refs.extend(forward_refs);
            (forward_text, all_refs)
        }
        None => (raw_text, media_refs),
    };
    if raw_text.trim().is_empty() {
        return;
    }
    let resolved = resolve_inbound_media(runner, &media_refs).await;
    let own_resolved = resolved.iter().filter(|entry| entry.is_some()).count();
    // hermes PlaceholderFilterMiddleware: pure placeholder text with no
    // resolved media of its own is dropped.
    if own_resolved == 0 && SKIPPABLE_PLACEHOLDERS.contains(&raw_text.trim()) {
        return;
    }
    let mut text = patch_media_anchors(&raw_text, &media_refs, &resolved);
    let mut attachments: Vec<crate::messaging::MediaAttachment> =
        resolved.iter().filter_map(|entry| entry.clone()).collect();

    // hermes QuoteContextMiddleware + MediaResolveMiddleware sources 2/3:
    // quoted-media backfill (already-local anchors + leftover ybres refs
    // from the adapter msg-content cache) or, in groups without a quote,
    // observed-media backfill from the per-group buffer.
    let (quote_id, quote_text) = extract_quote_context(&push.cloud_custom_data);
    let mut seen_paths: std::collections::HashSet<String> = attachments
        .iter()
        .map(|a| a.path.to_string_lossy().to_string())
        .collect();
    let mut seen_rids: std::collections::HashSet<String> = media_refs
        .iter()
        .map(|r| parse_resource_id(&r.url))
        .filter(|rid| !rid.is_empty())
        .collect();
    let mut backfill_refs: Vec<(String, String, String)> = Vec::new();
    if let Some(quoted_id) = quote_id.as_deref() {
        if let Some(content) = msg_content_cache_get(quoted_id) {
            for (path, mime) in local_media_from_text(&content) {
                let key = path.to_string_lossy().to_string();
                if !seen_paths.insert(key) {
                    continue;
                }
                let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) as u64;
                let original_name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                attachments.push(crate::messaging::MediaAttachment {
                    path,
                    mime,
                    bytes,
                    original_name,
                });
            }
            backfill_refs.extend(ybres_refs_from_text(&content));
        }
    } else if is_group {
        backfill_refs = collect_observed_refs(&chat_id);
    }
    if !backfill_refs.is_empty() {
        for attachment in resolve_backfill_refs(runner, backfill_refs, &mut seen_rids).await {
            let key = attachment.path.to_string_lossy().to_string();
            if seen_paths.insert(key) {
                attachments.push(attachment);
            }
        }
    }
    if is_group {
        record_observed(&chat_id, &media_refs);
    }
    if !push.msg_id.is_empty() && !text.trim().is_empty() {
        msg_content_cache_put(&push.msg_id, &text);
    }
    // hermes run.py reply-to pointer: disambiguates which prior message
    // the user is quoting.
    text = render_reply_to_prefix(&text, quote_id.as_deref(), quote_text.as_deref());

    eprintln!(
        "[yuanbao] inbound from={} {} text_len={}",
        truncate_str(&push.from_account, 12),
        if is_group {
            format!("group={}", push.group_code)
        } else {
            "dm".into()
        },
        text.chars().count()
    );

    let sender_name = if push.sender_nickname.is_empty() {
        push.from_account.clone()
    } else {
        push.sender_nickname.clone()
    };
    let mut event = MessageEvent {
        platform: "yuanbao".into(),
        chat_id: chat_id.clone(),
        sender_id: push.from_account.clone(),
        sender_name,
        text,
        message_id: push.msg_id.clone(),
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
        send_text_via(runner, out_tx, &chat_id, &push.group_code, echo).await;
    }
    let (reply_text, media_paths) = crate::messaging::extract_media_tags(&outcome.reply);
    let (reply_text, sticker_names) = crate::yuanbao_sticker::extract_sticker_tags(&reply_text);
    if media_paths.is_empty() {
        if !reply_text.trim().is_empty() {
            send_text_via(runner, out_tx, &chat_id, &push.group_code, &reply_text).await;
        }
    } else {
        // hermes: the caption rides the media message body (appended
        // TIMTextElem); extra media paths send without caption.
        for (index, path) in media_paths.iter().enumerate() {
            let caption = if index == 0 { reply_text.as_str() } else { "" };
            send_media_via(runner, out_tx, &chat_id, &push.group_code, path, caption).await;
        }
    }
    for name in &sticker_names {
        send_sticker_via(runner, out_tx, &chat_id, &push.group_code, name).await;
    }
}

fn open_opted_in() -> bool {
    for var in ["GATEWAY_ALLOW_ALL_USERS", "YUANBAO_ALLOW_ALL_USERS"] {
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

// ---------------------------------------------------------------------------
// Inbound media resolution (hermes ExtractContentMiddleware +
// MediaResolveMiddleware collapsed into the direct dispatch path)
// ---------------------------------------------------------------------------

/// hermes `MediaResolveMiddleware._fetch_resource_url` endpoint.
const RESOURCE_DOWNLOAD_PATH: &str = "/api/resource/v1/download";
/// hermes httpx timeout for the resource exchange.
const RESOURCE_RESOLVE_TIMEOUT_SECS: u64 = 15;
/// Per-object download timeout.
const MEDIA_DOWNLOAD_TIMEOUT_SECS: u64 = 60;
/// hermes `_RESOURCE_CACHE_TTL_S` (24 hours).
const RESOURCE_CACHE_TTL_SECS: u64 = 24 * 60 * 60;
/// hermes `_RESOURCE_CACHE_MAX_SIZE`.
const RESOURCE_CACHE_MAX_SIZE: usize = 256;
/// hermes `_DEFAULT_RESOLVE_CONCURRENCY` (config knob not ported).
const DEFAULT_RESOLVE_CONCURRENCY: usize = 6;

/// hermes `PlaceholderFilterMiddleware.SKIPPABLE_PLACEHOLDERS`.
const SKIPPABLE_PLACEHOLDERS: &[&str] = &[
    "[image]", "[图片]", "[file]", "[文件]", "[video]", "[视频]", "[voice]", "[语音]",
];

/// One inbound media reference (hermes `_extract_inbound_media_refs`
/// entry): image/file URL plus the original filename for files.
#[derive(Debug, Clone)]
pub(crate) struct MediaRef {
    kind: &'static str,
    url: String,
    name: String,
}

/// hermes `_parse_resource_id` — resourceId query parameter.
pub fn parse_resource_id(url: &str) -> String {
    if url.is_empty() {
        return String::new();
    }
    let Ok(parsed) = url::Url::parse(url) else {
        return String::new();
    };
    for (key, value) in parsed.query_pairs() {
        if key.eq_ignore_ascii_case("resourceid") {
            return value.trim().to_string();
        }
    }
    String::new()
}

/// hermes `ExtractContentMiddleware` text half: render msg_body to the
/// placeholder/anchor text and collect image/file media refs.
///
/// - TIMTextElem      -> text field
/// - TIMImageElem     -> `[image]` / `[image|ybres:RID]` (medium preferred)
/// - TIMFileElem      -> `[file: name]` / `[file:name|ybres:RID]`
/// - TIMSoundElem     -> `[voice]` / `[voice|ybres:RID]`
/// - TIMVideoFileElem -> `[video]` / `[video|ybres:RID]`
/// - TIMFaceElem      -> `[emoji: name]` / `[emoji]` (sticker module)
/// - anything else    -> `[media: type]`
fn extract_inbound_text(msg_body: &[proto::MsgBodyElement]) -> (String, Vec<MediaRef>) {
    let mut parts: Vec<String> = Vec::new();
    let mut refs: Vec<MediaRef> = Vec::new();
    for element in msg_body {
        match element.msg_type.as_str() {
            "TIMTextElem" => {
                if !element.msg_content.text.is_empty() {
                    parts.push(element.msg_content.text.clone());
                }
            }
            "TIMImageElem" => {
                // hermes: prefer the medium image (index 1), fall back to 0.
                let info = if element.msg_content.image_info_array.len() > 1 {
                    Some(&element.msg_content.image_info_array[1])
                } else {
                    element.msg_content.image_info_array.first()
                };
                let image_url = info
                    .map(|entry| entry.url.trim().to_string())
                    .unwrap_or_default();
                let rid = parse_resource_id(&image_url);
                parts.push(if rid.is_empty() {
                    "[image]".to_string()
                } else {
                    format!("[image|ybres:{rid}]")
                });
                if !image_url.is_empty() {
                    refs.push(MediaRef {
                        kind: "image",
                        url: image_url,
                        name: String::new(),
                    });
                }
            }
            "TIMFileElem" => {
                let file_url = element.msg_content.url.trim().to_string();
                let file_name = element.msg_content.file_name.trim().to_string();
                let rid = parse_resource_id(&file_url);
                if rid.is_empty() {
                    parts.push(if file_name.is_empty() {
                        "[file]".to_string()
                    } else {
                        format!("[file: {file_name}]")
                    });
                } else {
                    parts.push(if file_name.is_empty() {
                        format!("[file|ybres:{rid}]")
                    } else {
                        format!("[file:{file_name}|ybres:{rid}]")
                    });
                }
                if !file_url.is_empty() {
                    refs.push(MediaRef {
                        kind: "file",
                        url: file_url,
                        name: file_name,
                    });
                }
            }
            "TIMSoundElem" => {
                let rid = parse_resource_id(element.msg_content.url.trim());
                parts.push(if rid.is_empty() {
                    "[voice]".to_string()
                } else {
                    format!("[voice|ybres:{rid}]")
                });
            }
            "TIMVideoFileElem" => {
                let rid = parse_resource_id(element.msg_content.url.trim());
                parts.push(if rid.is_empty() {
                    "[video]".to_string()
                } else {
                    format!("[video|ybres:{rid}]")
                });
            }
            "TIMFaceElem" => {
                // hermes: `[emoji: {name}]` / `[emoji]` from the data JSON.
                parts.push(crate::yuanbao_sticker::render_face_element(
                    &element.msg_content,
                ));
            }
            other if !other.is_empty() => {
                parts.push(format!("[media: {other}]"));
            }
            _ => {}
        }
    }
    (parts.join("\n"), refs)
}

/// hermes `_guess_image_ext_from_url`.
fn guess_image_ext_from_url(url: &str) -> String {
    let path = url::Url::parse(url)
        .map(|parsed| parsed.path().to_string())
        .unwrap_or_default();
    let ext = std::path::Path::new(&path)
        .extension()
        .and_then(|raw| raw.to_str())
        .map(|raw| format!(".{}", raw.to_lowercase()))
        .unwrap_or_default();
    if matches!(
        ext.as_str(),
        ".jpg" | ".jpeg" | ".png" | ".gif" | ".webp" | ".bmp" | ".heic" | ".tiff"
    ) {
        ext
    } else {
        ".jpg".to_string()
    }
}

/// hermes `os.path.basename(parsed.path) or "file"`.
fn url_path_basename(url: &str) -> String {
    let path = url::Url::parse(url)
        .map(|parsed| parsed.path().to_string())
        .unwrap_or_default();
    path.rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or("file")
        .to_string()
}

fn resource_cache() -> &'static Mutex<HashMap<String, (crate::messaging::MediaAttachment, Instant)>>
{
    static CACHE: std::sync::OnceLock<
        Mutex<HashMap<String, (crate::messaging::MediaAttachment, Instant)>>,
    > = std::sync::OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// hermes `_get_cached_resource` — TTL + on-disk existence check.
fn resource_cache_get(resource_id: &str) -> Option<crate::messaging::MediaAttachment> {
    let mut cache = resource_cache().lock().unwrap();
    let (attachment, cached_at) = cache.get(resource_id)?.clone();
    if cached_at.elapsed() > Duration::from_secs(RESOURCE_CACHE_TTL_SECS)
        || !attachment.path.is_file()
    {
        cache.remove(resource_id);
        return None;
    }
    Some(attachment)
}

/// hermes `_put_cached_resource` — evicts the oldest 25% when full.
fn resource_cache_put(resource_id: &str, attachment: crate::messaging::MediaAttachment) {
    if resource_id.is_empty() {
        return;
    }
    let mut cache = resource_cache().lock().unwrap();
    if cache.len() >= RESOURCE_CACHE_MAX_SIZE {
        let mut entries: Vec<(String, Instant)> = cache
            .iter()
            .map(|(key, (_, cached_at))| (key.clone(), *cached_at))
            .collect();
        entries.sort_by_key(|(_, cached_at)| *cached_at);
        for (key, _) in entries.into_iter().take(RESOURCE_CACHE_MAX_SIZE / 4) {
            cache.remove(&key);
        }
    }
    cache.insert(resource_id.to_string(), (attachment, Instant::now()));
}

/// hermes `_fetch_resource_url` — exchange a resourceId for a direct
/// download URL via `/api/resource/v1/download`, retrying once on 401
/// with a forced sign-token refresh.
async fn fetch_resource_url(
    runner: &Arc<Runner>,
    resource_id: &str,
) -> std::result::Result<String, String> {
    let mut entry = runner.sign.get_token().await?;
    let bot_id = {
        let id = runner.bot_id.lock().unwrap().clone();
        if id.is_empty() {
            resolve_app_id(&runner.cfg)
        } else {
            id
        }
    };
    if entry.token.is_empty() || bot_id.is_empty() {
        return Err("missing token or bot_id for resource download".to_string());
    }
    let api_url = format!(
        "{}{}",
        resolve_api_domain(&runner.cfg),
        RESOURCE_DOWNLOAD_PATH
    );
    let client = reqwest::Client::new();
    for attempt in 0..2u32 {
        let response = client
            .get(&api_url)
            .query(&[("resourceId", resource_id)])
            .header("Content-Type", "application/json")
            .header("X-ID", &bot_id)
            .header("X-Token", &entry.token)
            .header("X-Source", &entry.source)
            .timeout(Duration::from_secs(RESOURCE_RESOLVE_TIMEOUT_SECS))
            .send()
            .await
            .map_err(|e| format!("resource/v1/download: {e}"))?;
        let status = response.status().as_u16();
        if status == 401 && attempt == 0 {
            entry = runner.sign.force_refresh().await?;
            continue;
        }
        if status >= 400 {
            let body = response.text().await.unwrap_or_default();
            return Err(format!("resource/v1/download HTTP {status}: {body}"));
        }
        let payload: Value = response
            .json()
            .await
            .map_err(|e| format!("resource/v1/download json: {e}"))?;
        if let Some(code) = payload.get("code").and_then(|v| v.as_i64()) {
            if code != 0 {
                let msg = payload.get("msg").and_then(|v| v.as_str()).unwrap_or("");
                return Err(format!(
                    "resource/v1/download failed: code={code}, msg={msg}"
                ));
            }
        }
        let data = if payload.get("data").map(|v| v.is_object()).unwrap_or(false) {
            payload.get("data").unwrap()
        } else {
            &payload
        };
        let real_url = data
            .get("url")
            .or_else(|| data.get("realUrl"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if !real_url.is_empty() {
            return Ok(real_url);
        }
        return Err("resource/v1/download missing url/realUrl".to_string());
    }
    Err("resource/v1/download did not return a URL".to_string())
}

/// hermes `_resolve_download_url` — placeholders resolve to the real
/// URL; anything else (or a failed exchange) passes through unchanged.
async fn resolve_download_url(runner: &Arc<Runner>, url: &str) -> String {
    let rid = parse_resource_id(url);
    if rid.is_empty() {
        return url.to_string();
    }
    match fetch_resource_url(runner, &rid).await {
        Ok(real_url) => real_url,
        Err(e) => {
            eprintln!("[yuanbao] resource resolve failed, using placeholder URL: {e}");
            url.to_string()
        }
    }
}

/// hermes `_download_and_cache` — fetch (size-capped) + media-cache
/// write, returning the attachment with its mime.
async fn download_and_cache_media(
    fetch_url: &str,
    reference: &MediaRef,
    resource_id: &str,
) -> Option<crate::messaging::MediaAttachment> {
    let response = match reqwest::Client::new()
        .get(fetch_url)
        .timeout(Duration::from_secs(MEDIA_DOWNLOAD_TIMEOUT_SECS))
        .send()
        .await
    {
        Ok(response) => response,
        Err(e) => {
            eprintln!(
                "[yuanbao] inbound media download failed: kind={} err={e}",
                reference.kind
            );
            return None;
        }
    };
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(e) => {
            eprintln!(
                "[yuanbao] inbound media download failed: kind={} err={e}",
                reference.kind
            );
            return None;
        }
    };
    if bytes.len() as u64 > MEDIA_MAX_SIZE_MB * 1024 * 1024 {
        eprintln!(
            "[yuanbao] inbound media too large ({} MB > {MEDIA_MAX_SIZE_MB} MB): kind={}",
            bytes.len() / (1024 * 1024),
            reference.kind
        );
        return None;
    }
    let (mime, filename_hint) = if reference.kind == "image" {
        let ext = guess_image_ext_from_url(fetch_url);
        let mut mime =
            crate::media_cache::mime_for_ext(std::path::Path::new(&format!("image{ext}")));
        if !mime.starts_with("image/") {
            mime = if content_type.starts_with("image/") {
                content_type.clone()
            } else {
                "image/jpeg".to_string()
            };
        }
        (mime, String::new())
    } else {
        let file_name = if reference.name.is_empty() {
            url_path_basename(fetch_url)
        } else {
            reference.name.clone()
        };
        let guessed = crate::media_cache::mime_for_ext(std::path::Path::new(&file_name));
        let mime = if guessed == "application/octet-stream" && !content_type.is_empty() {
            content_type.clone()
        } else {
            guessed
        };
        (mime, file_name)
    };
    let home = crate::config::ulnclaw_home();
    match crate::media_cache::cache_media_bytes(&home, &bytes, &mime, &filename_hint) {
        Ok(path) => {
            let attachment = crate::messaging::MediaAttachment {
                path,
                mime: crate::media_cache::normalize_mime(&mime),
                bytes: bytes.len() as u64,
                original_name: filename_hint,
            };
            resource_cache_put(resource_id, attachment.clone());
            Some(attachment)
        }
        Err(e) => {
            eprintln!(
                "[yuanbao] inbound media cache failed: kind={} err={e}",
                reference.kind
            );
            None
        }
    }
}

/// Resolve one ref (hermes `_resolve_one`): resource cache hit →
/// resourceId exchange → download + media-cache write.
async fn resolve_one_media(
    runner: &Arc<Runner>,
    reference: &MediaRef,
) -> Option<crate::messaging::MediaAttachment> {
    let rid = parse_resource_id(&reference.url);
    if !rid.is_empty() {
        if let Some(cached) = resource_cache_get(&rid) {
            eprintln!(
                "[yuanbao] resource cache hit: rid={rid} path={}",
                cached.path.display()
            );
            return Some(cached);
        }
    }
    let fetch_url = resolve_download_url(runner, &reference.url).await;
    download_and_cache_media(&fetch_url, reference, &rid).await
}

/// hermes `_resolve_media_urls` — order-preserving bounded-concurrency
/// resolution of all refs (hermes default concurrency 6).
async fn resolve_inbound_media(
    runner: &Arc<Runner>,
    refs: &[MediaRef],
) -> Vec<Option<crate::messaging::MediaAttachment>> {
    use futures::StreamExt;
    futures::stream::iter(refs.iter().cloned())
        .map(|reference| {
            let runner = runner.clone();
            async move { resolve_one_media(&runner, &reference).await }
        })
        .buffered(DEFAULT_RESOLVE_CONCURRENCY)
        .collect()
        .await
}

/// hermes `PatchAnchorsMiddleware._patch` — replace resolved
/// `[kind|ybres:RID]` anchors with local paths (`[image: /path]`,
/// `[file: name → /path]`). Adaptation: anchors match by resourceId
/// instead of positionally zipping the resolved list.
fn patch_media_anchors(
    text: &str,
    refs: &[MediaRef],
    resolved: &[Option<crate::messaging::MediaAttachment>],
) -> String {
    let mut patched = text.to_string();
    for (reference, attachment) in refs.iter().zip(resolved.iter()) {
        let Some(attachment) = attachment else {
            continue;
        };
        let rid = parse_resource_id(&reference.url);
        if rid.is_empty() {
            continue;
        }
        let local = attachment.path.display().to_string();
        match reference.kind {
            "image" => {
                patched = patched.replacen(
                    &format!("[image|ybres:{rid}]"),
                    &format!("[image: {local}]"),
                    1,
                );
            }
            "video" => {
                // Forwarded-record video markers (hermes PatchAnchors
                // `[video: {path}]` replacement).
                patched = patched.replacen(
                    &format!("[video|ybres:{rid}]"),
                    &format!("[video: {local}]"),
                    1,
                );
            }
            _ => {
                let label = if reference.name.is_empty() {
                    attachment
                        .path
                        .file_name()
                        .map(|name| name.to_string_lossy().to_string())
                        .unwrap_or(local.clone())
                } else {
                    reference.name.clone()
                };
                let replacement = format!("[file: {label} → {local}]");
                let named = format!("[file:{}|ybres:{rid}]", reference.name);
                // Forwarded-record file markers keep the filename OUTSIDE
                // the anchor (`[file|ybres:RID] name`) — fall back to the
                // nameless anchor when the named one is absent.
                if !reference.name.is_empty() && patched.contains(&named) {
                    patched = patched.replacen(&named, &replacement, 1);
                } else {
                    patched = patched.replacen(&format!("[file|ybres:{rid}]"), &replacement, 1);
                }
            }
        }
    }
    patched
}

// ---------------------------------------------------------------------------
// Quote context + quote/observed-media backfill (hermes
// QuoteContextMiddleware + MediaResolveMiddleware sources 2/3)
// ---------------------------------------------------------------------------

/// hermes `_extract_quote_context` — parse the `quote` object out of
/// `cloud_custom_data` JSON into `(quote_id, quote_text)` where the
/// text is `"sender: desc"` (or bare `desc` without a sender).
pub fn extract_quote_context(cloud_custom_data: &str) -> (Option<String>, Option<String>) {
    let trimmed = cloud_custom_data.trim();
    if trimmed.is_empty() {
        return (None, None);
    }
    let Ok(parsed) = serde_json::from_str::<Value>(trimmed) else {
        return (None, None);
    };
    let Some(quote) = parsed.get("quote") else {
        return (None, None);
    };
    if !quote.is_object() {
        return (None, None);
    }
    let quote_id = match quote.get("id") {
        Some(Value::String(value)) if !value.trim().is_empty() => Some(value.trim().to_string()),
        Some(Value::Number(value)) => Some(value.to_string()),
        _ => None,
    };
    let desc = quote
        .get("desc")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let sender_nick = quote
        .get("sender_nickname")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let sender = if !sender_nick.is_empty() {
        sender_nick.to_string()
    } else {
        quote
            .get("sender_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let quote_text = if desc.is_empty() {
        None
    } else if sender.is_empty() {
        Some(desc)
    } else {
        Some(format!("{sender}: {desc}"))
    };
    (quote_id, quote_text)
}

/// hermes `_YB_RES_REF_RE` scan — `(rid, kind, filename)` tuples for
/// every resolvable `[kind|ybres:RID]` anchor in the text (voice
/// anchors are excluded by `RESOLVABLE_MEDIA_KINDS`).
pub fn ybres_refs_from_text(text: &str) -> Vec<(String, String, String)> {
    let Ok(re) =
        regex::Regex::new(r"\[(image|voice|video|file(?::[^|\]]*)?)\|ybres:([A-Za-z0-9_\-]+)\]")
    else {
        return Vec::new();
    };
    let mut refs = Vec::new();
    for capture in re.captures_iter(text) {
        let head = capture.get(1).map(|g| g.as_str()).unwrap_or("");
        let rid = capture.get(2).map(|g| g.as_str()).unwrap_or("");
        if rid.is_empty() {
            continue;
        }
        let (kind, filename) = match head.split_once(':') {
            Some((kind, name)) => (kind.trim(), name.trim().to_string()),
            None => (head.trim(), String::new()),
        };
        if !RESOLVABLE_MEDIA_KINDS.contains(&kind) {
            continue;
        }
        refs.push((rid.to_string(), kind.to_string(), filename));
    }
    refs
}

/// hermes `_YB_LOCAL_MEDIA_RE` + `_collect_quote_local_media` —
/// already-local `[kind: /path]` / `[file: name → /path]` anchors
/// (patched on the original turn) whose paths still exist. No
/// re-download: unresolved anchors belong to `ybres_refs_from_text`.
pub fn local_media_from_text(text: &str) -> Vec<(std::path::PathBuf, String)> {
    let Ok(re) = regex::Regex::new(r"\[(\w+):[^\]]*?(/[^\]]+?)\s*\]") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for capture in re.captures_iter(text) {
        let kind = capture
            .get(1)
            .map(|g| g.as_str())
            .unwrap_or("")
            .to_lowercase();
        let path_str = capture.get(2).map(|g| g.as_str()).unwrap_or("").trim();
        if path_str.is_empty() {
            continue;
        }
        let path = std::path::PathBuf::from(path_str);
        if !path.exists() {
            continue;
        }
        if !seen.insert(path.to_string_lossy().to_string()) {
            continue;
        }
        let mime = crate::media_cache::mime_for_ext(&path);
        let mime = if mime == "application/octet-stream" && kind == "image" {
            "image/jpeg".to_string()
        } else {
            mime
        };
        out.push((path, mime));
    }
    out
}

/// Adapter-side message-content cache (hermes `_msg_content_cache`):
/// msg_id → patched text, FIFO-capped at 200 entries. ulnclaw
/// transcripts carry no platform message ids, so quote lookup rides
/// this cache instead of the hermes transcript scan.
fn msg_content_cache() -> &'static Mutex<std::collections::VecDeque<(String, String)>> {
    static CACHE: std::sync::OnceLock<Mutex<std::collections::VecDeque<(String, String)>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(std::collections::VecDeque::new()))
}

fn msg_content_cache_get(msg_id: &str) -> Option<String> {
    let cache = msg_content_cache().lock().unwrap();
    cache
        .iter()
        .find(|(id, _)| id == msg_id)
        .map(|(_, content)| content.clone())
}

fn msg_content_cache_put(msg_id: &str, content: &str) {
    let mut cache = msg_content_cache().lock().unwrap();
    if let Some(entry) = cache.iter_mut().find(|(id, _)| id == msg_id) {
        entry.1 = content.to_string();
        return;
    }
    cache.push_back((msg_id.to_string(), content.to_string()));
    while cache.len() > MSG_CONTENT_CACHE_MAX {
        cache.pop_front();
    }
}

/// Per-group observed-media buffer (hermes transcript window feeding
/// `_collect_observed_media`): one entry per inbound group message —
/// the message's `(rid, kind, name)` refs — capped at the lookback
/// window.
fn observed_media_buffer(
) -> &'static Mutex<HashMap<String, std::collections::VecDeque<Vec<(String, String, String)>>>> {
    static BUFFER: std::sync::OnceLock<
        Mutex<HashMap<String, std::collections::VecDeque<Vec<(String, String, String)>>>>,
    > = std::sync::OnceLock::new();
    BUFFER.get_or_init(|| Mutex::new(HashMap::new()))
}

fn record_observed(chat_id: &str, refs: &[MediaRef]) {
    let entries: Vec<(String, String, String)> = refs
        .iter()
        .map(|r| {
            (
                parse_resource_id(&r.url),
                r.kind.to_string(),
                r.name.clone(),
            )
        })
        .filter(|(rid, _, _)| !rid.is_empty())
        .collect();
    let mut buffer = observed_media_buffer().lock().unwrap();
    let queue = buffer.entry(chat_id.to_string()).or_default();
    queue.push_back(entries);
    while queue.len() > OBSERVED_MEDIA_BACKFILL_LOOKBACK {
        queue.pop_front();
    }
}

/// hermes `_collect_observed_media` walk: newest message first (inner
/// matches reversed too) so the per-turn cap keeps the *latest* media,
/// deduped by rid, then restored to chronological order.
fn collect_observed_refs(chat_id: &str) -> Vec<(String, String, String)> {
    let buffer = observed_media_buffer().lock().unwrap();
    let Some(queue) = buffer.get(chat_id) else {
        return Vec::new();
    };
    let mut order: Vec<(String, String, String)> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    'outer: for message_refs in queue.iter().rev() {
        for (rid, kind, name) in message_refs.iter().rev() {
            if !RESOLVABLE_MEDIA_KINDS.contains(&kind.as_str()) {
                continue;
            }
            if !seen.insert(rid.clone()) {
                continue;
            }
            order.push((rid.clone(), kind.clone(), name.clone()));
            if order.len() >= OBSERVED_MEDIA_BACKFILL_MAX_RESOLVE_PER_TURN {
                break 'outer;
            }
        }
    }
    order.reverse();
    order
}

/// Map a runtime kind back to the `&'static str` `MediaRef` expects.
fn static_media_kind(kind: &str) -> &'static str {
    match kind {
        "image" => "image",
        "video" => "video",
        "voice" => "voice",
        _ => "file",
    }
}

/// hermes `_resolve_ybres_refs` — resolve backfill `(rid, kind, name)`
/// tuples through the resource cache / download pipeline, skipping rids
/// already covered by the current message's own refs.
async fn resolve_backfill_refs(
    runner: &Arc<Runner>,
    refs: Vec<(String, String, String)>,
    seen_rids: &mut std::collections::HashSet<String>,
) -> Vec<crate::messaging::MediaAttachment> {
    let mut out = Vec::new();
    for (rid, kind, filename) in refs {
        if !seen_rids.insert(rid.clone()) {
            continue;
        }
        if let Some(cached) = resource_cache_get(&rid) {
            out.push(cached);
            continue;
        }
        let fetch_url = match fetch_resource_url(runner, &rid).await {
            Ok(url) => url,
            Err(e) => {
                eprintln!("[yuanbao] backfill resource resolve failed rid={rid}: {e}");
                continue;
            }
        };
        let reference = MediaRef {
            kind: static_media_kind(&kind),
            url: String::new(),
            name: filename,
        };
        if let Some(attachment) = download_and_cache_media(&fetch_url, &reference, &rid).await {
            out.push(attachment);
        }
    }
    out
}

/// hermes run.py reply-to pointer injection: `[Replying to: "..."]`
/// prefix (snippet capped at 500 chars) when both the quote id and
/// text are present. The "replying to your own message" variant is not
/// ported (ulnclaw does not track the bot's platform message ids).
pub fn render_reply_to_prefix(
    text: &str,
    quote_id: Option<&str>,
    quote_text: Option<&str>,
) -> String {
    let (Some(_), Some(quote_text)) = (quote_id, quote_text) else {
        return text.to_string();
    };
    if quote_text.trim().is_empty() {
        return text.to_string();
    }
    let snippet: String = quote_text.chars().take(QUOTE_SNIPPET_MAX_CHARS).collect();
    format!("[Replying to: \"{snippet}\"]\n\n{text}")
}

// ---------------------------------------------------------------------------
// Forwarded chat records (hermes ForwardedRecordsParseMiddleware)
// ---------------------------------------------------------------------------

/// hermes `_extract_forwarded_records` — find the first TIMCustomElem
/// with `elem_type == 1009` whose `ext_map` carries a decodable
/// `wexin_forward_msg_*` ForwardMsgData (`sub_type == 1`). Note the
/// hermes quirk: a 1009 element with an empty ext_map stops the scan.
pub(crate) fn extract_forwarded_records(
    msg_body: &[proto::MsgBodyElement],
) -> Option<proto::ForwardMsgData> {
    use base64::Engine;
    for element in msg_body {
        if element.msg_type != "TIMCustomElem" {
            continue;
        }
        let data_str = element.msg_content.data.as_str();
        if data_str.is_empty() {
            continue;
        }
        let Ok(custom) = serde_json::from_str::<Value>(data_str) else {
            continue;
        };
        if custom.get("elem_type").and_then(|v| v.as_i64()) != Some(1009) {
            continue;
        }
        if element.msg_content.ext_map.is_empty() {
            return None;
        }
        for (key, value) in &element.msg_content.ext_map {
            if !key.starts_with("wexin_forward_msg_") {
                continue;
            }
            let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(value) else {
                continue;
            };
            if let Some(forward) = proto::decode_forward_msg_data(&bytes) {
                if forward.sub_type == 1 {
                    return Some(forward);
                }
            }
        }
    }
    None
}

/// hermes `_media_marker` — render one forwarded `multimedia` entry as a
/// text marker plus (when downloadable) a media ref. `media_id` is
/// directly usable as a ybres RID (design §2.10.9); otherwise the
/// resourceId parses out of the URL.
fn forward_media_marker(
    media: &proto::ForwardMultimedia,
    plain_text: &str,
) -> (String, Option<MediaRef>) {
    let media_type = media.media_type.trim().to_lowercase();
    let url = media.url.trim().to_string();
    let media_id = media.media_id.trim().to_string();
    let file_name = media.file_name.trim().to_string();
    let rid = if !media_id.is_empty() {
        media_id
    } else {
        parse_resource_id(&url)
    };
    if media_type == "image" {
        if !url.is_empty() && !rid.is_empty() {
            return (
                format!("[image|ybres:{rid}] {file_name}")
                    .trim_end()
                    .to_string(),
                Some(MediaRef {
                    kind: "image",
                    url,
                    name: String::new(),
                }),
            );
        }
        let label = if !file_name.is_empty() {
            &file_name
        } else {
            plain_text
        };
        return (format!("[image] {label}").trim_end().to_string(), None);
    }
    if matches!(media_type.as_str(), "file" | "document" | "code") {
        if !url.is_empty() && !rid.is_empty() {
            return (
                format!("[file|ybres:{rid}] {file_name}")
                    .trim_end()
                    .to_string(),
                Some(MediaRef {
                    kind: "file",
                    url,
                    name: file_name,
                }),
            );
        }
        return (format!("[file] {file_name}").trim_end().to_string(), None);
    }
    if media_type == "url" {
        // Link share (e.g. WeChat article) — keep the URL for the agent.
        return (
            format!("[link] {file_name} {url}").trim_end().to_string(),
            None,
        );
    }
    if media_type == "video" {
        if !url.is_empty() && !rid.is_empty() {
            return (
                format!("[video|ybres:{rid}] {file_name}")
                    .trim_end()
                    .to_string(),
                Some(MediaRef {
                    kind: "video",
                    url,
                    name: String::new(),
                }),
            );
        }
        let label = if !file_name.is_empty() {
            &file_name
        } else {
            url.as_str()
        };
        return (format!("[video] {label}").trim_end().to_string(), None);
    }
    let kind_label = if media_type.is_empty() {
        "media"
    } else {
        media_type.as_str()
    };
    let label = if !url.is_empty() { &url } else { &file_name };
    (
        format!("[{kind_label}] {label}").trim_end().to_string(),
        None,
    )
}

/// hermes `_walk_forward_msgs` + `build_forward_text` (dispatch flavor):
/// body lines are `发送人：正文` with `[kind|ybres:RID]` media markers
/// preserved, refs appended in textual order for resolution/patching,
/// and a `用户附言：` footer when the forwarder added a caption.
pub(crate) fn build_forward_text(
    forward: &proto::ForwardMsgData,
    sender_nickname: &str,
    raw_text: &str,
    refs_out: &mut Vec<MediaRef>,
) -> String {
    let nickname = if sender_nickname.trim().is_empty() {
        "用户"
    } else {
        sender_nickname
    };
    let mut lines: Vec<String> = vec![
        format!("当前用户的昵称为{nickname}"),
        "以下为用户的聊天记录".to_string(),
    ];
    for msg in &forward.msg {
        let mut rendered = msg.plain_text.clone();
        if !msg.msg_content.is_empty() {
            let mut parts: Vec<String> = Vec::new();
            for content in &msg.msg_content {
                match content.content_type {
                    1 => parts.push(content.text.clone()),
                    2 => {
                        for media in &content.multimedia {
                            let (marker, reference) = forward_media_marker(media, &msg.plain_text);
                            parts.push(marker);
                            if let Some(reference) = reference {
                                refs_out.push(reference);
                            }
                        }
                    }
                    3 => parts.push("[嵌套聊天记录]".to_string()),
                    _ => {
                        if !msg.plain_text.is_empty() {
                            parts.push(msg.plain_text.clone());
                        }
                    }
                }
            }
            let joined: Vec<&str> = parts
                .iter()
                .filter(|p| !p.is_empty())
                .map(String::as_str)
                .collect();
            let combined = joined.join("  ");
            rendered = if combined.is_empty() {
                msg.plain_text.clone()
            } else {
                combined
            };
        }
        if rendered.chars().count() > FORWARD_MSG_TEXT_MAX_CHARS {
            let truncated: String = rendered.chars().take(FORWARD_MSG_TEXT_MAX_CHARS).collect();
            rendered = format!("{truncated}…(已截断)");
        }
        lines.push(format!("{}：{rendered}", msg.sender));
    }
    let mut text = lines.join("\n");
    if !raw_text.trim().is_empty() {
        text.push_str(&format!("\n\n用户附言：{}", raw_text.trim()));
    }
    text
}

// ---------------------------------------------------------------------------
// Outbound media (hermes `yuanbao_media.py` + MediaSendHandler)
// ---------------------------------------------------------------------------

/// hermes `_MIME_TO_IMAGE_FORMAT` — TIM `image_format` codes.
pub fn tim_image_format(mime_type: &str) -> u32 {
    match mime_type.to_lowercase().as_str() {
        "image/jpeg" | "image/jpg" => 1,
        "image/gif" => 2,
        "image/png" => 3,
        "image/bmp" => 4,
        _ => 255,
    }
}

/// hermes `parse_image_size` — PNG/JPEG/GIF/WebP header walk, no deps.
pub fn parse_image_size(data: &[u8]) -> Option<(u32, u32)> {
    parse_png_size(data)
        .or_else(|| parse_jpeg_size(data))
        .or_else(|| parse_gif_size(data))
        .or_else(|| parse_webp_size(data))
}

fn parse_png_size(buf: &[u8]) -> Option<(u32, u32)> {
    if buf.len() < 24 || buf[..4] != [0x89, b'P', b'N', b'G'] {
        return None;
    }
    let w = u32::from_be_bytes([buf[16], buf[17], buf[18], buf[19]]);
    let h = u32::from_be_bytes([buf[20], buf[21], buf[22], buf[23]]);
    Some((w, h))
}

fn parse_jpeg_size(buf: &[u8]) -> Option<(u32, u32)> {
    if buf.len() < 4 || buf[0] != 0xFF || buf[1] != 0xD8 {
        return None;
    }
    let mut i = 2usize;
    while i + 9 < buf.len() {
        if buf[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = buf[i + 1];
        if marker == 0xC0 || marker == 0xC2 {
            let h = u16::from_be_bytes([buf[i + 5], buf[i + 6]]) as u32;
            let w = u16::from_be_bytes([buf[i + 7], buf[i + 8]]) as u32;
            return Some((w, h));
        }
        if i + 3 < buf.len() {
            i += 2 + u16::from_be_bytes([buf[i + 2], buf[i + 3]]) as usize;
        } else {
            break;
        }
    }
    None
}

fn parse_gif_size(buf: &[u8]) -> Option<(u32, u32)> {
    if buf.len() < 10 {
        return None;
    }
    if buf[..6] != *b"GIF87a" && buf[..6] != *b"GIF89a" {
        return None;
    }
    let w = u16::from_le_bytes([buf[6], buf[7]]) as u32;
    let h = u16::from_le_bytes([buf[8], buf[9]]) as u32;
    Some((w, h))
}

fn parse_webp_size(buf: &[u8]) -> Option<(u32, u32)> {
    if buf.len() < 16 || buf[..4] != *b"RIFF" || buf[8..12] != *b"WEBP" {
        return None;
    }
    match &buf[12..16] {
        b"VP8 " => {
            if buf.len() >= 30 && buf[23] == 0x9D && buf[24] == 0x01 && buf[25] == 0x2A {
                let w = (u16::from_le_bytes([buf[26], buf[27]]) & 0x3FFF) as u32;
                let h = (u16::from_le_bytes([buf[28], buf[29]]) & 0x3FFF) as u32;
                return Some((w, h));
            }
        }
        b"VP8L" => {
            if buf.len() >= 25 && buf[20] == 0x2F {
                let bits = u32::from_le_bytes([buf[21], buf[22], buf[23], buf[24]]);
                return Some(((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1));
            }
        }
        b"VP8X" => {
            if buf.len() >= 30 {
                let w = (buf[24] as u32) | ((buf[25] as u32) << 8) | ((buf[26] as u32) << 16);
                let h = (buf[27] as u32) | ((buf[28] as u32) << 8) | ((buf[29] as u32) << 16);
                return Some((w + 1, h + 1));
            }
        }
        _ => {}
    }
    None
}

/// hermes `md5_hex` — file content digest doubles as the TIM uuid.
fn media_md5_hex(data: &[u8]) -> String {
    use md5::Digest;
    format!("{:x}", md5::Md5::digest(data))
}

/// hermes `generate_file_id` — 32 hex chars.
fn generate_file_id() -> String {
    let mut bytes = [0u8; 16];
    getrandom_fill(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// hermes `is_image` (filename/mime gate).
fn is_image_filename(filename: &str) -> bool {
    let lower = filename.to_lowercase();
    [
        ".jpg", ".jpeg", ".png", ".gif", ".webp", ".bmp", ".heic", ".tiff", ".ico",
    ]
    .iter()
    .any(|ext| lower.ends_with(ext))
}

/// Percent-encode like Python `urllib.parse.quote` — unreserved
/// ALPHA/DIGIT/`-._~` stay; `keep_slash` mirrors `safe="/"`.
fn cos_percent_encode(value: &str, keep_slash: bool) -> String {
    use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
    const BASE: &AsciiSet = &NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'.')
        .remove(b'_')
        .remove(b'~');
    if keep_slash {
        const SLASH: &AsciiSet = &BASE.remove(b'/');
        utf8_percent_encode(value, SLASH).to_string()
    } else {
        utf8_percent_encode(value, BASE).to_string()
    }
}

fn hmac_sha1_hex(key: &str, data: &str) -> String {
    let mut mac = Hmac::<sha1::Sha1>::new_from_slice(key.as_bytes()).expect("hmac key");
    mac.update(data.as_bytes());
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn sha1_hex(data: &str) -> String {
    use sha1::Digest;
    format!("{:x}", sha1::Sha1::digest(data.as_bytes()))
}

/// hermes `_cos_sign` — COS `q-sign-algorithm=sha1` Authorization value
/// (params/headers sorted by lowercased key, values percent-encoded).
pub fn cos_sign(
    method: &str,
    path: &str,
    params: &[(String, String)],
    headers: &[(String, String)],
    secret_id: &str,
    secret_key: &str,
    start_time: u64,
    expire_seconds: u64,
) -> String {
    let q_sign_time = format!("{start_time};{}", start_time + expire_seconds);
    let sign_key = hmac_sha1_hex(secret_key, &q_sign_time);

    let mut sorted_params: Vec<(String, String)> = params
        .iter()
        .map(|(k, v)| (k.to_lowercase(), cos_percent_encode(v, false)))
        .collect();
    sorted_params.sort();
    let mut sorted_headers: Vec<(String, String)> = headers
        .iter()
        .map(|(k, v)| (k.to_lowercase(), cos_percent_encode(v, false)))
        .collect();
    sorted_headers.sort();

    let url_param_list: Vec<&str> = sorted_params.iter().map(|(k, _)| k.as_str()).collect();
    let url_params: Vec<String> = sorted_params
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    let header_list: Vec<&str> = sorted_headers.iter().map(|(k, _)| k.as_str()).collect();
    let header_str: Vec<String> = sorted_headers
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();

    let http_string = format!(
        "{}\n{}\n{}\n{}\n",
        method.to_lowercase(),
        path,
        url_params.join("&"),
        header_str.join("&"),
    );
    let string_to_sign = format!("sha1\n{}\n{}\n", q_sign_time, sha1_hex(&http_string));
    let signature = hmac_sha1_hex(&sign_key, &string_to_sign);

    format!(
        "q-sign-algorithm=sha1&q-ak={secret_id}&q-sign-time={q_sign_time}&q-key-time={q_sign_time}&q-header-list={}&q-url-param-list={}&q-signature={signature}",
        header_list.join(";"),
        url_param_list.join(";"),
    )
}

/// hermes `build_image_msg_body` → TIMImageElem proto element.
pub fn image_msg_body_element(
    url: &str,
    uuid: &str,
    size: u64,
    width: u32,
    height: u32,
    mime_type: &str,
) -> proto::MsgBodyElement {
    proto::MsgBodyElement {
        msg_type: "TIMImageElem".into(),
        msg_content: proto::MsgContent {
            uuid: uuid.to_string(),
            image_format: tim_image_format(mime_type),
            image_info_array: vec![proto::ImageInfo {
                info_type: 1,
                size,
                width: width as u64,
                height: height as u64,
                url: url.to_string(),
            }],
            ..Default::default()
        },
    }
}

/// hermes `build_file_msg_body` → TIMFileElem proto element.
pub fn file_msg_body_element(
    url: &str,
    filename: &str,
    uuid: &str,
    size: u64,
) -> proto::MsgBodyElement {
    proto::MsgBodyElement {
        msg_type: "TIMFileElem".into(),
        msg_content: proto::MsgContent {
            uuid: uuid.to_string(),
            url: url.to_string(),
            file_name: filename.to_string(),
            file_size: size,
            ..Default::default()
        },
    }
}

// ---------------------------------------------------------------------------
// Outbound text (hermes MessageSender.send_text essence)
// ---------------------------------------------------------------------------

/// Paragraph-aware chunking at MAX_TEXT_CHUNK (hermes truncate_message +
/// MarkdownProcessor essentials).
pub fn chunk_markdown_text(content: &str, max: usize) -> Vec<String> {
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
            let mut len = 0usize;
            for ch in paragraph.chars() {
                if len + 1 > max {
                    chunks.push(std::mem::take(&mut piece));
                    len = 0;
                }
                piece.push(ch);
                len += 1;
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

/// Encode + send one msg_body over the WS, wait for the RPC response
/// (hermes `MessageSender.dispatch_msg_body`).
async fn send_msg_body_ws(
    runner: &Arc<Runner>,
    out_tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
    chat_id: &str,
    group_code: &str,
    elements: &[proto::MsgBodyElement],
) -> std::result::Result<(), String> {
    send_msg_body_ws_ref(runner, out_tx, chat_id, group_code, elements, "").await
}

/// `send_msg_body_ws` with an optional group quote-reply ref_msg_id.
async fn send_msg_body_ws_ref(
    runner: &Arc<Runner>,
    out_tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
    chat_id: &str,
    group_code: &str,
    elements: &[proto::MsgBodyElement],
    ref_msg_id: &str,
) -> std::result::Result<(), String> {
    let msg_id = new_uuid();
    let from_account = runner.bot_id.lock().unwrap().clone();
    let bytes = if group_code.is_empty() {
        proto::encode_send_c2c_message(chat_id, elements, &from_account, &msg_id, 0, None, "", "")
    } else {
        proto::encode_send_group_message(
            group_code,
            elements,
            &from_account,
            &msg_id,
            "",
            "",
            None,
            ref_msg_id,
            "",
        )
    };
    out_tx
        .send(bytes)
        .await
        .map_err(|e| format!("send channel closed: {e}"))?;

    let deadline = Instant::now() + Duration::from_secs(DEFAULT_SEND_TIMEOUT_SECS);
    loop {
        if Instant::now() >= deadline {
            return Err(format!(
                "yuanbao send timeout waiting for response (req_id={msg_id})"
            ));
        }
        if let Some(data) = pending_responses().lock().unwrap().remove(&msg_id) {
            // SendC2CMessageRsp/SendGroupMessageRsp: field 1 = code.
            let code = proto::decode_send_rsp_code(&data);
            if code != 0 {
                return Err(format!(
                    "yuanbao send response code={code} (req_id={msg_id})"
                ));
            }
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Encode + send one text chunk over the WS, wait for the RPC response.
async fn send_text_chunk_ws(
    runner: &Arc<Runner>,
    out_tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
    chat_id: &str,
    group_code: &str,
    chunk: &str,
) -> std::result::Result<(), String> {
    let element = proto::MsgBodyElement {
        msg_type: "TIMTextElem".into(),
        msg_content: proto::MsgContent {
            text: chunk.to_string(),
            ..Default::default()
        },
    };
    send_msg_body_ws(runner, out_tx, chat_id, group_code, &[element]).await
}

/// Send text with chunking + retries (hermes send_text semantics).
async fn send_text_via(
    runner: &Arc<Runner>,
    out_tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
    chat_id: &str,
    group_code: &str,
    content: &str,
) {
    let chunks = chunk_markdown_text(content, MAX_TEXT_CHUNK);
    for chunk in chunks {
        let mut last_error = String::new();
        for attempt in 0..3u32 {
            match send_text_chunk_ws(runner, out_tx, chat_id, group_code, &chunk).await {
                Ok(()) => {
                    last_error = String::new();
                    break;
                }
                Err(e) => {
                    last_error = e;
                    if attempt < 2 {
                        tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
                    }
                }
            }
        }
        if !last_error.is_empty() {
            eprintln!("[yuanbao] send failed to {chat_id}: {last_error}");
            return;
        }
    }
}

/// hermes `get_cos_credentials` — `genUploadInfo` temporary COS keys.
async fn get_cos_credentials(
    runner: &Arc<Runner>,
    filename: &str,
) -> std::result::Result<Value, String> {
    let entry = runner.sign.get_token().await?;
    let bot_id = runner.bot_id.lock().unwrap().clone();
    let id = if bot_id.is_empty() {
        resolve_app_id(&runner.cfg)
    } else {
        bot_id
    };
    let route_env = resolve_route_env(&runner.cfg);
    let url = format!("{}{}", resolve_api_domain(&runner.cfg), UPLOAD_INFO_PATH);
    let mut request = reqwest::Client::new()
        .post(&url)
        .header("Content-Type", "application/json")
        .header("X-Token", &entry.token)
        .header("X-ID", &id)
        .header("X-Source", "web")
        .json(&json!({
            "fileName": filename,
            "fileId": generate_file_id(),
            "docFrom": "localDoc",
            "docOpenId": "",
        }))
        .timeout(Duration::from_secs(COS_CREDS_TIMEOUT_SECS));
    if !route_env.is_empty() {
        request = request.header("X-Route-Env", &route_env);
    }
    let resp = request
        .send()
        .await
        .map_err(|e| format!("genUploadInfo: {e}"))?;
    let status = resp.status().as_u16();
    if status >= 400 {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("genUploadInfo HTTP {status}: {body}"));
    }
    let result: Value = resp
        .json()
        .await
        .map_err(|e| format!("genUploadInfo JSON: {e}"))?;
    let code = result.get("code").and_then(|v| v.as_i64());
    if let Some(code) = code {
        if code != 0 {
            let msg = result.get("msg").and_then(|v| v.as_str()).unwrap_or("");
            return Err(format!("genUploadInfo code={code}: {msg}"));
        }
    }
    let data = result
        .get("data")
        .filter(|v| v.is_object() && !v.as_object().unwrap().is_empty())
        .cloned()
        .unwrap_or(result);
    for field in ["bucketName", "location"] {
        if data
            .get(field)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .is_empty()
        {
            return Err(format!("genUploadInfo missing field {field}"));
        }
    }
    Ok(data)
}

/// hermes `upload_to_cos` — signed PUT with the temporary credentials
/// (global-accelerate host, `x-cos-security-token` session token).
async fn upload_to_cos(
    file_bytes: &[u8],
    filename: &str,
    content_type: &str,
    credentials: &Value,
) -> std::result::Result<String, String> {
    let secret_id = credentials
        .get("encryptTmpSecretId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let secret_key = credentials
        .get("encryptTmpSecretKey")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let session_token = credentials
        .get("encryptToken")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let cos_key = credentials
        .get("location")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let resource_url = credentials
        .get("resourceUrl")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let start_time = credentials.get("startTime").and_then(|v| v.as_u64());
    let expired_time = credentials.get("expiredTime").and_then(|v| v.as_u64());
    let bucket = credentials
        .get("bucketName")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if secret_id.is_empty() || secret_key.is_empty() || cos_key.is_empty() {
        return Err("COS credentials incomplete (secretId/secretKey/location)".into());
    }

    // hermes COS_USE_ACCELERATE = true.
    let cos_host = format!("{bucket}.cos.accelerate.myqcloud.com");
    let encoded_key = cos_percent_encode(&cos_key, true);
    let trimmed_key = encoded_key.trim_start_matches('/');
    let cos_url = format!("https://{cos_host}/{trimmed_key}");

    let resolved_content_type: String =
        if content_type.is_empty() || content_type == "application/octet-stream" {
            if is_image_filename(filename) {
                crate::media_cache::mime_for_ext(std::path::Path::new(filename))
            } else {
                "application/octet-stream".to_string()
            }
        } else {
            content_type.to_string()
        };
    let content_type = resolved_content_type.as_str();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let sign_start = start_time.unwrap_or(now);
    let sign_expire = if expired_time.unwrap_or(0) > now {
        expired_time.unwrap_or(0) - now
    } else {
        3600
    };
    let sign_headers = [
        ("host".to_string(), cos_host.clone()),
        ("content-type".to_string(), content_type.to_string()),
        ("x-cos-security-token".to_string(), session_token.clone()),
    ];
    let authorization = cos_sign(
        "put",
        &format!("/{trimmed_key}"),
        &[],
        &sign_headers,
        &secret_id,
        &secret_key,
        sign_start,
        sign_expire,
    );

    let resp = reqwest::Client::new()
        .put(&cos_url)
        .header("Authorization", &authorization)
        .header("Content-Type", content_type)
        .header("x-cos-security-token", &session_token)
        .body(file_bytes.to_vec())
        .timeout(Duration::from_secs(COS_UPLOAD_TIMEOUT_SECS))
        .send()
        .await
        .map_err(|e| format!("COS PUT: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("COS PUT HTTP {status}: {body}"));
    }
    Ok(if resource_url.is_empty() {
        cos_url
    } else {
        resource_url
    })
}

/// hermes MediaSendHandler.handle (local file): read → validate (≤50 MB)
/// → genUploadInfo → COS PUT → TIMImageElem/TIMFileElem (+caption
/// TIMTextElem) → WS dispatch.
async fn send_media_via(
    runner: &Arc<Runner>,
    out_tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
    chat_id: &str,
    group_code: &str,
    path: &std::path::Path,
    caption: &str,
) {
    if let Err(e) = send_media_checked(runner, out_tx, chat_id, group_code, path, caption).await {
        eprintln!("[yuanbao] media send failed for {}: {e}", path.display());
    }
}

/// `send_media_via` with a Result for tool-facing callers (P248).
async fn send_media_checked(
    runner: &Arc<Runner>,
    out_tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
    chat_id: &str,
    group_code: &str,
    path: &std::path::Path,
    caption: &str,
) -> std::result::Result<(), String> {
    let file_path = path;
    let file_bytes = std::fs::read(file_path).map_err(|e| format!("media read failed: {e}"))?;
    if file_bytes.is_empty() {
        return Err("empty media file".to_string());
    }
    if file_bytes.len() as u64 > MEDIA_MAX_SIZE_MB * 1024 * 1024 {
        return Err(format!(
            "media too large ({} MB > {MEDIA_MAX_SIZE_MB} MB)",
            file_bytes.len() / (1024 * 1024)
        ));
    }
    let filename = file_path
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    let mime_type = crate::media_cache::mime_for_ext(file_path);
    let credentials = get_cos_credentials(runner, &filename)
        .await
        .map_err(|e| format!("COS credentials failed: {e}"))?;
    let is_image = mime_type.starts_with("image/");
    let (width, height) = if is_image {
        parse_image_size(&file_bytes).unwrap_or((0, 0))
    } else {
        (0, 0)
    };
    let url = upload_to_cos(&file_bytes, &filename, &mime_type, &credentials)
        .await
        .map_err(|e| format!("COS upload failed: {e}"))?;
    let uuid = media_md5_hex(&file_bytes);
    let element = if is_image {
        image_msg_body_element(
            &url,
            &uuid,
            file_bytes.len() as u64,
            width,
            height,
            &mime_type,
        )
    } else {
        file_msg_body_element(&url, &filename, &uuid, file_bytes.len() as u64)
    };
    let mut elements = vec![element];
    if !caption.trim().is_empty() {
        elements.push(proto::MsgBodyElement {
            msg_type: "TIMTextElem".into(),
            msg_content: proto::MsgContent {
                text: caption.to_string(),
                ..Default::default()
            },
        });
    }
    send_msg_body_ws(runner, out_tx, chat_id, group_code, &elements).await
}

/// Send one sticker (hermes `StickerHandler` + `send_sticker`): fuzzy
/// catalog lookup → TIMFaceElem over the WS, three attempts like
/// `send_text_via`.
async fn send_sticker_via(
    runner: &Arc<Runner>,
    out_tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
    chat_id: &str,
    group_code: &str,
    name: &str,
) {
    let sticker = match crate::yuanbao_sticker::get_sticker_by_name(name) {
        Some(sticker) => sticker,
        None => {
            eprintln!("[yuanbao] sticker not found: {name}");
            return;
        }
    };
    let element = crate::yuanbao_sticker::build_sticker_msg_body(sticker);
    let mut last_error = String::new();
    for _attempt in 0..3u32 {
        match send_msg_body_ws(
            runner,
            out_tx,
            chat_id,
            group_code,
            std::slice::from_ref(&element),
        )
        .await
        {
            Ok(()) => return,
            Err(e) => last_error = e,
        }
    }
    if !last_error.is_empty() {
        eprintln!("[yuanbao] sticker send failed to {chat_id}: {last_error}");
    }
}

// ---------------------------------------------------------------------------
// Live adapter handle for the yb_* tools (hermes `get_active_adapter`)
// ---------------------------------------------------------------------------

/// Handle onto the running Yuanbao adapter session, used by the yb_*
/// tools (hermes `get_active_adapter`). Re-registered on every WS
/// session start.
pub struct YuanbaoHandle {
    runner: Arc<Runner>,
    out_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
}

static ACTIVE_HANDLE: std::sync::OnceLock<Mutex<Option<Arc<YuanbaoHandle>>>> =
    std::sync::OnceLock::new();

fn active_handle_slot() -> &'static Mutex<Option<Arc<YuanbaoHandle>>> {
    ACTIVE_HANDLE.get_or_init(|| Mutex::new(None))
}

/// The live adapter handle, if the yuanbao transport is running.
pub fn active_handle() -> Option<Arc<YuanbaoHandle>> {
    active_handle_slot().lock().unwrap().clone()
}

fn register_active_handle(handle: Arc<YuanbaoHandle>) {
    *active_handle_slot().lock().unwrap() = Some(handle);
}

fn clear_active_handle() {
    *active_handle_slot().lock().unwrap() = None;
}

/// Parse a tool-facing chat target: `direct:{account}` / `group:{code}`
/// / bare account id (hermes chat_id convention).
pub fn parse_chat_target(chat_id: &str) -> (String, String) {
    let trimmed = chat_id.trim();
    if let Some(rest) = trimmed.strip_prefix("direct:") {
        (rest.trim().to_string(), String::new())
    } else if let Some(rest) = trimmed.strip_prefix("group:") {
        (String::new(), rest.trim().to_string())
    } else {
        (trimmed.to_string(), String::new())
    }
}

/// Resolve a sticker by id, name, or random when empty (hermes
/// `send_sticker` lookup order).
fn resolve_sticker(
    raw: &str,
) -> std::result::Result<&'static crate::yuanbao_sticker::Sticker, String> {
    if raw.is_empty() {
        return Ok(crate::yuanbao_sticker::get_random_sticker(None));
    }
    if raw.chars().all(|c| c.is_ascii_digit()) {
        if let Some(sticker) = crate::yuanbao_sticker::get_sticker_by_id(raw) {
            return Ok(sticker);
        }
    }
    crate::yuanbao_sticker::get_sticker_by_name(raw).ok_or_else(|| {
        format!(
            "Sticker not found: '{raw}'. Use yb_search_sticker first to discover available stickers."
        )
    })
}

impl YuanbaoHandle {
    /// WS session liveness (tools gate on this).
    pub fn is_connected(&self) -> bool {
        self.runner.connected.load(Ordering::SeqCst)
    }

    /// Send one encoded biz request and await the correlated response
    /// payload (hermes `send_biz_request`).
    pub async fn send_biz_request(
        &self,
        bytes: Vec<u8>,
        req_id: &str,
    ) -> std::result::Result<Vec<u8>, String> {
        self.out_tx
            .send(bytes)
            .await
            .map_err(|e| format!("send channel closed: {e}"))?;
        let deadline = Instant::now() + Duration::from_secs(DEFAULT_SEND_TIMEOUT_SECS);
        loop {
            if Instant::now() >= deadline {
                return Err(format!("yuanbao biz request timeout (req_id={req_id})"));
            }
            if let Some(data) = pending_responses().lock().unwrap().remove(req_id) {
                return Ok(data);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Group name/owner/member count (hermes `query_group_info_raw`).
    pub async fn query_group_info(
        &self,
        group_code: &str,
    ) -> std::result::Result<proto::QueryGroupInfoRsp, String> {
        let req_id = format!("qgi_{}", proto::next_seq_no());
        let bytes = proto::encode_query_group_info(group_code, &req_id);
        let data = self.send_biz_request(bytes, &req_id).await?;
        Ok(proto::decode_query_group_info_rsp(&data))
    }

    /// Group member list (hermes `get_group_member_list_raw`).
    pub async fn get_group_member_list(
        &self,
        group_code: &str,
    ) -> std::result::Result<proto::MemberListRsp, String> {
        let req_id = format!("gml_{}", proto::next_seq_no());
        let bytes = proto::encode_get_group_member_list(group_code, 0, 200, &req_id);
        let data = self.send_biz_request(bytes, &req_id).await?;
        Ok(proto::decode_get_group_member_list_rsp(&data))
    }

    /// DM text to a member (hermes `adapter.send_dm`) — a C2C message
    /// carrying the source group context, chunked like send_text_via.
    pub async fn send_dm(
        &self,
        user_id: &str,
        message: &str,
        group_code: &str,
    ) -> std::result::Result<(), String> {
        let from_account = self.runner.bot_id.lock().unwrap().clone();
        for chunk in chunk_markdown_text(message, MAX_TEXT_CHUNK) {
            let element = proto::MsgBodyElement {
                msg_type: "TIMTextElem".into(),
                msg_content: proto::MsgContent {
                    text: chunk.clone(),
                    ..Default::default()
                },
            };
            let msg_id = new_uuid();
            let bytes = proto::encode_send_c2c_message(
                user_id,
                &[element],
                &from_account,
                &msg_id,
                0,
                None,
                group_code,
                "",
            );
            self.out_tx
                .send(bytes)
                .await
                .map_err(|e| format!("send channel closed: {e}"))?;
            let deadline = Instant::now() + Duration::from_secs(DEFAULT_SEND_TIMEOUT_SECS);
            loop {
                if Instant::now() >= deadline {
                    return Err(format!("yuanbao DM timeout (req_id={msg_id})"));
                }
                if let Some(data) = pending_responses().lock().unwrap().remove(&msg_id) {
                    let code = proto::decode_send_rsp_code(&data);
                    if code != 0 {
                        return Err(format!("yuanbao DM response code={code} (req_id={msg_id})"));
                    }
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
        Ok(())
    }

    /// Send a built-in sticker (hermes `adapter.send_sticker`). Returns
    /// `(sticker_id, name)` of the delivered sticker.
    pub async fn send_sticker(
        &self,
        chat_id: &str,
        sticker_name: &str,
        reply_to: Option<&str>,
    ) -> std::result::Result<(String, String), String> {
        let (target_chat, target_group) = parse_chat_target(chat_id);
        let sticker = resolve_sticker(sticker_name)?;
        let element = crate::yuanbao_sticker::build_sticker_msg_body(sticker);
        send_msg_body_ws_ref(
            &self.runner,
            &self.out_tx,
            &target_chat,
            &target_group,
            std::slice::from_ref(&element),
            reply_to.unwrap_or(""),
        )
        .await?;
        Ok((sticker.sticker_id.to_string(), sticker.name.to_string()))
    }

    /// Upload + send a local media file (hermes `send_image_file` /
    /// `send_document` — type chosen by mime).
    pub async fn send_media(
        &self,
        chat_id: &str,
        path: &std::path::Path,
    ) -> std::result::Result<(), String> {
        let (target_chat, target_group) = parse_chat_target(chat_id);
        send_media_checked(
            &self.runner,
            &self.out_tx,
            &target_chat,
            &target_group,
            path,
            "",
        )
        .await
    }
}

// ---------------------------------------------------------------------------
// PlatformSender (clarify gateway / webhook delivery)
// ---------------------------------------------------------------------------

struct YuanbaoSender {
    runner: Arc<Runner>,
    ws_url: String,
}

#[async_trait::async_trait]
impl crate::messaging::PlatformSender for YuanbaoSender {
    async fn send_text(&self, chat_id: &str, text: &str) {
        // Out-of-session sends open a short-lived WS with the same auth
        // flow (hermes keeps one connection; ulnclaw reuses the live one
        // through the clarify task-local when available, else connects).
        send_direct(&self.runner, &self.ws_url, chat_id, text).await;
    }
}

fn register_sender(runner: Arc<Runner>, ws_url: String) {
    crate::messaging::register_platform_sender(
        "yuanbao",
        Arc::new(YuanbaoSender { runner, ws_url }),
    );
}

/// One-shot direct send (own ephemeral session).
async fn send_direct(runner: &Arc<Runner>, ws_url: &str, chat_id: &str, text: &str) {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    let Ok(token_entry) = runner.sign.get_token().await else {
        eprintln!("[yuanbao] direct send: sign-token failed");
        return;
    };
    if !token_entry.bot_id.is_empty() {
        *runner.bot_id.lock().unwrap() = token_entry.bot_id.clone();
    }
    let Ok(Ok((ws, _))) = tokio::time::timeout(
        Duration::from_secs(CONNECT_TIMEOUT_SECS + 10),
        tokio_tungstenite::connect_async(ws_url),
    )
    .await
    else {
        eprintln!("[yuanbao] direct send: WS connect failed");
        return;
    };
    let (mut sink, mut stream) = ws.split();
    let auth_msg_id = new_uuid();
    let auth_bytes = proto::encode_auth_bind(
        "ybBot",
        &runner.bot_id.lock().unwrap().clone(),
        if token_entry.source.is_empty() {
            "bot"
        } else {
            &token_entry.source
        },
        &token_entry.token,
        &auth_msg_id,
        &app_version(),
        std::env::consts::OS,
        &app_version(),
        &resolve_route_env(&runner.cfg),
    );
    if sink.send(WsMessage::Binary(auth_bytes)).await.is_err() {
        return;
    }
    // Wait for BIND_ACK.
    let deadline = Instant::now() + Duration::from_secs(AUTH_TIMEOUT_SECS);
    loop {
        if Instant::now() >= deadline {
            return;
        }
        let Ok(Some(Ok(message))) = tokio::time::timeout(
            deadline.saturating_duration_since(Instant::now()),
            stream.next(),
        )
        .await
        else {
            return;
        };
        let WsMessage::Binary(raw) = message else {
            continue;
        };
        let msg = proto::decode_conn_msg(&raw);
        if msg.head.cmd_type == proto::CMD_TYPE_RESPONSE && msg.head.cmd == proto::CMD_AUTH_BIND {
            if proto::decode_auth_bind_rsp(&msg.data).is_err() {
                return;
            }
            break;
        }
    }
    let msg_id = new_uuid();
    let from_account = runner.bot_id.lock().unwrap().clone();
    let element = proto::MsgBodyElement {
        msg_type: "TIMTextElem".into(),
        msg_content: proto::MsgContent {
            text: text.to_string(),
            ..Default::default()
        },
    };
    let bytes = proto::encode_send_c2c_message(
        chat_id,
        &[element],
        &from_account,
        &msg_id,
        0,
        None,
        "",
        "",
    );
    sink.send(WsMessage::Binary(bytes)).await.ok();
    // Give the server a moment to ack before closing.
    tokio::time::sleep(Duration::from_secs(1)).await;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_matches_hmac_sha256_vector() {
        // HMAC-SHA256(key=app_secret, msg=nonce+timestamp+app_key+app_secret)
        let signature =
            compute_signature("abc", "2026-08-06T10:00:00+08:00", "appkey", "secret123");
        assert_eq!(
            signature,
            "0806db561aa469139a2a2c249268026bbf9ea4304331590db060496a2ddc9999"
        );
    }

    #[test]
    fn timestamp_is_beijing_iso8601() {
        let ts = build_timestamp();
        assert!(
            regex::Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\+08:00$")
                .unwrap()
                .is_match(&ts),
            "unexpected timestamp: {ts}"
        );
    }

    #[test]
    fn chunk_markdown_respects_limit() {
        assert_eq!(chunk_markdown_text("short", MAX_TEXT_CHUNK), vec!["short"]);

        let mut long = String::new();
        for i in 0..100 {
            long.push_str(&format!("Paragraph {i} with body text.\n\n"));
        }
        let chunks = chunk_markdown_text(&long, 400);
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            assert!(chunk.chars().count() <= 400);
        }

        let giant = "x".repeat(MAX_TEXT_CHUNK * 2 + 10);
        let chunks = chunk_markdown_text(&giant, MAX_TEXT_CHUNK);
        assert_eq!(chunks.len(), 3);
    }

    #[test]
    fn config_resolution_prefers_explicit_values() {
        let mut cfg = YuanbaoConfig::default();
        cfg.app_id = "cfg-app".into();
        cfg.api_domain = "https://example.com/".into();
        assert_eq!(resolve_app_id(&cfg), "cfg-app");
        assert_eq!(resolve_api_domain(&cfg), "https://example.com");
        // Defaults fall through.
        let empty = YuanbaoConfig::default();
        assert_eq!(resolve_api_domain(&empty), DEFAULT_API_DOMAIN);
        assert!(resolve_ws_url(&empty).is_empty());
    }

    #[test]
    fn uuid_shape() {
        let id = new_uuid();
        assert_eq!(id.len(), 36);
        assert_eq!(id.matches('-').count(), 4);
        assert_ne!(new_uuid(), id);
    }

    #[test]
    fn dedup_flags_repeats() {
        let mut seen: HashMap<String, Instant> = HashMap::new();
        assert!(!dedup_check(&mut seen, "k1"));
        assert!(dedup_check(&mut seen, "k1"));
        assert!(!dedup_check(&mut seen, "k2"));
        // Expired entries pass again.
        seen.insert("k3".into(), Instant::now() - Duration::from_secs(301));
        assert!(!dedup_check(&mut seen, "k3"));
    }

    #[test]
    fn tim_image_format_mapping_matches_hermes() {
        assert_eq!(tim_image_format("image/jpeg"), 1);
        assert_eq!(tim_image_format("image/jpg"), 1);
        assert_eq!(tim_image_format("image/gif"), 2);
        assert_eq!(tim_image_format("image/png"), 3);
        assert_eq!(tim_image_format("image/bmp"), 4);
        assert_eq!(tim_image_format("image/webp"), 255);
        assert_eq!(tim_image_format("application/pdf"), 255);
    }

    #[test]
    fn parse_image_size_recognizes_formats() {
        // PNG: signature + IHDR, width 300 / height 200 big-endian.
        let mut png = vec![0x89, b'P', b'N', b'G', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        png.extend_from_slice(&300u32.to_be_bytes());
        png.extend_from_slice(&200u32.to_be_bytes());
        assert_eq!(parse_image_size(&png), Some((300, 200)));

        // GIF89a: little-endian u16 pair.
        let mut gif = b"GIF89a".to_vec();
        gif.extend_from_slice(&300u16.to_le_bytes());
        gif.extend_from_slice(&200u16.to_le_bytes());
        assert_eq!(parse_image_size(&gif), Some((300, 200)));

        // JPEG: SOF0 marker, height then width big-endian.
        let jpeg = [
            0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x11, 0x08, 0x00, 0xF0, 0x01, 0x40, 0x00,
        ];
        assert_eq!(parse_image_size(&jpeg), Some((320, 240)));

        // WebP VP8L: 14-bit width/height minus one.
        let mut webp = b"RIFF".to_vec();
        webp.extend_from_slice(&[0, 0, 0, 0]);
        webp.extend_from_slice(b"WEBPVP8L");
        webp.extend_from_slice(&[0, 0, 0, 0]);
        webp.push(0x2F);
        let bits: u32 = 100 | (200 << 14); // w=101, h=201
        webp.extend_from_slice(&bits.to_le_bytes());
        assert_eq!(parse_image_size(&webp), Some((101, 201)));

        assert_eq!(parse_image_size(&[0u8; 8]), None);
    }

    #[test]
    fn cos_sign_matches_python_reference_vector() {
        // Computed with hermes `_cos_sign` (Python hmac/hashlib/quote).
        let auth = cos_sign(
            "put",
            "/chatbot-upload/test.png",
            &[],
            &[
                (
                    "host".to_string(),
                    "chatbot-123.cos.accelerate.myqcloud.com".to_string(),
                ),
                ("content-type".to_string(), "image/png".to_string()),
                ("x-cos-security-token".to_string(), "tok123".to_string()),
            ],
            "AKIDtest",
            "secrettest",
            1700000000,
            3600,
        );
        assert_eq!(
            auth,
            "q-sign-algorithm=sha1&q-ak=AKIDtest&q-sign-time=1700000000;1700003600&q-key-time=1700000000;1700003600&q-header-list=content-type;host;x-cos-security-token&q-url-param-list=&q-signature=f174a0b22b925e88759e910c5bb9364c77071e62"
        );
    }

    #[test]
    fn media_msg_body_elements_match_hermes_layout() {
        let image = image_msg_body_element(
            "https://cos.example/img.png",
            "md5uuid",
            1234,
            300,
            200,
            "image/png",
        );
        assert_eq!(image.msg_type, "TIMImageElem");
        assert_eq!(image.msg_content.uuid, "md5uuid");
        assert_eq!(image.msg_content.image_format, 3);
        assert_eq!(image.msg_content.image_info_array.len(), 1);
        let info = &image.msg_content.image_info_array[0];
        assert_eq!(info.info_type, 1);
        assert_eq!(info.size, 1234);
        assert_eq!(info.width, 300);
        assert_eq!(info.height, 200);
        assert_eq!(info.url, "https://cos.example/img.png");

        let file = file_msg_body_element("https://cos.example/a.pdf", "a.pdf", "md5uuid", 999);
        assert_eq!(file.msg_type, "TIMFileElem");
        assert_eq!(file.msg_content.file_name, "a.pdf");
        assert_eq!(file.msg_content.file_size, 999);
        assert_eq!(file.msg_content.url, "https://cos.example/a.pdf");

        // Proto round-trip keeps the image array intact.
        let element = proto::encode_msg_body_element(&image);
        let decoded = proto::decode_msg_body_element(&element);
        assert_eq!(decoded.msg_content.image_info_array.len(), 1);
        assert_eq!(
            decoded.msg_content.image_info_array[0].url,
            "https://cos.example/img.png"
        );
        assert_eq!(decoded.msg_content.image_format, 3);
    }

    #[test]
    fn generate_file_id_is_32_hex_chars() {
        let id = generate_file_id();
        assert_eq!(id.len(), 32);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(id, generate_file_id());
    }

    #[test]
    fn parse_resource_id_variants() {
        assert_eq!(
            parse_resource_id(
                "https://hunyuan.tencent.com/api/resource/download?resourceId=abc123"
            ),
            "abc123"
        );
        assert_eq!(
            parse_resource_id("https://cdn.example/x.png?resourceid=low-1&x=1"),
            "low-1"
        );
        assert_eq!(parse_resource_id("https://cdn.example/x.png?other=1"), "");
        assert_eq!(parse_resource_id("not a url"), "");
        assert_eq!(parse_resource_id(""), "");
    }

    #[test]
    fn extract_inbound_text_renders_anchors_and_refs() {
        let msg_body = vec![
            proto::MsgBodyElement {
                msg_type: "TIMTextElem".into(),
                msg_content: proto::MsgContent {
                    text: "看这个".into(),
                    ..Default::default()
                },
            },
            proto::MsgBodyElement {
                msg_type: "TIMImageElem".into(),
                msg_content: proto::MsgContent {
                    image_info_array: vec![
                        proto::ImageInfo {
                            url: "https://cdn.example/small?resourceId=small-1".into(),
                            ..Default::default()
                        },
                        proto::ImageInfo {
                            url: "https://cdn.example/medium?resourceId=abc123".into(),
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                },
            },
            proto::MsgBodyElement {
                msg_type: "TIMImageElem".into(),
                msg_content: proto::MsgContent::default(),
            },
            proto::MsgBodyElement {
                msg_type: "TIMFileElem".into(),
                msg_content: proto::MsgContent {
                    url: "https://cdn.example/f?resourceId=def456".into(),
                    file_name: "a.pdf".into(),
                    ..Default::default()
                },
            },
            proto::MsgBodyElement {
                msg_type: "TIMSoundElem".into(),
                msg_content: proto::MsgContent {
                    url: "https://cdn.example/v?resourceId=snd1".into(),
                    ..Default::default()
                },
            },
            proto::MsgBodyElement {
                msg_type: "TIMVideoFileElem".into(),
                msg_content: proto::MsgContent::default(),
            },
            proto::MsgBodyElement {
                msg_type: "TIMCustomElem".into(),
                msg_content: proto::MsgContent::default(),
            },
        ];
        let (text, refs) = extract_inbound_text(&msg_body);
        assert_eq!(
            text,
            "看这个\n[image|ybres:abc123]\n[image]\n[file:a.pdf|ybres:def456]\n[voice|ybres:snd1]\n[video]\n[media: TIMCustomElem]"
        );
        // hermes: image (medium preferred) + file refs only.
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].kind, "image");
        assert_eq!(refs[0].url, "https://cdn.example/medium?resourceId=abc123");
        assert_eq!(refs[1].kind, "file");
        assert_eq!(refs[1].url, "https://cdn.example/f?resourceId=def456");
        assert_eq!(refs[1].name, "a.pdf");
    }

    #[test]
    fn skippable_placeholders_match_hermes() {
        assert_eq!(
            SKIPPABLE_PLACEHOLDERS,
            &[
                "[image]", "[图片]", "[file]", "[文件]", "[video]", "[视频]", "[voice]", "[语音]"
            ]
        );
    }

    #[test]
    fn guess_image_ext_from_url_variants() {
        assert_eq!(
            guess_image_ext_from_url("https://cdn.example/a/b.PNG"),
            ".png"
        );
        assert_eq!(
            guess_image_ext_from_url("https://cdn.example/a/b.webp?x=1"),
            ".webp"
        );
        assert_eq!(
            guess_image_ext_from_url("https://cdn.example/a/b.exe"),
            ".jpg"
        );
        assert_eq!(guess_image_ext_from_url("https://cdn.example/a/b"), ".jpg");
    }

    #[test]
    fn patch_media_anchors_rewrites_resolved() {
        let temp = tempfile::tempdir().unwrap();
        let image_path = temp.path().join("img.png");
        let file_path = temp.path().join("doc.pdf");
        std::fs::write(&image_path, b"png").unwrap();
        std::fs::write(&file_path, b"pdf").unwrap();
        let refs = vec![
            MediaRef {
                kind: "image",
                url: "https://cdn.example/medium?resourceId=abc123".into(),
                name: String::new(),
            },
            MediaRef {
                kind: "file",
                url: "https://cdn.example/f?resourceId=def456".into(),
                name: "a.pdf".into(),
            },
            MediaRef {
                kind: "image",
                url: "https://cdn.example/x?resourceId=failed1".into(),
                name: String::new(),
            },
        ];
        let resolved = vec![
            Some(crate::messaging::MediaAttachment {
                path: image_path.clone(),
                mime: "image/png".into(),
                bytes: 3,
                original_name: String::new(),
            }),
            Some(crate::messaging::MediaAttachment {
                path: file_path.clone(),
                mime: "application/pdf".into(),
                bytes: 3,
                original_name: "a.pdf".into(),
            }),
            None,
        ];
        let text = "看图\n[image|ybres:abc123]\n[file:a.pdf|ybres:def456]\n[image|ybres:failed1]";
        let patched = patch_media_anchors(text, &refs, &resolved);
        assert!(patched.contains(&format!("[image: {}]", image_path.display())));
        assert!(patched.contains(&format!("[file: a.pdf → {}]", file_path.display())));
        // Failed resolution leaves the anchor untouched.
        assert!(patched.contains("[image|ybres:failed1]"));
    }

    #[test]
    fn resource_cache_evicts_and_invalidates() {
        let temp = tempfile::tempdir().unwrap();
        let file_path = temp.path().join("cached.bin");
        std::fs::write(&file_path, b"data").unwrap();
        let make = || crate::messaging::MediaAttachment {
            path: file_path.clone(),
            mime: "image/jpeg".into(),
            bytes: 4,
            original_name: String::new(),
        };
        for index in 0..(RESOURCE_CACHE_MAX_SIZE + 1) {
            resource_cache_put(&format!("evict-{index}"), make());
        }
        assert!(resource_cache().lock().unwrap().len() <= RESOURCE_CACHE_MAX_SIZE);

        resource_cache_put("invalidate-me", make());
        assert!(resource_cache_get("invalidate-me").is_some());
        std::fs::remove_file(&file_path).unwrap();
        // Cached file vanished on disk → entry invalidated.
        assert!(resource_cache_get("invalidate-me").is_none());
    }

    #[test]
    fn quote_context_extraction_variants() {
        // Full quote: id + sender nickname + desc.
        let data =
            r#"{"quote": {"id": "msg-123", "desc": "look at this", "sender_nickname": "Alice"}}"#;
        assert_eq!(
            extract_quote_context(data),
            (
                Some("msg-123".to_string()),
                Some("Alice: look at this".to_string())
            )
        );
        // sender_id fallback, no nickname.
        let data = r#"{"quote": {"id": 42, "desc": "hi", "sender_id": "u9"}}"#;
        assert_eq!(
            extract_quote_context(data),
            (Some("42".to_string()), Some("u9: hi".to_string()))
        );
        // Desc only (no sender).
        let data = r#"{"quote": {"id": "m1", "desc": "bare"}}"#;
        assert_eq!(
            extract_quote_context(data),
            (Some("m1".to_string()), Some("bare".to_string()))
        );
        // Id without desc → no quote text.
        assert_eq!(
            extract_quote_context(r#"{"quote": {"id": "m2"}}"#),
            (Some("m2".to_string()), None)
        );
        // No quote object / malformed JSON / empty → (None, None).
        assert_eq!(extract_quote_context(r#"{"other": 1}"#), (None, None));
        assert_eq!(extract_quote_context("not json"), (None, None));
        assert_eq!(extract_quote_context(""), (None, None));
    }

    #[test]
    fn ybres_refs_extraction_filters_kinds() {
        let text = "pic [image|ybres:rid-1] doc [file:report.pdf|ybres:rid_2] \
                    clip [video|ybres:rid-3] note [voice|ybres:rid-4] bare [image]";
        let refs = ybres_refs_from_text(text);
        assert_eq!(refs.len(), 3);
        assert_eq!(
            refs[0],
            ("rid-1".to_string(), "image".to_string(), String::new())
        );
        assert_eq!(
            refs[1],
            (
                "rid_2".to_string(),
                "file".to_string(),
                "report.pdf".to_string()
            )
        );
        assert_eq!(
            refs[2],
            ("rid-3".to_string(), "video".to_string(), String::new())
        );
        assert!(ybres_refs_from_text("no anchors here").is_empty());
    }

    #[test]
    fn local_media_extraction_uses_existing_paths() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("pic.jpg");
        let file = dir.path().join("report.pdf");
        std::fs::write(&image, b"img").unwrap();
        std::fs::write(&file, b"doc").unwrap();
        let text = format!(
            "[image: {}] [file: report.pdf → {}] [image: /does/not/exist.png] [image: {}]",
            image.display(),
            file.display(),
            image.display()
        );
        let found = local_media_from_text(&text);
        // Missing path filtered; duplicate path deduped.
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].0, image);
        assert!(found[0].1.starts_with("image/"));
        assert_eq!(found[1].0, file);
        assert_eq!(found[1].1, "application/pdf");
    }

    #[test]
    fn msg_content_cache_fifo_and_update() {
        let prefix = format!("p209-cache-{}", std::process::id());
        for i in 0..MSG_CONTENT_CACHE_MAX {
            msg_content_cache_put(&format!("{prefix}-{i}"), &format!("content-{i}"));
        }
        assert_eq!(
            msg_content_cache_get(&format!("{prefix}-0")),
            Some("content-0".to_string())
        );
        // One more entry evicts the oldest.
        msg_content_cache_put(&format!("{prefix}-new"), "newest");
        assert_eq!(msg_content_cache_get(&format!("{prefix}-0")), None);
        assert_eq!(
            msg_content_cache_get(&format!("{prefix}-{}", MSG_CONTENT_CACHE_MAX - 1)),
            Some(format!("content-{}", MSG_CONTENT_CACHE_MAX - 1))
        );
        assert_eq!(
            msg_content_cache_get(&format!("{prefix}-new")),
            Some("newest".to_string())
        );
        // Update in place keeps the entry.
        msg_content_cache_put(&format!("{prefix}-new"), "updated");
        assert_eq!(
            msg_content_cache_get(&format!("{prefix}-new")),
            Some("updated".to_string())
        );
    }

    #[test]
    fn observed_backfill_window_dedup_cap_and_order() {
        let chat = format!("p209-group-{}", std::process::id());
        let mk = |kind: &str, rid: &str, name: &str| MediaRef {
            kind: match kind {
                "image" => "image",
                "video" => "video",
                "voice" => "voice",
                _ => "file",
            },
            url: format!("https://example.com/r?resourceId={rid}"),
            name: name.to_string(),
        };
        // Three messages: oldest has rid-a (image) + rid-b (file); middle
        // re-sends rid-a plus rid-c (voice — unresolvable); newest rid-d.
        record_observed(
            &chat,
            &[mk("image", "rid-a", ""), mk("file", "rid-b", "b.pdf")],
        );
        record_observed(&chat, &[mk("image", "rid-a", ""), mk("voice", "rid-c", "")]);
        record_observed(&chat, &[mk("video", "rid-d", "")]);
        let refs = collect_observed_refs(&chat);
        // Newest-first walk discovers rid-d, then rid-a (via the middle
        // message, deduping its older copy), then rid-b; voice rid-c is
        // dropped. Reversing yields the hermes order: rid-b, rid-a, rid-d.
        let rids: Vec<&str> = refs.iter().map(|(rid, _, _)| rid.as_str()).collect();
        assert_eq!(rids, vec!["rid-b", "rid-a", "rid-d"]);
        assert_eq!(refs[0].2, "b.pdf");
        // Unknown chats yield nothing.
        assert!(collect_observed_refs("p209-unknown-group").is_empty());
    }

    #[test]
    fn reply_to_prefix_rendering() {
        let text = render_reply_to_prefix("hello", Some("m1"), Some("quoted words"));
        assert_eq!(text, "[Replying to: \"quoted words\"]\n\nhello");
        // Missing id or text → unchanged; blank text → unchanged.
        assert_eq!(render_reply_to_prefix("hello", None, Some("q")), "hello");
        assert_eq!(render_reply_to_prefix("hello", Some("m1"), None), "hello");
        assert_eq!(
            render_reply_to_prefix("hello", Some("m1"), Some("  ")),
            "hello"
        );
        // Snippet truncates at 500 chars.
        let long = "x".repeat(600);
        let rendered = render_reply_to_prefix("hi", Some("m1"), Some(&long));
        assert!(rendered.contains(&"x".repeat(500)));
        assert!(!rendered.contains(&"x".repeat(501)));
    }

    fn sample_forward_data() -> proto::ForwardMsgData {
        proto::ForwardMsgData {
            sub_type: 1,
            begin_time: 100,
            end_time: 200,
            nick_name: "转发者".into(),
            msg: vec![
                proto::ForwardMsg {
                    sender: "Alice".into(),
                    time: 1,
                    plain_text: "hello".into(),
                    msg_content: vec![proto::ForwardMsgContent {
                        content_type: 1,
                        text: "hello world".into(),
                        multimedia: Vec::new(),
                    }],
                },
                proto::ForwardMsg {
                    sender: "Bob".into(),
                    time: 2,
                    msg_content: vec![proto::ForwardMsgContent {
                        content_type: 2,
                        multimedia: vec![proto::ForwardMultimedia {
                            media_type: "image".into(),
                            url: "https://cdn.example/r?resourceId=fw-rid-1".into(),
                            file_name: "pic.jpg".into(),
                            ..Default::default()
                        }],
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            ],
        }
    }

    fn custom_elem_with_forward(
        forward: &proto::ForwardMsgData,
        sub_type_override: Option<u64>,
    ) -> proto::MsgBodyElement {
        use base64::Engine;
        let mut data = forward.clone();
        if let Some(sub) = sub_type_override {
            data.sub_type = sub;
        }
        let payload =
            base64::engine::general_purpose::STANDARD.encode(proto::encode_forward_msg_data(&data));
        proto::MsgBodyElement {
            msg_type: "TIMCustomElem".into(),
            msg_content: proto::MsgContent {
                data: json!({"elem_type": 1009}).to_string(),
                ext_map: vec![("wexin_forward_msg_abc_u1".into(), payload)],
                ..Default::default()
            },
        }
    }

    #[test]
    fn forward_msg_data_round_trips() {
        let data = sample_forward_data();
        let encoded = proto::encode_forward_msg_data(&data);
        assert_eq!(proto::decode_forward_msg_data(&encoded), Some(data));
        assert_eq!(proto::decode_forward_msg_data(&[]), None);
    }

    #[test]
    fn extract_forwarded_records_variants() {
        let forward = sample_forward_data();
        let body = vec![custom_elem_with_forward(&forward, None)];
        let found = extract_forwarded_records(&body).unwrap();
        assert_eq!(found.sub_type, 1);
        assert_eq!(found.msg.len(), 2);
        // Wrong sub_type → None.
        let body = vec![custom_elem_with_forward(&forward, Some(2))];
        assert!(extract_forwarded_records(&body).is_none());
        // Non-1009 custom elem → None.
        let other = proto::MsgBodyElement {
            msg_type: "TIMCustomElem".into(),
            msg_content: proto::MsgContent {
                data: json!({"elem_type": 42}).to_string(),
                ..Default::default()
            },
        };
        assert!(extract_forwarded_records(&[other]).is_none());
        // 1009 with empty ext_map stops the scan (hermes quirk) even
        // when a valid element follows.
        let empty_ext = proto::MsgBodyElement {
            msg_type: "TIMCustomElem".into(),
            msg_content: proto::MsgContent {
                data: json!({"elem_type": 1009}).to_string(),
                ..Default::default()
            },
        };
        let body = vec![empty_ext, custom_elem_with_forward(&forward, None)];
        assert!(extract_forwarded_records(&body).is_none());
    }

    #[test]
    fn forward_media_marker_variants() {
        // Image with usable RID — media_id wins over URL parsing.
        let media = proto::ForwardMultimedia {
            media_type: "image".into(),
            url: "https://cdn/r?resourceId=x".into(),
            file_name: "p.jpg".into(),
            media_id: "mid-9".into(),
            ..Default::default()
        };
        let (marker, reference) = forward_media_marker(&media, "");
        assert_eq!(marker, "[image|ybres:mid-9] p.jpg");
        assert_eq!(reference.unwrap().kind, "image");
        // Document with filename.
        let media = proto::ForwardMultimedia {
            media_type: "document".into(),
            url: "https://cdn/r?resourceId=f1".into(),
            file_name: "r.pdf".into(),
            ..Default::default()
        };
        let (marker, reference) = forward_media_marker(&media, "");
        assert_eq!(marker, "[file|ybres:f1] r.pdf");
        assert_eq!(reference.unwrap().name, "r.pdf");
        // Link share keeps the URL, no downloadable ref.
        let media = proto::ForwardMultimedia {
            media_type: "url".into(),
            url: "https://mp.weixin.qq.com/s/x".into(),
            file_name: "文章".into(),
            ..Default::default()
        };
        let (marker, reference) = forward_media_marker(&media, "");
        assert_eq!(marker, "[link] 文章 https://mp.weixin.qq.com/s/x");
        assert!(reference.is_none());
        // RID-less image → plain marker with plain_text fallback.
        let media = proto::ForwardMultimedia {
            media_type: "image".into(),
            url: "https://cdn/no-rid".into(),
            ..Default::default()
        };
        let (marker, reference) = forward_media_marker(&media, "caption");
        assert_eq!(marker, "[image] caption");
        assert!(reference.is_none());
        // Video marker + ref.
        let media = proto::ForwardMultimedia {
            media_type: "video".into(),
            url: "https://cdn/r?resourceId=v1".into(),
            file_name: "c.mp4".into(),
            ..Default::default()
        };
        let (marker, reference) = forward_media_marker(&media, "");
        assert_eq!(marker, "[video|ybres:v1] c.mp4");
        assert_eq!(reference.unwrap().kind, "video");
    }

    #[test]
    fn build_forward_text_renders_records() {
        let forward = sample_forward_data();
        let mut refs = Vec::new();
        let text = build_forward_text(&forward, "小明", "请看看", &mut refs);
        assert!(text
            .starts_with("当前用户的昵称为小明\n以下为用户的聊天记录\nAlice：hello world\nBob："));
        assert!(text.contains("[image|ybres:fw-rid-1] pic.jpg"));
        assert!(text.ends_with("用户附言：请看看"));
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].kind, "image");
        // Nickname fallback + no caption → no footer.
        let mut refs = Vec::new();
        let text = build_forward_text(&forward, " ", "", &mut refs);
        assert!(text.starts_with("当前用户的昵称为用户\n"));
        assert!(!text.contains("用户附言"));
    }

    #[test]
    fn build_forward_text_caps_record_body() {
        let long = "长".repeat(1200);
        let forward = proto::ForwardMsgData {
            sub_type: 1,
            msg: vec![proto::ForwardMsg {
                sender: "S".into(),
                plain_text: long,
                ..Default::default()
            }],
            ..Default::default()
        };
        let text = build_forward_text(&forward, "", "", &mut Vec::new());
        assert!(text.contains(&"长".repeat(1000)));
        assert!(text.contains("…(已截断)"));
        assert!(!text.contains(&"长".repeat(1001)));
    }

    #[test]
    fn patch_media_anchors_handles_forward_markers() {
        let refs = vec![
            MediaRef {
                kind: "file",
                url: "https://cdn/r?resourceId=fw-f1".into(),
                name: "report.pdf".into(),
            },
            MediaRef {
                kind: "video",
                url: "https://cdn/r?resourceId=fw-v1".into(),
                name: "".into(),
            },
        ];
        let resolved = vec![
            Some(crate::messaging::MediaAttachment {
                path: std::path::PathBuf::from("/cache/report.pdf"),
                mime: "application/pdf".into(),
                bytes: 10,
                original_name: "report.pdf".into(),
            }),
            Some(crate::messaging::MediaAttachment {
                path: std::path::PathBuf::from("/cache/clip.mp4"),
                mime: "video/mp4".into(),
                bytes: 10,
                original_name: "".into(),
            }),
        ];
        // Forward markers: filename OUTSIDE the file anchor; video anchor.
        let text = "看 [file|ybres:fw-f1] report.pdf 和 [video|ybres:fw-v1] clip";
        let patched = patch_media_anchors(text, &refs, &resolved);
        assert!(patched.contains("[file: report.pdf → /cache/report.pdf]"));
        assert!(patched.contains("[video: /cache/clip.mp4]"));
        assert!(!patched.contains("ybres:fw-f1"));
        assert!(!patched.contains("ybres:fw-v1"));
    }
}

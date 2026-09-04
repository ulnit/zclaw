//! WeCom (Enterprise WeChat) platform adapter — port of hermes
//! `plugins/platforms/wecom` @ v2026.8.3 (adapter.py).
//!
//! Uses the WeCom **AI Bot WebSocket gateway**: `aibot_subscribe`
//! handshake with bot id + secret, application-level `ping` heartbeats,
//! inbound `aibot_msg_callback` events, outbound `aibot_respond_msg`
//! (correlated to the inbound req_id — required for group chats) with
//! `aibot_send_msg` fallback for proactive DMs, and the three-step
//! `aibot_upload_media_*` chunked upload for native attachments.
//! Inbound media arrives as base64 payloads or remote URLs (optionally
//! AES-256-CBC encrypted with the per-message `aeskey` — WeCom's scheme
//! uses the key itself as the IV prefix).
//!
//! Inbound text batching merges the WeCom client's 4000-char splits:
//! plain-text frames are debounced per session scope (0.6 s quiet
//! period, 2.0 s when the latest chunk sits near the split threshold)
//! before dispatch (hermes `_enqueue_text_event` /
//! `_flush_text_batch`). The webhook self-built-app flow lives in
//! `wecom_callback` (hermes `callback_adapter.py`), not here.

use crate::messaging::{Dispatcher, MediaAttachment, MessageEvent};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

const DEFAULT_WS_URL: &str = "wss://openws.work.weixin.qq.com";
const CMD_SUBSCRIBE: &str = "aibot_subscribe";
const CMD_CALLBACK: &str = "aibot_msg_callback";
const CMD_LEGACY_CALLBACK: &str = "aibot_callback";
const CMD_EVENT_CALLBACK: &str = "aibot_event_callback";
const CMD_SEND: &str = "aibot_send_msg";
const CMD_RESPONSE: &str = "aibot_respond_msg";
const CMD_PING: &str = "ping";
const CMD_UPLOAD_INIT: &str = "aibot_upload_media_init";
const CMD_UPLOAD_CHUNK: &str = "aibot_upload_media_chunk";
const CMD_UPLOAD_FINISH: &str = "aibot_upload_media_finish";

const CONNECT_TIMEOUT_SECS: u64 = 20;
const REQUEST_TIMEOUT_SECS: u64 = 15;
const HEARTBEAT_INTERVAL_SECS: u64 = 30;
const RECONNECT_BACKOFF: &[u64] = &[2, 5, 10, 30, 60];
/// hermes `MAX_MESSAGE_LENGTH` for WeCom markdown.
const MAX_MESSAGE_LENGTH: usize = 4000;
const UPLOAD_CHUNK_SIZE: usize = 512 * 1024;
const MAX_UPLOAD_CHUNKS: usize = 100;
const DEDUP_WINDOW_SECS: u64 = 300;
const DEDUP_MAX_SIZE: usize = 1000;
/// hermes `_SPLIT_THRESHOLD` — a chunk this close to the 4000-char cap
/// almost certainly has a continuation chunk behind it.
const SPLIT_THRESHOLD: usize = 3900;
/// hermes `_text_batch_delay_seconds` default.
const DEFAULT_TEXT_BATCH_DELAY_SECS: f64 = 0.6;
/// hermes `_text_batch_split_delay_seconds` default.
const DEFAULT_TEXT_BATCH_SPLIT_DELAY_SECS: f64 = 2.0;

/// `[messaging.wecom]` — WeCom AI Bot adapter (hermes `platforms.wecom`
/// plugin config + `WECOM_*` env vars).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WeComConfig {
    pub enabled: bool,
    /// AI bot id (fallback `WECOM_BOT_ID`).
    pub bot_id: String,
    /// AI bot secret (fallback `WECOM_SECRET`).
    pub secret: String,
    /// Gateway URL (fallback `WECOM_WEBSOCKET_URL`).
    pub websocket_url: String,
    /// Stable device id sent with the subscribe handshake.
    pub device_id: String,
    /// DM intake policy: `pairing` (default) | `allowlist` | `open` |
    /// `disabled` (hermes dm_policy).
    pub dm_policy: String,
    /// DM allowlist.
    pub allow_from: Vec<String>,
    /// Group intake policy (same values).
    pub group_policy: String,
    /// Group allowlist.
    pub group_allow_from: Vec<String>,
    /// Quiet period before a buffered text batch flushes; 0 disables
    /// batching (hermes `HERMES_WECOM_TEXT_BATCH_DELAY_SECONDS`).
    pub text_batch_delay_seconds: f64,
    /// Longer quiet period when the latest chunk sits at the split
    /// threshold (hermes `HERMES_WECOM_TEXT_BATCH_SPLIT_DELAY_SECONDS`).
    pub text_batch_split_delay_seconds: f64,
}

impl Default for WeComConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bot_id: String::new(),
            secret: String::new(),
            websocket_url: String::new(),
            device_id: String::new(),
            dm_policy: "pairing".into(),
            allow_from: Vec::new(),
            group_policy: "pairing".into(),
            group_allow_from: Vec::new(),
            text_batch_delay_seconds: DEFAULT_TEXT_BATCH_DELAY_SECS,
            text_batch_split_delay_seconds: DEFAULT_TEXT_BATCH_SPLIT_DELAY_SECS,
        }
    }
}

fn env_trim(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn env_csv(name: &str) -> Option<Vec<String>> {
    env_trim(name).map(|raw| {
        raw.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    })
}

#[derive(Debug, Clone)]
pub struct ResolvedWeCom {
    pub bot_id: String,
    pub secret: String,
    pub websocket_url: String,
    pub device_id: String,
    pub dm_policy: String,
    pub allow_from: Vec<String>,
    pub group_policy: String,
    pub group_allow_from: Vec<String>,
    pub text_batch_delay_seconds: f64,
    pub text_batch_split_delay_seconds: f64,
}

impl WeComConfig {
    pub fn resolve(&self) -> ResolvedWeCom {
        ResolvedWeCom {
            bot_id: env_trim("WECOM_BOT_ID").unwrap_or_else(|| self.bot_id.trim().to_string()),
            secret: env_trim("WECOM_SECRET").unwrap_or_else(|| self.secret.trim().to_string()),
            websocket_url: env_trim("WECOM_WEBSOCKET_URL")
                .unwrap_or_else(|| self.websocket_url.trim().to_string()),
            device_id: env_trim("WECOM_DEVICE_ID")
                .unwrap_or_else(|| self.device_id.trim().to_string()),
            dm_policy: env_trim("WECOM_DM_POLICY")
                .unwrap_or_else(|| self.dm_policy.clone())
                .to_lowercase(),
            allow_from: env_csv("WECOM_ALLOW_FROM").unwrap_or_else(|| self.allow_from.clone()),
            group_policy: env_trim("WECOM_GROUP_POLICY")
                .unwrap_or_else(|| self.group_policy.clone())
                .to_lowercase(),
            group_allow_from: env_csv("WECOM_GROUP_ALLOW_FROM")
                .unwrap_or_else(|| self.group_allow_from.clone()),
            text_batch_delay_seconds: env_trim("WECOM_TEXT_BATCH_DELAY_SECONDS")
                .and_then(|v| v.parse().ok())
                .unwrap_or(self.text_batch_delay_seconds)
                .max(0.0),
            text_batch_split_delay_seconds: env_trim("WECOM_TEXT_BATCH_SPLIT_DELAY_SECONDS")
                .and_then(|v| v.parse().ok())
                .unwrap_or(self.text_batch_split_delay_seconds)
                .max(0.0),
        }
    }

    pub fn resolved_ws_url(&self) -> String {
        let resolved = self.resolve();
        if resolved.websocket_url.is_empty() {
            DEFAULT_WS_URL.to_string()
        } else {
            resolved.websocket_url
        }
    }
}

fn new_req_id(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::new_v4().simple())
}

fn payload_req_id(payload: &Value) -> String {
    payload
        .pointer("/headers/req_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn response_error(response: &Value) -> Option<String> {
    let errcode = response
        .get("errcode")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    if errcode == 0 {
        return None;
    }
    let errmsg = response
        .get("errmsg")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown error");
    Some(format!("WeCom errcode {errcode}: {errmsg}"))
}

/// WeCom AI-bot media AES: key = base64(aeskey), IV = key[..16],
/// AES-256-CBC, PKCS#7 (hermes `_decrypt_file_bytes`).
pub fn decrypt_file_bytes(encrypted: &[u8], aes_key: &str) -> Result<Vec<u8>, String> {
    use aes::cipher::{BlockDecrypt, KeyInit};
    use aes::Block;

    if encrypted.is_empty() {
        return Err("encrypted_data is empty".into());
    }
    let mut padded = aes_key.to_string();
    while padded.len() % 4 != 0 {
        padded.push('=');
    }
    use base64::Engine;
    let key = base64::engine::general_purpose::STANDARD
        .decode(padded.as_bytes())
        .map_err(|e| format!("bad aes_key: {e}"))?;
    if key.len() != 32 {
        return Err(format!(
            "invalid WeCom AES key length: expected 32 bytes, got {}",
            key.len()
        ));
    }
    if encrypted.len() % 16 != 0 {
        return Err("ciphertext not block-aligned".into());
    }
    let cipher = aes::Aes256::new_from_slice(&key).map_err(|e| e.to_string())?;
    let iv: [u8; 16] = key[..16].try_into().unwrap();
    let mut out = encrypted.to_vec();
    let mut prev = iv;
    for block in out.chunks_exact_mut(16) {
        let ciphertext: [u8; 16] = block.try_into().unwrap();
        let mut block_dec = Block::from(ciphertext);
        cipher.decrypt_block(&mut block_dec);
        for i in 0..16 {
            block[i] = block_dec[i] ^ prev[i];
        }
        prev = ciphertext;
    }
    let pad_len = *out.last().ok_or("empty plaintext")? as usize;
    if pad_len < 1 || pad_len > 32 || pad_len > out.len() {
        return Err(format!("invalid PKCS#7 padding value: {pad_len}"));
    }
    if out[out.len() - pad_len..]
        .iter()
        .any(|&b| b as usize != pad_len)
    {
        return Err("invalid PKCS#7 padding: bytes mismatch".into());
    }
    out.truncate(out.len() - pad_len);
    Ok(out)
}

struct Runtime {
    cfg: ResolvedWeCom,
    client: reqwest::Client,
    dedup: Mutex<HashMap<String, u64>>,
    /// chat_id → last inbound req_id (group replies MUST use
    /// `aibot_respond_msg` bound to a recent inbound).
    chat_req_ids: Mutex<HashMap<String, String>>,
    /// ws sender half shared with the PlatformSender.
    ws_sink: Mutex<Option<WsSink>>,
    /// Pending text batches keyed by session scope (hermes
    /// `_pending_text_batches` — merges WeCom client-side splits).
    text_batches: Mutex<HashMap<String, PendingTextBatch>>,
}

/// One buffered inbound text awaiting its quiet-period flush (hermes
/// `_pending_text_batches` entry + `_last_chunk_len` bookkeeping).
struct PendingTextBatch {
    event: MessageEvent,
    last_chunk_len: usize,
    is_group: bool,
    /// Incremented on every merge; flush tasks carrying a stale
    /// generation no-op (replaces hermes' task-cancellation dance).
    generation: u64,
}

type WsSink = futures::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    tokio_tungstenite::tungstenite::Message,
>;

impl Runtime {
    async fn is_duplicate(&self, msg_id: &str) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut dedup = self.dedup.lock().await;
        dedup.retain(|_, ts| now.saturating_sub(*ts) < DEDUP_WINDOW_SECS);
        if dedup.contains_key(msg_id) {
            return true;
        }
        if dedup.len() >= DEDUP_MAX_SIZE {
            let mut entries: Vec<(String, u64)> = dedup.drain().collect();
            entries.sort_by_key(|(_, ts)| *ts);
            entries.truncate(entries.len() / 2);
            *dedup = entries.into_iter().collect();
        }
        dedup.insert(msg_id.to_string(), now);
        false
    }

    async fn send_frame(&self, payload: &Value) -> Result<(), String> {
        use futures::SinkExt;
        let mut guard = self.ws_sink.lock().await;
        let sink = guard.as_mut().ok_or("websocket not connected")?;
        sink.send(tokio_tungstenite::tungstenite::Message::Text(
            payload.to_string(),
        ))
        .await
        .map_err(|e| format!("ws send: {e}"))
    }

    /// hermes `_send_request`: request/response correlated by req_id.
    /// Responses arrive on the listen loop, which parks them in
    /// `pending`; this helper polls the pending map.
    async fn send_request(
        &self,
        cmd: &str,
        body: &Value,
        pending: &Arc<Mutex<HashMap<String, Value>>>,
    ) -> Result<Value, String> {
        let req_id = new_req_id(cmd);
        self.send_frame(&json!({"cmd": cmd, "headers": {"req_id": req_id}, "body": body}))
            .await?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(REQUEST_TIMEOUT_SECS);
        loop {
            {
                let mut map = pending.lock().await;
                if let Some(resp) = map.remove(&req_id) {
                    return Ok(resp);
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(format!("{cmd} timed out after {REQUEST_TIMEOUT_SECS}s"));
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Reply bound to an inbound req_id (works for DMs and groups).
    async fn send_reply_markdown(&self, reply_req_id: &str, content: &str) -> Result<(), String> {
        let truncated: String = content.chars().take(MAX_MESSAGE_LENGTH).collect();
        self.send_frame(&json!({
            "cmd": CMD_RESPONSE,
            "headers": {"req_id": reply_req_id},
            "body": {"msgtype": "markdown", "markdown": {"content": truncated}},
        }))
        .await
    }

    /// Proactive DM send (groups cannot initiate `aibot_send_msg`).
    async fn send_proactive_markdown(&self, chat_id: &str, content: &str) -> Result<(), String> {
        let truncated: String = content.chars().take(MAX_MESSAGE_LENGTH).collect();
        self.send_frame(&json!({
            "cmd": CMD_SEND,
            "headers": {"req_id": new_req_id(CMD_SEND)},
            "body": {
                "chatid": chat_id,
                "msgtype": "markdown",
                "markdown": {"content": truncated},
            },
        }))
        .await
    }

    /// Three-step chunked media upload (hermes `_upload_media_bytes`).
    async fn upload_media(
        &self,
        data: &[u8],
        media_type: &str,
        filename: &str,
        pending: &Arc<Mutex<HashMap<String, Value>>>,
    ) -> Result<String, String> {
        use md5::Digest;
        let total_size = data.len();
        let total_chunks = (total_size + UPLOAD_CHUNK_SIZE - 1) / UPLOAD_CHUNK_SIZE;
        if total_chunks > MAX_UPLOAD_CHUNKS {
            return Err(format!(
                "file too large: {total_chunks} chunks exceeds maximum of {MAX_UPLOAD_CHUNKS}"
            ));
        }
        let md5_hex = format!("{:x}", md5::Md5::digest(data));
        let init = self
            .send_request(
                CMD_UPLOAD_INIT,
                &json!({
                    "type": media_type,
                    "filename": filename,
                    "total_size": total_size,
                    "total_chunks": total_chunks,
                    "md5": md5_hex,
                }),
                pending,
            )
            .await?;
        if let Some(err) = response_error(&init) {
            return Err(format!("media upload init failed: {err}"));
        }
        let upload_id = init
            .pointer("/body/upload_id")
            .and_then(|v| v.as_str())
            .ok_or("media upload init: missing upload_id")?
            .to_string();
        for (chunk_index, start) in (0..total_size).step_by(UPLOAD_CHUNK_SIZE).enumerate() {
            let chunk = &data[start..(start + UPLOAD_CHUNK_SIZE).min(total_size)];
            use base64::Engine;
            let b64 = base64::engine::general_purpose::STANDARD.encode(chunk);
            let resp = self
                .send_request(
                    CMD_UPLOAD_CHUNK,
                    &json!({
                        "upload_id": upload_id,
                        "chunk_index": chunk_index,
                        "base64_data": b64,
                    }),
                    pending,
                )
                .await?;
            if let Some(err) = response_error(&resp) {
                return Err(format!("media upload chunk {chunk_index} failed: {err}"));
            }
        }
        let finish = self
            .send_request(CMD_UPLOAD_FINISH, &json!({"upload_id": upload_id}), pending)
            .await?;
        if let Some(err) = response_error(&finish) {
            return Err(format!("media upload finish failed: {err}"));
        }
        finish
            .pointer("/body/media_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or("media upload finish: missing media_id".into())
    }
}

/// Entry point spawned by `run_messaging`.
pub async fn run(
    cfg: WeComConfig,
    dispatcher: Arc<Dispatcher>,
    pairing: Option<Arc<crate::pairing::PairingStore>>,
) {
    let resolved = cfg.resolve();
    if resolved.bot_id.is_empty() || resolved.secret.is_empty() {
        eprintln!(
            "[wecom] disabled: set [messaging.wecom] bot_id/secret or WECOM_BOT_ID/WECOM_SECRET"
        );
        return;
    }
    let ws_url = if resolved.websocket_url.is_empty() {
        DEFAULT_WS_URL.to_string()
    } else {
        resolved.websocket_url.clone()
    };
    let runtime = Arc::new(Runtime {
        cfg: resolved,
        client: reqwest::Client::new(),
        dedup: Mutex::new(HashMap::new()),
        chat_req_ids: Mutex::new(HashMap::new()),
        ws_sink: Mutex::new(None),
        text_batches: Mutex::new(HashMap::new()),
    });
    let pending: Arc<Mutex<HashMap<String, Value>>> = Arc::new(Mutex::new(HashMap::new()));
    crate::messaging::register_platform_sender(
        "wecom",
        Arc::new(WeComSender {
            runtime: runtime.clone(),
        }),
    );

    let mut backoff_idx: usize = 0;
    loop {
        match ws_session(&runtime, &pending, &ws_url, &dispatcher, &pairing).await {
            Ok(()) => backoff_idx = 0,
            Err(e) => eprintln!("[wecom] session error: {e}"),
        }
        let delay = RECONNECT_BACKOFF[backoff_idx.min(RECONNECT_BACKOFF.len() - 1)];
        eprintln!("[wecom] reconnecting in {delay}s");
        tokio::time::sleep(Duration::from_secs(delay)).await;
        backoff_idx += 1;
    }
}

async fn ws_session(
    runtime: &Arc<Runtime>,
    pending: &Arc<Mutex<HashMap<String, Value>>>,
    ws_url: &str,
    dispatcher: &Arc<Dispatcher>,
    pairing: &Option<Arc<crate::pairing::PairingStore>>,
) -> Result<(), String> {
    use futures::StreamExt;

    let connect = tokio_tungstenite::connect_async(ws_url);
    let (ws, _) = tokio::time::timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS), connect)
        .await
        .map_err(|_| "ws connect timeout".to_string())?
        .map_err(|e| format!("ws connect: {e}"))?;
    let (sink, mut stream) = ws.split();
    *runtime.ws_sink.lock().await = Some(sink);

    // Subscribe handshake.
    let req_id = new_req_id("subscribe");
    runtime
        .send_frame(&json!({
            "cmd": CMD_SUBSCRIBE,
            "headers": {"req_id": req_id},
            "body": {
                "bot_id": runtime.cfg.bot_id,
                "secret": runtime.cfg.secret,
                "device_id": runtime.cfg.device_id,
            },
        }))
        .await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(CONNECT_TIMEOUT_SECS);
    let mut authenticated = false;
    while !authenticated {
        if tokio::time::Instant::now() >= deadline {
            return Err("timed out waiting for WeCom subscribe acknowledgement".into());
        }
        let Some(Ok(message)) = stream.next().await else {
            return Err("ws closed during authentication".into());
        };
        let tokio_tungstenite::tungstenite::Message::Text(text) = message else {
            continue;
        };
        let Ok(payload) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if payload.get("cmd").and_then(|v| v.as_str()) == Some(CMD_PING) {
            continue;
        }
        if payload_req_id(&payload) == req_id {
            if let Some(err) = response_error(&payload) {
                return Err(format!("authentication failed: {err}"));
            }
            authenticated = true;
        }
    }
    eprintln!(
        "[wecom] connected and subscribed as bot {}",
        runtime.cfg.bot_id
    );

    let mut ping_due = tokio::time::Instant::now() + Duration::from_secs(HEARTBEAT_INTERVAL_SECS);
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(ping_due) => {
                let _ = runtime
                    .send_frame(&json!({
                        "cmd": CMD_PING,
                        "headers": {"req_id": new_req_id("ping")},
                        "body": {},
                    }))
                    .await;
                ping_due = tokio::time::Instant::now() + Duration::from_secs(HEARTBEAT_INTERVAL_SECS);
            }
            message = stream.next() => {
                let Some(message) = message else {
                    return Err("ws closed".into());
                };
                let message = match message {
                    Ok(m) => m,
                    Err(e) => return Err(format!("ws: {e}")),
                };
                let tokio_tungstenite::tungstenite::Message::Text(text) = message else {
                    continue;
                };
                let Ok(payload) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                let cmd = payload
                    .get("cmd")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                match cmd.as_str() {
                    CMD_CALLBACK | CMD_LEGACY_CALLBACK => {
                        handle_callback(runtime, &payload, dispatcher, pairing).await;
                    }
                    CMD_EVENT_CALLBACK | CMD_PING => {}
                    "" => {}
                    _ => {
                        // Response to an outstanding request — park it for
                        // `send_request` correlation.
                        let rid = payload_req_id(&payload);
                        if !rid.is_empty() {
                            pending.lock().await.insert(rid, payload);
                        }
                    }
                }
            }
        }
    }
}

/// hermes `_on_message`.
async fn handle_callback(
    runtime: &Arc<Runtime>,
    payload: &Value,
    dispatcher: &Arc<Dispatcher>,
    pairing: &Option<Arc<crate::pairing::PairingStore>>,
) {
    let body = payload.get("body").cloned().unwrap_or(json!({}));
    let inbound_req_id = payload_req_id(payload);
    let msg_id = body
        .get("msgid")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            if inbound_req_id.is_empty() {
                uuid::Uuid::new_v4().simple().to_string()
            } else {
                inbound_req_id.clone()
            }
        });
    if runtime.is_duplicate(&msg_id).await {
        return;
    }

    let sender_id = body
        .pointer("/from/userid")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let chat_id_raw = body
        .get("chatid")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let chat_id = if chat_id_raw.is_empty() {
        sender_id.clone()
    } else {
        chat_id_raw
    };
    if chat_id.is_empty() {
        return;
    }
    let is_group = body
        .get("chattype")
        .and_then(|v| v.as_str())
        .map(|t| t.eq_ignore_ascii_case("group"))
        .unwrap_or(false);

    // Intake policies (hermes dm_policy/group_policy; ulnclaw pairing
    // union).
    if is_group {
        let allowed = policy_allows(
            &runtime.cfg.group_policy,
            &runtime.cfg.group_allow_from,
            &chat_id,
            pairing.as_deref(),
            "wecom",
        );
        if !allowed {
            eprintln!("[wecom] group {chat_id} blocked by policy");
            return;
        }
    } else {
        let allowed = policy_allows(
            &runtime.cfg.dm_policy,
            &runtime.cfg.allow_from,
            &sender_id,
            pairing.as_deref(),
            "wecom",
        );
        if !allowed {
            if let Some(store) = pairing {
                if let Some(code_msg) =
                    crate::messaging::pairing_offer_public(store, "wecom", &sender_id, &sender_id)
                {
                    let _ = runtime
                        .send_reply_markdown(&inbound_req_id, &code_msg)
                        .await;
                }
            }
            eprintln!("[wecom] DM sender {sender_id} blocked by policy");
            return;
        }
    }

    // Cache the req_id for later group replies.
    if !inbound_req_id.is_empty() {
        runtime
            .chat_req_ids
            .lock()
            .await
            .insert(chat_id.clone(), inbound_req_id.clone());
    }

    let mut text = extract_text(&body);
    let reply_text = extract_quote_text(&body);
    if is_group && !text.is_empty() {
        text = strip_leading_mention(&text);
    }
    let attachments = extract_media(runtime, &body).await;
    if text.trim().is_empty() && attachments.is_empty() {
        if let Some(rt) = reply_text {
            text = rt;
        }
    }
    if text.trim().is_empty() && attachments.is_empty() {
        return;
    }

    let event = MessageEvent {
        platform: "wecom".into(),
        chat_id: chat_id.clone(),
        sender_id: sender_id.clone(),
        sender_name: sender_id.clone(),
        text,
        message_id: msg_id,
        attachments,
    };
    // Text batching (hermes `_enqueue_text_event`): everything except
    // voice notes debounces — the WeCom client splits long user
    // messages around 4000 chars and the chunks land milliseconds
    // apart.
    let msgtype = body
        .get("msgtype")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();
    if msgtype != "voice" && runtime.cfg.text_batch_delay_seconds > 0.0 {
        enqueue_text_batch(runtime, dispatcher, event, is_group).await;
        return;
    }
    dispatch_wecom_event(runtime, dispatcher, event, is_group).await;
}

/// Dispatch one (possibly batch-merged) inbound event and deliver the
/// reply — the tail of `handle_callback`, also run by the batch flush.
async fn dispatch_wecom_event(
    runtime: &Arc<Runtime>,
    dispatcher: &Arc<Dispatcher>,
    mut event: MessageEvent,
    is_group: bool,
) {
    if !crate::messaging::pre_gateway_dispatch_gate_public(&mut event).await {
        return;
    }
    let chat_id = event.chat_id.clone();
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
    let (reply_text_out, media_paths) = crate::messaging::extract_media_tags(&full);
    let reply_req = runtime
        .chat_req_ids
        .lock()
        .await
        .get(&chat_id)
        .cloned()
        .unwrap_or_default();
    for path in &media_paths {
        send_media_path(runtime, &chat_id, &reply_req, path).await;
    }
    if !reply_text_out.trim().is_empty() {
        // P705: ledger-protected reply delivery (reply-markdown, then
        // proactive DM fallback).
        dispatcher
            .try_send_with_ledger("wecom", &chat_id, &reply_text_out, || async {
                if !reply_req.is_empty() {
                    match runtime
                        .send_reply_markdown(&reply_req, &reply_text_out)
                        .await
                    {
                        Ok(()) => Ok(()),
                        Err(e) => {
                            eprintln!("[wecom] reply failed: {e}");
                            Err(e.to_string())
                        }
                    }
                } else if !is_group {
                    match runtime
                        .send_proactive_markdown(&chat_id, &reply_text_out)
                        .await
                    {
                        Ok(()) => Ok(()),
                        Err(e) => {
                            eprintln!("[wecom] proactive send failed: {e}");
                            Err(e.to_string())
                        }
                    }
                } else {
                    eprintln!("[wecom] cannot reply to group {chat_id}: no inbound req_id cached");
                    Err(format!(
                        "cannot reply to group {chat_id}: no inbound req_id cached"
                    ))
                }
            })
            .await;
    }
}

/// Session-scoped batch key (hermes `_text_batch_key` with the default
/// `group_sessions_per_user = true`): group messages batch per sender,
/// DMs per chat.
pub fn text_batch_key(chat_id: &str, sender_id: &str, is_group: bool) -> String {
    if is_group {
        format!("{chat_id}|{sender_id}")
    } else {
        chat_id.to_string()
    }
}

/// Quiet period before flushing a batch (hermes `_flush_text_batch`):
/// a chunk at/over the split threshold gets the longer delay since a
/// continuation is almost certain.
pub fn batch_flush_delay(last_chunk_len: usize, delay: f64, split_delay: f64) -> f64 {
    if last_chunk_len >= SPLIT_THRESHOLD {
        split_delay
    } else {
        delay
    }
}

/// Merge an inbound text frame into the pending batch and (re)start
/// the flush timer (hermes `_enqueue_text_event`). Each merge bumps
/// the generation; stale flush tasks no-op.
async fn enqueue_text_batch(
    runtime: &Arc<Runtime>,
    dispatcher: &Arc<Dispatcher>,
    event: MessageEvent,
    is_group: bool,
) {
    let key = text_batch_key(&event.chat_id, &event.sender_id, is_group);
    let chunk_len = event.text.chars().count();
    let generation = {
        let mut batches = runtime.text_batches.lock().await;
        match batches.get_mut(&key) {
            Some(pending) => {
                if !event.text.is_empty() {
                    if pending.event.text.is_empty() {
                        pending.event.text = event.text.clone();
                    } else {
                        pending.event.text.push('\n');
                        pending.event.text.push_str(&event.text);
                    }
                }
                pending.event.attachments.extend(event.attachments);
                pending.last_chunk_len = chunk_len;
                pending.generation += 1;
                pending.generation
            }
            None => {
                batches.insert(
                    key.clone(),
                    PendingTextBatch {
                        event,
                        last_chunk_len: chunk_len,
                        is_group,
                        generation: 0,
                    },
                );
                0
            }
        }
    };
    let runtime = runtime.clone();
    let dispatcher = dispatcher.clone();
    tokio::spawn(async move {
        flush_text_batch(&runtime, &dispatcher, &key, generation).await;
    });
}

/// Wait out the quiet period, then dispatch the merged batch (hermes
/// `_flush_text_batch`). A bumped generation means a newer frame
/// restarted the timer — this task steps aside.
async fn flush_text_batch(
    runtime: &Arc<Runtime>,
    dispatcher: &Arc<Dispatcher>,
    key: &str,
    generation: u64,
) {
    let delay = {
        let batches = runtime.text_batches.lock().await;
        match batches.get(key) {
            Some(pending) if pending.generation == generation => batch_flush_delay(
                pending.last_chunk_len,
                runtime.cfg.text_batch_delay_seconds,
                runtime.cfg.text_batch_split_delay_seconds,
            ),
            _ => return,
        }
    };
    tokio::time::sleep(Duration::from_secs_f64(delay)).await;
    let batch = {
        let mut batches = runtime.text_batches.lock().await;
        match batches.get(key) {
            Some(pending) if pending.generation == generation => batches.remove(key),
            _ => None,
        }
    };
    let Some(batch) = batch else {
        return;
    };
    dispatch_wecom_event(runtime, dispatcher, batch.event, batch.is_group).await;
}

/// hermes policy semantics: `open` | `allowlist` | `disabled` |
/// `pairing` (pairing = allowlist ∪ approved pairing codes).
fn policy_allows(
    policy: &str,
    allow_from: &[String],
    target: &str,
    pairing: Option<&crate::pairing::PairingStore>,
    platform: &str,
) -> bool {
    match policy {
        "open" => true,
        "disabled" => false,
        "allowlist" => allow_from.iter().any(|a| a == target || a == "*"),
        _ => {
            // pairing (default)
            if allow_from.iter().any(|a| a == target || a == "*") {
                return true;
            }
            pairing
                .map(|store| store.is_approved(platform, target))
                .unwrap_or(false)
        }
    }
}

/// hermes `_extract_text`: mixed items / text / voice content / appmsg
/// title.
pub fn extract_text(body: &Value) -> String {
    let msgtype = body
        .get("msgtype")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();
    let mut parts: Vec<String> = Vec::new();
    if msgtype == "mixed" {
        if let Some(items) = body.pointer("/mixed/msg_item").and_then(|v| v.as_array()) {
            for item in items {
                if item.get("msgtype").and_then(|v| v.as_str()) == Some("text") {
                    if let Some(content) = item
                        .pointer("/text/content")
                        .and_then(|v| v.as_str())
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                    {
                        parts.push(content);
                    }
                }
            }
        }
    } else {
        if let Some(content) = body
            .pointer("/text/content")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        {
            parts.push(content);
        }
        if msgtype == "voice" {
            if let Some(voice_text) = body
                .pointer("/voice/content")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
            {
                parts.push(voice_text);
            }
        }
        if msgtype == "appmsg" {
            if let Some(title) = body
                .pointer("/appmsg/title")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
            {
                parts.push(title);
            }
        }
    }
    parts.join("\n")
}

fn extract_quote_text(body: &Value) -> Option<String> {
    let quote_type = body
        .pointer("/quote/msgtype")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();
    let content = match quote_type.as_str() {
        "text" => body.pointer("/quote/text/content").and_then(|v| v.as_str()),
        "voice" => body
            .pointer("/quote/voice/content")
            .and_then(|v| v.as_str()),
        _ => None,
    };
    content
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// hermes: strip leading `@BotName` so `/commands` survive.
pub fn strip_leading_mention(text: &str) -> String {
    let re = regex::Regex::new(r"^@\S+\s*").unwrap();
    re.replace(text, "").trim().to_string()
}

/// Inbound media: base64 payloads or URLs (optionally AES-encrypted).
async fn extract_media(runtime: &Arc<Runtime>, body: &Value) -> Vec<MediaAttachment> {
    let mut refs: Vec<(&str, Value)> = Vec::new();
    let msgtype = body
        .get("msgtype")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();
    if msgtype == "mixed" {
        if let Some(items) = body.pointer("/mixed/msg_item").and_then(|v| v.as_array()) {
            for item in items {
                if item.get("msgtype").and_then(|v| v.as_str()) == Some("image") {
                    if let Some(image) = item.get("image") {
                        refs.push(("image", image.clone()));
                    }
                }
            }
        }
    } else {
        if let Some(image) = body.get("image") {
            refs.push(("image", image.clone()));
        }
        if msgtype == "file" {
            if let Some(file) = body.get("file") {
                refs.push(("file", file.clone()));
            }
        }
        if msgtype == "appmsg" {
            if let Some(file) = body.pointer("/appmsg/file") {
                refs.push(("file", file.clone()));
            } else if let Some(image) = body.pointer("/appmsg/image") {
                refs.push(("image", image.clone()));
            }
        }
    }
    if let Some(quote) = body.get("quote") {
        let quote_type = quote
            .get("msgtype")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_lowercase();
        if quote_type == "image" {
            if let Some(image) = quote.get("image") {
                refs.push(("image", image.clone()));
            }
        } else if quote_type == "file" {
            if let Some(file) = quote.get("file") {
                refs.push(("file", file.clone()));
            }
        }
    }

    let mut attachments = Vec::new();
    for (kind, media) in refs {
        if let Some(att) = cache_media(runtime, kind, &media).await {
            attachments.push(att);
        }
    }
    attachments
}

async fn cache_media(runtime: &Arc<Runtime>, kind: &str, media: &Value) -> Option<MediaAttachment> {
    use base64::Engine;
    let home = crate::config::ulnclaw_home();
    if let Some(b64) = media
        .get("base64")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        let payload = b64.split(',').next_back().unwrap_or("").trim();
        let raw = base64::engine::general_purpose::STANDARD
            .decode(payload)
            .ok()?;
        let filename = media
            .get("filename")
            .or_else(|| media.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("wecom_file")
            .to_string();
        let mime = if kind == "image" {
            mime_from_magic(&raw)
        } else {
            crate::media_cache::mime_for_ext(std::path::Path::new(&filename))
        };
        let path = crate::media_cache::cache_media_bytes(&home, &raw, &mime, &filename).ok()?;
        return Some(MediaAttachment {
            path,
            mime,
            bytes: raw.len() as u64,
            original_name: filename,
        });
    }
    let url = media
        .get("url")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if url.is_empty() {
        return None;
    }
    let resp = runtime
        .client
        .get(&url)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let mut raw = resp.bytes().await.ok()?.to_vec();
    if let Some(aes_key) = media
        .get("aeskey")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
    {
        match decrypt_file_bytes(&raw, aes_key.trim()) {
            Ok(decrypted) => raw = decrypted,
            Err(e) => {
                eprintln!("[wecom] media decrypt failed: {e}");
                return None;
            }
        }
    }
    let filename = url
        .rsplit_once('/')
        .map(|(_, name)| name.split('?').next().unwrap_or("wecom_file").to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "wecom_file".into());
    let mime = if kind == "image" {
        mime_from_magic(&raw)
    } else {
        crate::media_cache::mime_for_ext(std::path::Path::new(&filename))
    };
    let path = crate::media_cache::cache_media_bytes(&home, &raw, &mime, &filename).ok()?;
    Some(MediaAttachment {
        path,
        mime,
        bytes: raw.len() as u64,
        original_name: filename,
    })
}

fn mime_from_magic(data: &[u8]) -> String {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png".into()
    } else if data.starts_with(b"\xff\xd8\xff") {
        "image/jpeg".into()
    } else if data.starts_with(b"GIF8") {
        "image/gif".into()
    } else if data.len() > 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        "image/webp".into()
    } else {
        "image/jpeg".into()
    }
}

async fn send_media_path(
    runtime: &Arc<Runtime>,
    chat_id: &str,
    reply_req_id: &str,
    path: &std::path::Path,
) {
    // Uploads need request/response correlation which only exists while a
    // session loop owns `pending`; the sender path degrades to a markdown
    // path note when no session is live.
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[wecom] media read {} failed: {e}", path.display());
            return;
        }
    };
    let mime = crate::media_cache::mime_for_ext(path);
    let media_type = match crate::media_cache::media_kind(&mime) {
        "image" => "image",
        "audio" => "voice",
        "video" => "file",
        _ => "file",
    };
    let filename = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "attachment".into());
    // Media upload requires the pending-response map of a live session;
    // without it, fall back to a markdown mention.
    if reply_req_id.is_empty() {
        let _ = runtime
            .send_proactive_markdown(chat_id, &format!("[file] {}", path.display()))
            .await;
        return;
    }
    let pending: Arc<Mutex<HashMap<String, Value>>> = Arc::new(Mutex::new(HashMap::new()));
    match runtime
        .upload_media(&data, media_type, &filename, &pending)
        .await
    {
        Ok(media_id) => {
            let body = json!({"msgtype": media_type, media_type: {"media_id": media_id}});
            let _ = runtime
                .send_frame(&json!({
                    "cmd": CMD_RESPONSE,
                    "headers": {"req_id": reply_req_id},
                    "body": body,
                }))
                .await;
        }
        Err(e) => {
            eprintln!("[wecom] media upload failed: {e}");
            let _ = runtime
                .send_reply_markdown(reply_req_id, &format!("[file] {}", path.display()))
                .await;
        }
    }
}

struct WeComSender {
    runtime: Arc<Runtime>,
}

#[async_trait::async_trait]
impl crate::messaging::PlatformSender for WeComSender {
    async fn send_text(&self, chat_id: &str, text: &str) {
        // Prefer a cached inbound req_id (required for groups), fall back
        // to proactive DM send.
        let req_id = self
            .runtime
            .chat_req_ids
            .lock()
            .await
            .get(chat_id)
            .cloned()
            .unwrap_or_default();
        let result = if !req_id.is_empty() {
            self.runtime.send_reply_markdown(&req_id, text).await
        } else {
            self.runtime.send_proactive_markdown(chat_id, text).await
        };
        if let Err(e) = result {
            eprintln!("[wecom] send_text to {chat_id} failed: {e}");
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
    fn text_extraction_plain_and_voice() {
        let body = json!({"msgtype": "text", "text": {"content": " hello "}});
        assert_eq!(extract_text(&body), "hello");
        let body = json!({"msgtype": "voice", "voice": {"content": "转写文本"}});
        assert_eq!(extract_text(&body), "转写文本");
    }

    #[test]
    fn text_extraction_mixed() {
        let body = json!({
            "msgtype": "mixed",
            "mixed": {"msg_item": [
                {"msgtype": "text", "text": {"content": "part1"}},
                {"msgtype": "image", "image": {}},
                {"msgtype": "text", "text": {"content": "part2"}},
            ]},
        });
        assert_eq!(extract_text(&body), "part1\npart2");
    }

    #[test]
    fn appmsg_title_extracted() {
        let body = json!({"msgtype": "appmsg", "appmsg": {"title": "report.pdf"}});
        assert_eq!(extract_text(&body), "report.pdf");
    }

    #[test]
    fn quote_text_extraction() {
        let body = json!({"quote": {"msgtype": "text", "text": {"content": "quoted"}}});
        assert_eq!(extract_quote_text(&body), Some("quoted".into()));
        let body = json!({"quote": {"msgtype": "image"}});
        assert_eq!(extract_quote_text(&body), None);
    }

    #[test]
    fn leading_mention_stripped() {
        assert_eq!(strip_leading_mention("@BotName /approve"), "/approve");
        assert_eq!(strip_leading_mention("hello"), "hello");
    }

    #[test]
    fn response_error_parsing() {
        assert!(response_error(&json!({"errcode": 0})).is_none());
        let err = response_error(&json!({"errcode": 40001, "errmsg": "invalid token"})).unwrap();
        assert!(err.contains("40001"));
        assert!(err.contains("invalid token"));
    }

    #[test]
    fn aes_decrypt_roundtrip() {
        use aes::cipher::{BlockEncrypt, KeyInit};
        use aes::Block;
        // Build a ciphertext with the same scheme: key, IV = key[..16].
        let key = [7u8; 32];
        let iv: [u8; 16] = key[..16].try_into().unwrap();
        let plaintext = b"hello wecom media";
        let pad_len = 16 - (plaintext.len() % 16);
        let mut padded = plaintext.to_vec();
        padded.extend(std::iter::repeat(pad_len as u8).take(pad_len));
        let cipher = aes::Aes256::new_from_slice(&key).unwrap();
        let mut out = padded.clone();
        let mut prev = iv;
        for block in out.chunks_exact_mut(16) {
            for i in 0..16 {
                block[i] ^= prev[i];
            }
            let mut b = Block::from(<[u8; 16]>::try_from(&*block).unwrap());
            cipher.encrypt_block(&mut b);
            block.copy_from_slice(&b);
            prev = b.into();
        }
        use base64::Engine;
        let key_b64 = base64::engine::general_purpose::STANDARD.encode(key);
        let decrypted = decrypt_file_bytes(&out, &key_b64).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn policy_rules() {
        let one = vec!["user1".to_string()];
        let star = vec!["*".to_string()];
        assert!(policy_allows("open", &[], "anyone", None, "wecom"));
        assert!(!policy_allows("disabled", &one, "x", None, "wecom"));
        assert!(policy_allows("allowlist", &one, "user1", None, "wecom"));
        assert!(!policy_allows("allowlist", &one, "user2", None, "wecom"));
        assert!(policy_allows("pairing", &star, "user2", None, "wecom"));
        assert!(!policy_allows("pairing", &[], "user2", None, "wecom"));
    }

    #[test]
    fn req_id_generation() {
        let id = new_req_id("subscribe");
        assert!(id.starts_with("subscribe-"));
        assert!(id.len() > 12);
    }

    #[test]
    fn mime_magic_detection() {
        assert_eq!(mime_from_magic(b"\x89PNG\r\n\x1a\nrest"), "image/png");
        assert_eq!(mime_from_magic(b"\xff\xd8\xff\xe0"), "image/jpeg");
        assert_eq!(mime_from_magic(b"GIF89a"), "image/gif");
    }

    #[tokio::test]
    async fn dedup_behavior() {
        let runtime = Runtime {
            cfg: ResolvedWeCom {
                bot_id: String::new(),
                secret: String::new(),
                websocket_url: String::new(),
                device_id: String::new(),
                dm_policy: "pairing".into(),
                allow_from: Vec::new(),
                group_policy: "pairing".into(),
                group_allow_from: Vec::new(),
                text_batch_delay_seconds: DEFAULT_TEXT_BATCH_DELAY_SECS,
                text_batch_split_delay_seconds: DEFAULT_TEXT_BATCH_SPLIT_DELAY_SECS,
            },
            client: reqwest::Client::new(),
            dedup: Mutex::new(HashMap::new()),
            chat_req_ids: Mutex::new(HashMap::new()),
            ws_sink: Mutex::new(None),
            text_batches: Mutex::new(HashMap::new()),
        };
        assert!(!runtime.is_duplicate("m1").await);
        assert!(runtime.is_duplicate("m1").await);
    }

    #[test]
    fn batch_key_scopes_groups_per_sender() {
        // hermes default: group_sessions_per_user = true.
        assert_eq!(text_batch_key("c1", "alice", true), "c1|alice");
        assert_eq!(text_batch_key("c1", "bob", true), "c1|bob");
        // DMs batch per chat regardless of sender.
        assert_eq!(text_batch_key("c1", "alice", false), "c1");
    }

    #[test]
    fn flush_delay_uses_split_window_near_threshold() {
        assert_eq!(batch_flush_delay(100, 0.6, 2.0), 0.6);
        assert_eq!(batch_flush_delay(SPLIT_THRESHOLD - 1, 0.6, 2.0), 0.6);
        assert_eq!(batch_flush_delay(SPLIT_THRESHOLD, 0.6, 2.0), 2.0);
        assert_eq!(batch_flush_delay(4000, 0.6, 2.0), 2.0);
    }

    #[tokio::test]
    async fn text_batch_merge_and_generation() {
        let runtime = Arc::new(Runtime {
            cfg: ResolvedWeCom {
                bot_id: String::new(),
                secret: String::new(),
                websocket_url: String::new(),
                device_id: String::new(),
                dm_policy: "pairing".into(),
                allow_from: Vec::new(),
                group_policy: "pairing".into(),
                group_allow_from: Vec::new(),
                text_batch_delay_seconds: 0.6,
                text_batch_split_delay_seconds: 2.0,
            },
            client: reqwest::Client::new(),
            dedup: Mutex::new(HashMap::new()),
            chat_req_ids: Mutex::new(HashMap::new()),
            ws_sink: Mutex::new(None),
            text_batches: Mutex::new(HashMap::new()),
        });
        // Simulate the merge half of enqueue_text_batch directly (the
        // flush half needs a live dispatcher).
        let key = text_batch_key("c1", "alice", false);
        {
            let mut batches = runtime.text_batches.lock().await;
            batches.insert(
                key.clone(),
                PendingTextBatch {
                    event: MessageEvent {
                        platform: "wecom".into(),
                        chat_id: "c1".into(),
                        sender_id: "alice".into(),
                        sender_name: "alice".into(),
                        text: "part one".into(),
                        message_id: "m1".into(),
                        attachments: Vec::new(),
                    },
                    last_chunk_len: 8,
                    is_group: false,
                    generation: 0,
                },
            );
        }
        {
            let mut batches = runtime.text_batches.lock().await;
            let pending = batches.get_mut(&key).unwrap();
            pending.event.text.push('\n');
            pending.event.text.push_str("part two");
            pending.last_chunk_len = 8;
            pending.generation += 1;
        }
        let batches = runtime.text_batches.lock().await;
        let pending = batches.get(&key).unwrap();
        assert_eq!(pending.event.text, "part one\npart two");
        assert_eq!(pending.generation, 1);
    }
}

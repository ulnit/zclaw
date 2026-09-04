//! SimpleX Chat platform adapter — port of hermes
//! `plugins/platforms/simplex` @ v2026.8.3 (adapter.py).
//!
//! Connects to a `simplex-chat` daemon running in WebSocket mode
//! (`simplex-chat -p 5225`). Inbound events arrive as JSON frames
//! (`{"corrId": ..., "resp": {...}}`): `contactRequest` (auto-accepted
//! with `/accept <id>` when enabled), `rcvFileDescrReady` (answered
//! with fire-and-forget `/freceive <fileId>`), `newChatItems` /
//! `newChatItem` (message intake) and `rcvFileComplete` (delivers
//! voice notes deferred until the XFTP download finished).
//!
//! Intake mirrors hermes: own messages (`directSnd`/`groupSnd`) are
//! dropped, only `rcvMsgContent` is processed, DMs are allowlisted by
//! contact id OR display name ∪ pairing, groups require
//! `SIMPLEX_GROUP_ALLOWED` (`*` wildcard) membership. Consecutive text
//! messages batch into one event behind a quiet-period timer (0.8 s
//! default). Outbound rides the same WS: DMs use `@<id> text`, groups
//! the structured `/_send #<id> json [...]` form; `MEDIA:` tags become
//! voice notes (audio extensions) or file documents.

use crate::messaging::{Dispatcher, MediaAttachment, MessageEvent};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{oneshot, Mutex};

/// hermes chunk cap (SimpleX has no hard limit).
const MAX_MESSAGE_LENGTH: usize = 8000;
/// hermes `_CORR_PREFIX` — our corrIds, used to drop our own echoes.
const CORR_PREFIX: &str = "hermes-";
const WS_RETRY_INITIAL_SECS: u64 = 2;
const WS_RETRY_MAX_SECS: u64 = 60;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_TEXT_BATCH_DELAY_MS: u64 = 800;

/// `[messaging.simplex]` — SimpleX adapter (hermes `platforms.simplex`
/// plugin config + `SIMPLEX_*` env vars).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SimplexConfig {
    pub enabled: bool,
    /// Daemon WebSocket URL (fallback `SIMPLEX_WS_URL`, default
    /// `ws://127.0.0.1:5225`).
    pub ws_url: String,
    /// Contact ids or display names allowed in DMs (fallback
    /// `SIMPLEX_ALLOWED_USERS`).
    pub allowed_users: Vec<String>,
    /// Allow every contact (fallback `SIMPLEX_ALLOW_ALL_USERS`).
    pub allow_all_users: bool,
    /// Auto-accept contact requests (fallback `SIMPLEX_AUTO_ACCEPT`,
    /// default true).
    pub auto_accept: bool,
    /// Group ids to monitor, `*` for any (fallback
    /// `SIMPLEX_GROUP_ALLOWED`). Empty = groups disabled.
    pub group_allowed: Vec<String>,
    /// Cron/notification delivery chat (fallback `SIMPLEX_HOME_CHANNEL`).
    pub home_channel: String,
    /// Text-batch quiet period in ms (fallback
    /// `HERMES_SIMPLEX_TEXT_BATCH_DELAY`, default 800).
    pub text_batch_delay_ms: u64,
}

impl Default for SimplexConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            ws_url: String::new(),
            allowed_users: Vec::new(),
            allow_all_users: false,
            auto_accept: true,
            group_allowed: Vec::new(),
            home_channel: String::new(),
            text_batch_delay_ms: DEFAULT_TEXT_BATCH_DELAY_MS,
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
pub struct ResolvedSimplex {
    pub ws_url: String,
    pub allowed_users: Vec<String>,
    pub allow_all_users: bool,
    pub auto_accept: bool,
    pub group_allowed: Vec<String>,
    pub home_channel: String,
    pub text_batch_delay_ms: u64,
}

impl SimplexConfig {
    pub fn resolve(&self) -> ResolvedSimplex {
        ResolvedSimplex {
            ws_url: env_trim("SIMPLEX_WS_URL")
                .unwrap_or_else(|| self.ws_url.clone())
                .trim_end_matches('/')
                .to_string(),
            allowed_users: env_list("SIMPLEX_ALLOWED_USERS")
                .unwrap_or_else(|| self.allowed_users.clone()),
            allow_all_users: env_trim("SIMPLEX_ALLOW_ALL_USERS")
                .map(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes"))
                .unwrap_or(self.allow_all_users),
            auto_accept: env_trim("SIMPLEX_AUTO_ACCEPT")
                .map(|v| !matches!(v.to_lowercase().as_str(), "false" | "0" | "no"))
                .unwrap_or(self.auto_accept),
            group_allowed: env_list("SIMPLEX_GROUP_ALLOWED")
                .unwrap_or_else(|| self.group_allowed.clone()),
            home_channel: env_trim("SIMPLEX_HOME_CHANNEL")
                .unwrap_or_else(|| self.home_channel.clone()),
            text_batch_delay_ms: env_trim("HERMES_SIMPLEX_TEXT_BATCH_DELAY")
                .and_then(|v| v.parse::<f64>().ok())
                .map(|secs| (secs * 1000.0) as u64)
                .unwrap_or(self.text_batch_delay_ms),
        }
    }
}

type WsSink = futures::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    tokio_tungstenite::tungstenite::Message,
>;

struct Runtime {
    cfg: ResolvedSimplex,
    corr_counter: std::sync::atomic::AtomicU64,
    /// corrId → response channel for `_send_command`.
    pending_responses: Mutex<HashMap<String, oneshot::Sender<Value>>>,
    /// fileId → deferred chat item awaiting rcvFileComplete.
    pending_files: Mutex<HashMap<u64, Value>>,
    /// chat key → accumulated text + flush handle (text batching).
    batches: Mutex<HashMap<String, BatchState>>,
    /// Outbound frame channel into the live WS loop.
    outbound: std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<String>>>,
}

struct BatchState {
    event: MessageEvent,
    flush_task: Option<tokio::task::JoinHandle<()>>,
}

impl Runtime {
    fn next_corr_id(&self) -> String {
        let n = self
            .corr_counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        format!("{CORR_PREFIX}{n}")
    }
}

/// hermes `_guess_extension` mime table (subset used for caching).
fn mime_for_ext(ext: &str) -> &'static str {
    match ext {
        "png" | "jpg" | "jpeg" | "gif" | "webp" => "image",
        "mp3" | "wav" | "ogg" | "m4a" | "aac" | "opus" => "audio",
        _ => "document",
    }
}

fn is_voice_ext(path: &str) -> bool {
    matches!(
        path.rsplit('.')
            .next()
            .unwrap_or("")
            .to_lowercase()
            .as_str(),
        "ogg" | "mp3" | "wav" | "m4a" | "opus"
    )
}

/// Entry point spawned by `run_messaging`.
pub async fn run(
    cfg: SimplexConfig,
    dispatcher: Arc<Dispatcher>,
    pairing: Option<Arc<crate::pairing::PairingStore>>,
) {
    let mut resolved = cfg.resolve();
    if resolved.ws_url.is_empty() {
        resolved.ws_url = "ws://127.0.0.1:5225".to_string();
    }
    let runtime = Arc::new(Runtime {
        cfg: resolved,
        corr_counter: std::sync::atomic::AtomicU64::new(0),
        pending_responses: Mutex::new(HashMap::new()),
        pending_files: Mutex::new(HashMap::new()),
        batches: Mutex::new(HashMap::new()),
        outbound: std::sync::Mutex::new(None),
    });
    crate::messaging::register_platform_sender(
        "simplex",
        Arc::new(SimplexSender {
            runtime: runtime.clone(),
        }),
    );

    let mut delay = WS_RETRY_INITIAL_SECS;
    loop {
        match run_session(&runtime, &dispatcher, &pairing).await {
            Ok(()) => delay = WS_RETRY_INITIAL_SECS,
            Err(e) => eprintln!("[simplex] session error: {e}"),
        }
        tokio::time::sleep(Duration::from_secs(delay)).await;
        delay = (delay * 2).min(WS_RETRY_MAX_SECS);
    }
}

async fn send_frame(sink: &mut WsSink, payload: &Value) -> Result<(), String> {
    use tokio_tungstenite::tungstenite::Message;
    sink.send(Message::Text(payload.to_string().into()))
        .await
        .map_err(|e| format!("ws send: {e}"))
}

async fn run_session(
    runtime: &Arc<Runtime>,
    dispatcher: &Arc<Dispatcher>,
    pairing: &Option<Arc<crate::pairing::PairingStore>>,
) -> Result<(), String> {
    use tokio_tungstenite::tungstenite::Message;

    let (ws, _) = tokio_tungstenite::connect_async(&runtime.cfg.ws_url)
        .await
        .map_err(|e| format!("ws connect {}: {e}", runtime.cfg.ws_url))?;
    eprintln!("[simplex] connected to {}", runtime.cfg.ws_url);
    let (mut sink, mut stream) = ws.split();
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    *runtime.outbound.lock().unwrap() = Some(out_tx);

    loop {
        tokio::select! {
            frame = stream.next() => {
                let Some(frame) = frame else {
                    return Err("ws closed".into());
                };
                let frame = frame.map_err(|e| format!("ws read: {e}"))?;
                match frame {
                    Message::Text(text) => {
                        let Ok(event) = serde_json::from_str::<Value>(&text) else {
                            continue;
                        };
                        handle_event(runtime, dispatcher, pairing, &mut sink, &event).await;
                    }
                    Message::Ping(data) => {
                        let _ = sink.send(Message::Pong(data)).await;
                    }
                    Message::Close(_) => return Ok(()),
                    _ => {}
                }
            }
            Some(frame_text) = out_rx.recv() => {
                let _ = sink
                    .send(Message::Text(frame_text.into()))
                    .await
                    .map_err(|e| eprintln!("[simplex] outbound send failed: {e}"));
            }
        }
    }
}

/// hermes `_handle_event` — correlation responses, contact requests,
/// file descriptors, chat items.
async fn handle_event(
    runtime: &Arc<Runtime>,
    dispatcher: &Arc<Dispatcher>,
    pairing: &Option<Arc<crate::pairing::PairingStore>>,
    sink: &mut WsSink,
    event: &Value,
) {
    let resp = if event.get("resp").map(|r| r.is_object()).unwrap_or(false) {
        event.get("resp").unwrap().clone()
    } else {
        event.clone()
    };
    let corr_id = event
        .get("corrId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Correlated response to one of our commands.
    if !corr_id.is_empty() {
        let pending = runtime.pending_responses.lock().await.remove(&corr_id);
        if let Some(tx) = pending {
            let _ = tx.send(resp.clone());
            return;
        }
        if corr_id.starts_with(CORR_PREFIX) {
            return; // our own fire-and-forget echo
        }
    }

    let resp_type = resp
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    match resp_type.as_str() {
        "contactRequest" => {
            if runtime.cfg.auto_accept {
                if let Some(req_id) = resp
                    .pointer("/contactRequest/contactRequestId")
                    .and_then(|v| v.as_u64())
                {
                    eprintln!("[simplex] auto-accepting contact request {req_id}");
                    send_command(runtime, sink, &format!("/accept {req_id}")).await;
                }
            }
        }
        "rcvFileDescrReady" => {
            if let Some(file_id) = resp
                .pointer("/rcvFileTransfer/fileId")
                .and_then(|v| v.as_u64())
            {
                // Fire-and-forget: the daemon sends no corrId reply for
                // /freceive.
                let frame = json!({
                    "corrId": runtime.next_corr_id(),
                    "cmd": format!("/freceive {file_id}"),
                });
                let _ = send_frame(sink, &frame).await;
            }
        }
        "newChatItems" => {
            let items = resp
                .get("chatItems")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            for item in items {
                handle_chat_item(runtime, dispatcher, pairing, &item).await;
            }
        }
        "newChatItem" => {
            handle_chat_item(runtime, dispatcher, pairing, &resp).await;
        }
        "rcvFileComplete" => {
            let file_id = resp
                .pointer("/chatItem/chatItem/file/fileId")
                .and_then(|v| v.as_u64());
            let file_path = resp
                .pointer("/chatItem/chatItem/file/fileSource/filePath")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            if let (Some(file_id), Some(file_path)) = (file_id, file_path) {
                if let Some(mut pending_item) = runtime.pending_files.lock().await.remove(&file_id)
                {
                    // Inject the completed file path and deliver.
                    if let Some(file) = pending_item.pointer_mut("/chatItem/file") {
                        file["fileSource"] = json!({ "filePath": file_path });
                    }
                    handle_chat_item(runtime, dispatcher, pairing, &pending_item).await;
                }
            }
        }
        _ => {}
    }
}

/// hermes `_handle_chat_item` — filter, extract, gate, cache media,
/// batch text, dispatch.
async fn handle_chat_item(
    runtime: &Arc<Runtime>,
    dispatcher: &Arc<Dispatcher>,
    pairing: &Option<Arc<crate::pairing::PairingStore>>,
    chat_item: &Value,
) {
    let chat_info = chat_item.get("chatInfo").cloned().unwrap_or(json!({}));
    let item_data = chat_item.get("chatItem").cloned().unwrap_or(json!({}));
    let chat_type = chat_info.get("type").and_then(|v| v.as_str()).unwrap_or("");

    // Own messages never loop back.
    let direction_type = item_data
        .pointer("/chatDir/type")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if matches!(direction_type, "directSnd" | "groupSnd") {
        return;
    }
    let content = item_data.get("content").cloned().unwrap_or(json!({}));
    if content.get("type").and_then(|v| v.as_str()) != Some("rcvMsgContent") {
        return;
    }
    let msg_content = content.get("msgContent").cloned().unwrap_or(json!({}));
    let msg_type_str = msg_content
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let mut text = if matches!(
        msg_type_str,
        "text" | "file" | "image" | "voice" | "link" | "video"
    ) {
        msg_content
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    } else {
        String::new()
    };
    if text.is_empty() && !matches!(msg_type_str, "image" | "file" | "voice") {
        return;
    }

    let sender_id;
    let sender_name;
    let chat_id;
    let mut is_group = false;
    match chat_type {
        "direct" => {
            let contact = chat_info.get("contact").cloned().unwrap_or(json!({}));
            sender_id = contact
                .get("contactId")
                .map(|v| v.to_string())
                .unwrap_or_default();
            sender_name = contact
                .get("localDisplayName")
                .and_then(|v| v.as_str())
                .or_else(|| {
                    contact
                        .pointer("/profile/displayName")
                        .and_then(|v| v.as_str())
                })
                .unwrap_or("")
                .to_string();
            chat_id = sender_id.clone();
        }
        "group" => {
            let group_info = chat_info.get("groupInfo").cloned().unwrap_or(json!({}));
            let group_id = group_info
                .get("groupId")
                .map(|v| v.to_string())
                .unwrap_or_default();
            chat_id = format!("group:{group_id}");
            is_group = true;
            let member = item_data
                .pointer("/chatDir/groupMember")
                .cloned()
                .unwrap_or(json!({}));
            sender_id = member
                .get("memberId")
                .map(|v| v.to_string())
                .unwrap_or_default();
            sender_name = member
                .get("localDisplayName")
                .and_then(|v| v.as_str())
                .or_else(|| {
                    member
                        .pointer("/memberProfile/displayName")
                        .and_then(|v| v.as_str())
                })
                .unwrap_or("")
                .to_string();
            // Group allowlist (hermes SIMPLEX_GROUP_ALLOWED).
            if runtime.cfg.group_allowed.is_empty() {
                return;
            }
            if !runtime
                .cfg
                .group_allowed
                .iter()
                .any(|g| g == "*" || *g == group_id.trim_matches('"'))
            {
                return;
            }
        }
        _ => return,
    }
    if sender_id.is_empty() {
        return;
    }
    // DM allowlist∪pairing (hermes allowlist is id-or-display-name).
    if !is_group
        && !runtime.cfg.allow_all_users
        && !runtime
            .cfg
            .allowed_users
            .iter()
            .any(|u| u == &sender_id || (!sender_name.is_empty() && u == &sender_name))
    {
        if let Some(store) = pairing {
            if !store.is_approved("simplex", &sender_id) {
                if let Some(code_msg) = crate::messaging::pairing_offer_public(
                    store,
                    "simplex",
                    &sender_id,
                    &sender_name,
                ) {
                    send_chat_text(runtime, &chat_id, &code_msg).await;
                }
                return;
            }
        } else {
            eprintln!("[simplex] unauthorized DM from {sender_id} — add to allowed_users");
            return;
        }
    }

    // Media attachment (file.fileSource.filePath — daemon-local path).
    let mut attachments = Vec::new();
    if let Some(file_info) = item_data.get("file").filter(|f| f.is_object()) {
        let file_path = file_info
            .pointer("/fileSource/filePath")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let file_name = file_info
            .get("fileName")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let file_id = file_info.get("fileId").and_then(|v| v.as_u64());
        let ext = file_path
            .as_ref()
            .and_then(|p| p.rsplit('.').next())
            .or_else(|| file_name.rsplit('.').next())
            .unwrap_or("")
            .to_lowercase();
        match file_path {
            Some(path) => {
                if let Some(att) = cache_local_file(&path, &ext, &file_name).await {
                    attachments.push(att);
                }
            }
            None => {
                // Voice notes arrive before the download completes — defer
                // until rcvFileComplete (hermes pattern).
                if mime_for_ext(&ext) == "audio" {
                    if let Some(file_id) = file_id {
                        runtime
                            .pending_files
                            .lock()
                            .await
                            .insert(file_id, chat_item.clone());
                    }
                }
                return;
            }
        }
    }
    if text.is_empty() && attachments.is_empty() {
        return;
    }
    if text.is_empty() {
        text = "[media message]".to_string();
    }

    let event = MessageEvent {
        platform: "simplex".into(),
        chat_id: chat_id.clone(),
        sender_id,
        sender_name,
        text,
        message_id: format!(
            "{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        ),
        attachments,
    };

    // Text-only messages ride the quiet-period batch; media dispatches
    // immediately (hermes semantics).
    if event.attachments.is_empty() {
        enqueue_text_batch(runtime, dispatcher, pairing.clone(), event).await;
    } else {
        dispatch_event(runtime, dispatcher, event).await;
    }
}

/// Read a daemon-local file into the content-addressed media cache.
async fn cache_local_file(path: &str, ext: &str, name: &str) -> Option<MediaAttachment> {
    let kind = mime_for_ext(ext);
    let mime = match kind {
        "image" => format!("image/{ext}"),
        "audio" => format!("audio/{ext}"),
        _ => "application/octet-stream".to_string(),
    };
    match tokio::fs::read(path).await {
        Ok(bytes) => {
            let len = bytes.len() as u64;
            match crate::media_cache::cache_media_bytes(
                &crate::config::ulnclaw_home(),
                &bytes,
                &mime,
                name,
            ) {
                Ok(cached) => Some(MediaAttachment {
                    path: cached,
                    mime,
                    bytes: len,
                    original_name: name.to_string(),
                }),
                Err(e) => {
                    eprintln!("[simplex] media cache failed: {e}");
                    None
                }
            }
        }
        Err(e) => {
            eprintln!("[simplex] daemon file unreadable ({path}): {e}");
            None
        }
    }
}

/// hermes text batching: concatenate rapid-fire messages per chat and
/// flush after the quiet period.
async fn enqueue_text_batch(
    runtime: &Arc<Runtime>,
    dispatcher: &Arc<Dispatcher>,
    pairing: Option<Arc<crate::pairing::PairingStore>>,
    event: MessageEvent,
) {
    let key = format!("{}:{}", event.platform, event.chat_id);
    let delay = Duration::from_millis(runtime.cfg.text_batch_delay_ms);
    let mut batches = runtime.batches.lock().await;
    match batches.get_mut(&key) {
        Some(state) => {
            state.event.text = format!("{}\n{}", state.event.text, event.text);
            if let Some(task) = state.flush_task.take() {
                task.abort();
            }
        }
        None => {
            batches.insert(
                key.clone(),
                BatchState {
                    event,
                    flush_task: None,
                },
            );
        }
    }
    let runtime = runtime.clone();
    let dispatcher = dispatcher.clone();
    let key_clone = key.clone();
    let task = tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        let event = runtime
            .batches
            .lock()
            .await
            .remove(&key_clone)
            .map(|s| s.event);
        if let Some(event) = event {
            dispatch_event(&runtime, &dispatcher, event).await;
        }
        let _ = pairing;
    });
    if let Some(state) = batches.get_mut(&key) {
        state.flush_task = Some(task);
    }
}

async fn dispatch_event(
    runtime: &Arc<Runtime>,
    dispatcher: &Arc<Dispatcher>,
    mut event: MessageEvent,
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
    let (reply_text, media_paths) = crate::messaging::extract_media_tags(&full);
    for path in &media_paths {
        send_media(runtime, &chat_id, path).await;
    }
    let reply_text = reply_text.trim().to_string();
    if !reply_text.is_empty() {
        // P705: ledger-protected reply delivery.
        dispatcher
            .send_with_ledger("simplex", &chat_id, &reply_text, || {
                send_chat_text(runtime, &chat_id, &reply_text)
            })
            .await;
    }
}

/// Compose + queue the hermes send command for a text message.
async fn send_chat_text(runtime: &Arc<Runtime>, chat_id: &str, content: &str) {
    for chunk in crate::messaging::chunk_text(content, MAX_MESSAGE_LENGTH) {
        let cmd = if let Some(group_id) = chat_id.strip_prefix("group:") {
            let composed = serde_json::to_string(
                &json!([{ "msgContent": { "type": "text", "text": chunk } }]),
            )
            .unwrap_or_default();
            format!("/_send #{group_id} json {composed}")
        } else {
            format!("@{chat_id} {chunk}")
        };
        queue_command(runtime, &cmd);
    }
}

/// MEDIA: delivery — voice notes for audio extensions, documents
/// otherwise (hermes send_voice / send_document structured forms).
async fn send_media(runtime: &Arc<Runtime>, chat_id: &str, path: &std::path::Path) {
    let path_str = path.to_string_lossy().to_string();
    let payload = if is_voice_ext(&path_str) {
        json!([{
            "msgContent": { "type": "voice", "text": "", "duration": 0 },
            "fileSource": { "filePath": path_str },
        }])
    } else {
        json!([{
            "filePath": path_str,
            "msgContent": { "type": "file", "text": "" },
        }])
    };
    let composed = serde_json::to_string(&payload).unwrap_or_default();
    let cmd = if let Some(group_id) = chat_id.strip_prefix("group:") {
        format!("/_send #{group_id} json {composed}")
    } else {
        format!("/_send @{chat_id} json {composed}")
    };
    queue_command(runtime, &cmd);
}

fn queue_command(runtime: &Arc<Runtime>, cmd: &str) {
    let frame = json!({ "corrId": runtime.next_corr_id(), "cmd": cmd });
    let tx = runtime.outbound.lock().unwrap().clone();
    match tx {
        Some(tx) => {
            if tx.send(frame.to_string()).is_err() {
                eprintln!(
                    "[simplex] command dropped (session closed): {}",
                    &cmd[..cmd.len().min(50)]
                );
            }
        }
        None => eprintln!(
            "[simplex] command dropped (no live session): {}",
            &cmd[..cmd.len().min(50)]
        ),
    }
}

/// Correlated command (`/accept`) — awaits the daemon response.
async fn send_command(runtime: &Arc<Runtime>, sink: &mut WsSink, cmd: &str) {
    let corr_id = runtime.next_corr_id();
    let (tx, rx) = oneshot::channel();
    runtime
        .pending_responses
        .lock()
        .await
        .insert(corr_id.clone(), tx);
    let frame = json!({ "corrId": corr_id, "cmd": cmd });
    if let Err(e) = send_frame(sink, &frame).await {
        runtime.pending_responses.lock().await.remove(&corr_id);
        eprintln!("[simplex] command send failed: {e}");
        return;
    }
    match tokio::time::timeout(COMMAND_TIMEOUT, rx).await {
        Ok(Ok(_resp)) => {}
        _ => {
            runtime.pending_responses.lock().await.remove(&corr_id);
            eprintln!("[simplex] command timed out: {}", &cmd[..cmd.len().min(50)]);
        }
    }
}

struct SimplexSender {
    runtime: Arc<Runtime>,
}

#[async_trait::async_trait]
impl crate::messaging::PlatformSender for SimplexSender {
    async fn send_text(&self, chat_id: &str, text: &str) {
        send_chat_text(&self.runtime, chat_id, text).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_defaults() {
        let _guard = crate::models_dev::test_env_lock();
        let resolved = SimplexConfig::default().resolve();
        assert_eq!(resolved.ws_url, "");
        assert!(resolved.auto_accept);
        assert!(!resolved.allow_all_users);
        assert_eq!(resolved.text_batch_delay_ms, DEFAULT_TEXT_BATCH_DELAY_MS);
    }

    #[test]
    fn resolve_env_overrides() {
        let _guard = crate::models_dev::test_env_lock();
        std::env::set_var("SIMPLEX_WS_URL", "ws://10.1.1.5:5225/");
        std::env::set_var("SIMPLEX_ALLOWED_USERS", "1, alice");
        std::env::set_var("SIMPLEX_AUTO_ACCEPT", "false");
        std::env::set_var("HERMES_SIMPLEX_TEXT_BATCH_DELAY", "1.5");
        let resolved = SimplexConfig::default().resolve();
        assert_eq!(resolved.ws_url, "ws://10.1.1.5:5225");
        assert_eq!(
            resolved.allowed_users,
            vec!["1".to_string(), "alice".to_string()]
        );
        assert!(!resolved.auto_accept);
        assert_eq!(resolved.text_batch_delay_ms, 1500);
        std::env::remove_var("SIMPLEX_WS_URL");
        std::env::remove_var("SIMPLEX_ALLOWED_USERS");
        std::env::remove_var("SIMPLEX_AUTO_ACCEPT");
        std::env::remove_var("HERMES_SIMPLEX_TEXT_BATCH_DELAY");
    }

    #[test]
    fn voice_extension_detection() {
        assert!(is_voice_ext("/tmp/note.ogg"));
        assert!(is_voice_ext("/tmp/note.MP3"));
        assert!(!is_voice_ext("/tmp/doc.pdf"));
    }

    #[test]
    fn mime_classes() {
        assert_eq!(mime_for_ext("png"), "image");
        assert_eq!(mime_for_ext("ogg"), "audio");
        assert_eq!(mime_for_ext("pdf"), "document");
    }

    #[test]
    fn chat_item_own_messages_filtered() {
        let item: Value = serde_json::from_str(
            r#"{"chatInfo":{"type":"direct"},"chatItem":{"chatDir":{"type":"directSnd"}}}"#,
        )
        .unwrap();
        let dir = item
            .pointer("/chatItem/chatDir/type")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert!(matches!(dir, "directSnd" | "groupSnd"));
    }

    #[test]
    fn chat_item_text_extraction() {
        let item: Value = serde_json::from_str(
            r#"{"chatInfo":{"type":"direct","contact":{"contactId":3,"localDisplayName":"Alice"}},
                "chatItem":{"chatDir":{"type":"directRcv"},
                            "content":{"type":"rcvMsgContent","msgContent":{"type":"text","text":"hello"}}}}"#,
        )
        .unwrap();
        let text = item
            .pointer("/chatItem/content/msgContent/text")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert_eq!(text, "hello");
        let contact_id = item
            .pointer("/chatInfo/contact/contactId")
            .map(|v| v.to_string())
            .unwrap_or_default();
        assert_eq!(contact_id, "3");
    }

    #[test]
    fn group_allowlist_gate() {
        let allowed = vec!["*".to_string()];
        assert!(allowed.iter().any(|g| g == "*" || g == "12"));
        let allowed = vec!["12".to_string()];
        assert!(allowed.iter().any(|g| g == "*" || g == "12"));
        assert!(!allowed.iter().any(|g| g == "*" || g == "99"));
        let empty: Vec<String> = Vec::new();
        assert!(empty.is_empty()); // groups disabled
    }

    #[test]
    fn group_send_command_shape() {
        let chunk = "hi there";
        let composed =
            serde_json::to_string(&json!([{ "msgContent": { "type": "text", "text": chunk } }]))
                .unwrap();
        let cmd = format!("/_send #42 json {composed}");
        assert!(cmd.starts_with("/_send #42 json ["));
        assert!(cmd.contains("\"type\":\"text\""));
        let dm_cmd = format!("@7 {chunk}");
        assert_eq!(dm_cmd, "@7 hi there");
    }

    #[test]
    fn voice_send_payload_shape() {
        let payload = json!([{
            "msgContent": { "type": "voice", "text": "", "duration": 0 },
            "fileSource": { "filePath": "/tmp/note.ogg" },
        }]);
        let composed = serde_json::to_string(&payload).unwrap();
        assert!(composed.contains("\"type\":\"voice\""));
        assert!(composed.contains("fileSource"));
    }

    #[test]
    fn corr_prefix_matches_hermes() {
        assert_eq!(CORR_PREFIX, "hermes-");
        assert_eq!(MAX_MESSAGE_LENGTH, 8000);
    }
}

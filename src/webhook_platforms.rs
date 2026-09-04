//! Webhook-based messaging platforms — WhatsApp Cloud + Microsoft Graph
//! change-notification ingress (hermes `gateway/platforms/whatsapp_cloud.py`
//! and `gateway/platforms/msgraph_webhook.py` ports).
//!
//! Unlike the polling/websocket adapters in `messaging.rs`, these platforms
//! receive HTTP callbacks, so they mount as routes on the gateway router:
//!
//! * `GET  /webhooks/whatsapp`  — Meta verify handshake (hub.challenge echo)
//! * `POST /webhooks/whatsapp`  — inbound messages (HMAC-SHA256 verified)
//! * `GET/POST /webhooks/msgraph` — Graph validation + change notifications
//!
//! Outbound WhatsApp replies go through the Graph API messages endpoint;
//! both platforms share the messaging allowlist∪pairing union gate.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::Mutex;

use crate::error::{AgentError, Result};
use crate::messaging::{Dispatcher, MessageEvent};

/// Outbound chunk ceiling (hermes `_outgoing_chunk_limit`).
const WHATSAPP_CHUNK_LIMIT: usize = 4096;
/// Inbound webhook body cap (hermes `_read_limited_request_body`).
const MAX_WEBHOOK_BODY_BYTES: usize = 1024 * 1024;

// ---------------------------------------------------------------------------
// WhatsApp Cloud (hermes whatsapp_cloud.py)
// ---------------------------------------------------------------------------

/// `[messaging.whatsapp_cloud]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WhatsAppCloudConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Graph API permanent access token (system user recommended).
    #[serde(default)]
    pub access_token: String,
    /// The business phone number id that receives messages.
    #[serde(default)]
    pub phone_number_id: String,
    /// Shared secret for Meta's `hub.verify_token` handshake.
    #[serde(default)]
    pub verify_token: String,
    /// App secret for `X-Hub-Signature-256` HMAC verification.
    #[serde(default)]
    pub app_secret: String,
    /// Sender phone numbers (wa_id) allowed to talk to the bot.
    /// Empty = pairing-only (fail closed until someone pairs).
    #[serde(default)]
    pub allowed_sender_ids: Vec<String>,
    /// Graph API version prefix.
    #[serde(default = "default_graph_version")]
    pub graph_version: String,
}

fn default_graph_version() -> String {
    "v20.0".to_string()
}

impl WhatsAppCloudConfig {
    fn graph_url(&self, path: &str) -> String {
        format!("https://graph.facebook.com/{}/{path}", self.graph_version)
    }
}

/// Meta verify handshake (hermes `_handle_verify`): `hub.mode=subscribe` +
/// matching `hub.verify_token` → echo `hub.challenge`, else 403.
pub fn whatsapp_verify(cfg: &WhatsAppCloudConfig, query: &[(String, String)]) -> Result<String> {
    let get = |key: &str| {
        query
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    if cfg.verify_token.is_empty() {
        return Err(AgentError::config(
            "whatsapp_cloud verify_token is not configured",
        ));
    }
    if get("hub.mode") == "subscribe" && get("hub.verify_token") == cfg.verify_token {
        Ok(get("hub.challenge"))
    } else {
        Err(AgentError::config("whatsapp verify handshake failed"))
    }
}

/// Verify `X-Hub-Signature-256` over the raw body (hermes
/// `_verify_signature`, constant-time). Missing app_secret disables the
/// check (hermes warns; the verify-token handshake still gates setup).
pub fn whatsapp_signature_ok(raw_body: &[u8], header: &str, app_secret: &str) -> bool {
    if app_secret.is_empty() {
        return true;
    }
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let Some(provided_hex) = header.strip_prefix("sha256=") else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(app_secret.as_bytes()) else {
        return false;
    };
    mac.update(raw_body);
    let expected = mac.finalize().into_bytes();
    let provided = hex_bytes(provided_hex);
    provided.len() == expected.len() && constant_time_eq_bytes(&provided, expected.as_slice())
}

fn hex_bytes(text: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut chars = text.chars();
    while let (Some(a), Some(b)) = (chars.next(), chars.next()) {
        match u8::from_str_radix(&format!("{a}{b}"), 16) {
            Ok(byte) => out.push(byte),
            Err(_) => return Vec::new(),
        }
    }
    out
}

fn constant_time_eq_bytes(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

/// Parse one webhook payload into text message events (hermes inbound
/// shape: entry[].changes[].value.messages[], type=text).
/// One inbound media object referenced by a WhatsApp message (downloaded
/// via the Graph `/media` endpoint before dispatch).
#[derive(Debug, Clone)]
pub struct WaMediaRef {
    pub media_id: String,
    pub mime: String,
    pub kind: String,
    pub filename: String,
}

/// Parse inbound webhook payloads into events plus pending media
/// references (index-aligned with the returned events). Text messages
/// carry no media; image/document/audio/video/sticker messages carry
/// their Graph media ids (hermes inbound media pipeline).
pub fn whatsapp_parse_messages_full(payload: &Value) -> Vec<(MessageEvent, Vec<WaMediaRef>)> {
    let mut parsed = Vec::new();
    let Some(entries) = payload.get("entry").and_then(|v| v.as_array()) else {
        return parsed;
    };
    for entry in entries {
        let Some(changes) = entry.get("changes").and_then(|v| v.as_array()) else {
            continue;
        };
        for change in changes {
            let Some(messages) = change.pointer("/value/messages").and_then(|v| v.as_array())
            else {
                continue;
            };
            for message in messages {
                let msg_type = message.get("type").and_then(|v| v.as_str()).unwrap_or("");
                let from = message
                    .get("from")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if from.is_empty() {
                    continue;
                }
                let message_id = message
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let text: String;
                let mut media = Vec::new();
                match msg_type {
                    "text" => {
                        text = message
                            .pointer("/text/body")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        if text.is_empty() {
                            continue;
                        }
                    }
                    "image" | "document" | "audio" | "video" | "sticker" => {
                        let Some(body) = message.get(msg_type) else {
                            continue;
                        };
                        let media_id = body
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        if media_id.is_empty() {
                            continue;
                        }
                        media.push(WaMediaRef {
                            media_id,
                            mime: body
                                .get("mime_type")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                            kind: msg_type.to_string(),
                            filename: body
                                .get("filename")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                        });
                        // image/document carry optional captions.
                        text = body
                            .get("caption")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                    }
                    _ => {
                        // interactive/contacts/location/unknown — skipped
                        // like hermes (text + media are the ported types).
                        continue;
                    }
                }
                parsed.push((
                    MessageEvent {
                        platform: "whatsapp_cloud".into(),
                        chat_id: from.clone(),
                        sender_id: from,
                        sender_name: String::new(),
                        text,
                        message_id,
                        attachments: Vec::new(),
                    },
                    media,
                ));
            }
        }
    }
    parsed
}

/// Text-only view (kept for callers that don't care about media).
pub fn whatsapp_parse_messages(payload: &Value) -> Vec<MessageEvent> {
    whatsapp_parse_messages_full(payload)
        .into_iter()
        .map(|(event, _)| event)
        .collect()
}

/// Per-type size caps documented by Meta for the Cloud API /media
/// endpoint (hermes `_MEDIA_SIZE_LIMITS`) — refuse downloads/uploads
/// above them with a clean error instead of round-tripping to Graph.
pub fn whatsapp_media_cap(kind: &str) -> u64 {
    match kind {
        "image" => 5 * 1024 * 1024,
        "video" => 16 * 1024 * 1024,
        "audio" => 16 * 1024 * 1024,
        "sticker" => 500 * 1024,
        _ => 100 * 1024 * 1024, // document
    }
}

/// Default mime when the payload omits one (hermes `_DEFAULT_MIME`).
fn whatsapp_default_mime(kind: &str) -> &'static str {
    match kind {
        "image" => "image/jpeg",
        "video" => "video/mp4",
        "audio" => "audio/mpeg",
        "sticker" => "image/webp",
        _ => "application/octet-stream",
    }
}

/// Download one inbound media object: `GET /media/{id}` → signed URL →
/// fetch with bearer auth → cache content-addressed (hermes inbound
/// media pipeline). Returns the cached attachment, or None with a
/// logged reason.
pub async fn whatsapp_download_media(
    client: &reqwest::Client,
    cfg: &WhatsAppCloudConfig,
    media_ref: &WaMediaRef,
) -> Option<crate::messaging::MediaAttachment> {
    let meta_url = cfg.graph_url(&media_ref.media_id);
    let meta = match client
        .get(&meta_url)
        .header("Authorization", format!("Bearer {}", cfg.access_token))
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => response,
        Ok(response) => {
            eprintln!(
                "[whatsapp_cloud] media metadata failed ({})",
                response.status()
            );
            return None;
        }
        Err(e) => {
            eprintln!("[whatsapp_cloud] media metadata request failed: {e}");
            return None;
        }
    };
    let meta_value: Value = match meta.json().await {
        Ok(value) => value,
        Err(e) => {
            eprintln!("[whatsapp_cloud] media metadata unparseable: {e}");
            return None;
        }
    };
    let url = match meta_value.get("url").and_then(|v| v.as_str()) {
        Some(url) => url.to_string(),
        None => {
            eprintln!("[whatsapp_cloud] media metadata missing url");
            return None;
        }
    };
    let cap = whatsapp_media_cap(&media_ref.kind);
    let data = match client
        .get(&url)
        .header("Authorization", format!("Bearer {}", cfg.access_token))
        .send()
        .await
        .and_then(|response| response.error_for_status())
    {
        Ok(response) => match response.bytes().await {
            Ok(bytes) => bytes,
            Err(e) => {
                eprintln!("[whatsapp_cloud] media download failed: {e}");
                return None;
            }
        },
        Err(e) => {
            eprintln!("[whatsapp_cloud] media download failed: {e}");
            return None;
        }
    };
    if data.len() as u64 > cap {
        eprintln!(
            "[whatsapp_cloud] media {} exceeds the {} cap ({} bytes) — skipped",
            media_ref.kind,
            cap,
            data.len()
        );
        return None;
    }
    let mime = if media_ref.mime.trim().is_empty() {
        whatsapp_default_mime(&media_ref.kind).to_string()
    } else {
        media_ref.mime.clone()
    };
    let home = crate::config::ulnclaw_home();
    match crate::media_cache::cache_media_bytes(&home, &data, &mime, &media_ref.filename) {
        Ok(path) => Some(crate::messaging::MediaAttachment {
            path,
            mime,
            bytes: data.len() as u64,
            original_name: media_ref.filename.clone(),
        }),
        Err(e) => {
            eprintln!("[whatsapp_cloud] media cache write failed: {e}");
            None
        }
    }
}

/// Upload a local file to the Graph `/media` endpoint (hermes
/// `_upload_media`, step one of the two-step send). Returns the media id.
pub async fn whatsapp_upload_media(
    client: &reqwest::Client,
    cfg: &WhatsAppCloudConfig,
    path: &std::path::Path,
) -> Result<String> {
    let data = std::fs::read(path)
        .map_err(|e| AgentError::Tool(format!("read {}: {e}", path.display())))?;
    let kind = crate::media_cache::media_kind(&crate::media_cache::mime_for_ext(path));
    let cap = whatsapp_media_cap(kind);
    if data.len() as u64 > cap {
        return Err(AgentError::Tool(format!(
            "{} exceeds WhatsApp's {} limit ({} bytes)",
            path.display(),
            kind,
            data.len()
        )));
    }
    let mime = crate::media_cache::mime_for_ext(path);
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    let part = reqwest::multipart::Part::bytes(data)
        .file_name(file_name)
        .mime_str(&mime)
        .map_err(|e| AgentError::Tool(format!("multipart: {e}")))?;
    let form = reqwest::multipart::Form::new()
        .text("messaging_product", "whatsapp")
        .part("file", part);
    let response = client
        .post(cfg.graph_url("media"))
        .header("Authorization", format!("Bearer {}", cfg.access_token))
        .multipart(form)
        .send()
        .await
        .map_err(|e| AgentError::Tool(format!("whatsapp media upload: {e}")))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(AgentError::Tool(format!(
            "whatsapp media upload failed ({status}): {body}"
        )));
    }
    let value: Value = response
        .json()
        .await
        .map_err(|e| AgentError::Tool(format!("whatsapp media upload parse: {e}")))?;
    value
        .get("id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| AgentError::Tool("whatsapp media upload returned no id".to_string()))
}

/// Send an uploaded media object (hermes `_send_media`, step two).
pub async fn whatsapp_send_media(
    client: &reqwest::Client,
    cfg: &WhatsAppCloudConfig,
    to: &str,
    media_id: &str,
    path: &std::path::Path,
) -> Result<()> {
    let kind = crate::media_cache::media_kind(&crate::media_cache::mime_for_ext(path));
    let send_kind = match kind {
        "image" => "image",
        "video" => "video",
        "audio" => "audio",
        "sticker" => "sticker",
        _ => "document",
    };
    let mut media_obj = json!({ "id": media_id });
    if send_kind == "document" || send_kind == "image" || send_kind == "video" {
        if let Some(name) = path.file_name() {
            media_obj["filename"] = json!(name.to_string_lossy());
        }
    }
    let payload = json!({
        "messaging_product": "whatsapp",
        "recipient_type": "individual",
        "to": to,
        "type": send_kind,
        send_kind: media_obj,
    });
    let response = client
        .post(cfg.graph_url("messages"))
        .header("Authorization", format!("Bearer {}", cfg.access_token))
        .json(&payload)
        .send()
        .await
        .map_err(|e| AgentError::Tool(format!("whatsapp media send: {e}")))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(AgentError::Tool(format!(
            "whatsapp media send failed ({status}): {body}"
        )));
    }
    Ok(())
}

/// Send a text reply via the Graph API (hermes `send`): chunked, bearer
/// auth, `messaging_product: whatsapp`.
pub async fn whatsapp_send(
    client: &reqwest::Client,
    cfg: &WhatsAppCloudConfig,
    to: &str,
    text: &str,
) -> Result<()> {
    if text.trim().is_empty() {
        return Ok(());
    }
    let url = cfg.graph_url("messages");
    for chunk in crate::messaging::chunk_text(text, WHATSAPP_CHUNK_LIMIT) {
        let payload = json!({
            "messaging_product": "whatsapp",
            "recipient_type": "individual",
            "to": to,
            "type": "text",
            "text": {"body": chunk, "preview_url": true},
        });
        let response = client
            .post(&url)
            .header("Authorization", format!("Bearer {}", cfg.access_token))
            .json(&payload)
            .send()
            .await
            .map_err(|e| AgentError::Tool(format!("whatsapp send: {e}")))?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(AgentError::Tool(format!(
                "whatsapp send failed ({status}): {body}"
            )));
        }
    }
    Ok(())
}

/// Full inbound handling: signature → parse → authz union (allowlist OR
/// pairing) → dispatch → reply. Returns Ok(true) when the payload was
/// accepted (200 to Meta either way, hermes ack semantics).
pub async fn whatsapp_handle_webhook(
    cfg: &WhatsAppCloudConfig,
    dispatcher: &Arc<Dispatcher>,
    pairing: Option<&crate::pairing::PairingStore>,
    raw_body: &[u8],
    signature_header: &str,
) -> Result<()> {
    if raw_body.len() > MAX_WEBHOOK_BODY_BYTES {
        return Err(AgentError::config("whatsapp webhook body too large"));
    }
    if !whatsapp_signature_ok(raw_body, signature_header, &cfg.app_secret) {
        return Err(AgentError::config(
            "whatsapp webhook rejected: invalid X-Hub-Signature-256",
        ));
    }
    let payload: Value = serde_json::from_slice(raw_body)
        .map_err(|e| AgentError::config(format!("whatsapp webhook parse: {e}")))?;
    // Clarify prompts render through the registered platform sender.
    crate::messaging::register_platform_sender(
        "whatsapp_cloud",
        Arc::new(WhatsAppSender { cfg: cfg.clone() }),
    );
    // Interactive taps (clarify buttons/list rows) route to their
    // resolvers BEFORE normal dispatch (hermes ordering); unclaimed taps
    // fall back to text dispatch with the button title as the message.
    for tap in whatsapp_parse_interactive_replies(&payload) {
        let authorized = cfg
            .allowed_sender_ids
            .iter()
            .any(|allowed| *allowed == tap.sender_id)
            || pairing
                .map(|store| store.is_approved("whatsapp_cloud", &tap.sender_id))
                .unwrap_or(false);
        if !authorized {
            // Claim the tap without dispatching it (hermes semantics):
            // unauthorized taps must not re-enter the agent loop as text.
            eprintln!(
                "[whatsapp_cloud] rejected unauthorized interactive tap from {} (button_id={})",
                tap.sender_id, tap.button_id
            );
            continue;
        }
        if whatsapp_dispatch_interactive_tap(cfg, &tap).await {
            continue;
        }
        // Stale tap / unrecognized id → treat the title as a normal
        // text message (hermes graceful fallback).
        if tap.title.trim().is_empty() {
            continue;
        }
        let fallback_event = crate::messaging::MessageEvent {
            platform: "whatsapp_cloud".into(),
            chat_id: tap.chat_id.clone(),
            sender_id: tap.sender_id.clone(),
            sender_name: String::new(),
            text: tap.title.clone(),
            message_id: String::new(),
            attachments: Vec::new(),
        };
        let outcome = match dispatcher.handle_event(fallback_event).await {
            Ok(outcome) => outcome,
            Err(e) => crate::messaging::DispatchOutcome {
                reply: format!("error: {e}"),
                transcript_echoes: Vec::new(),
            },
        };
        let client = reqwest::Client::new();
        for echo in &outcome.transcript_echoes {
            if let Err(e) = whatsapp_send(&client, cfg, &tap.chat_id, echo).await {
                eprintln!("[whatsapp_cloud] transcript echo failed: {e}");
            }
        }
        if let Err(e) = whatsapp_send(&client, cfg, &tap.chat_id, &outcome.reply).await {
            eprintln!("[whatsapp_cloud] tap-fallback reply failed: {e}");
        }
    }
    let parsed = whatsapp_parse_messages_full(&payload);
    for (mut event, media_refs) in parsed {
        // Download inbound media into the content-addressed cache before
        // dispatch (hermes inbound media pipeline; failures degrade to a
        // text note via the attachment pipeline, never fatal).
        if !media_refs.is_empty() {
            let client = reqwest::Client::new();
            for media_ref in &media_refs {
                if let Some(attachment) = whatsapp_download_media(&client, cfg, media_ref).await {
                    event.attachments.push(attachment);
                }
            }
        }
        let authorized = cfg
            .allowed_sender_ids
            .iter()
            .any(|allowed| *allowed == event.sender_id)
            || pairing
                .map(|store| store.is_approved("whatsapp_cloud", &event.sender_id))
                .unwrap_or(false);
        if !authorized {
            eprintln!(
                "[whatsapp_cloud] refusing message from {} — add it to \
                 messaging.whatsapp_cloud.allowed_sender_ids or approve a pairing code",
                event.sender_id
            );
            if let Some(store) = pairing {
                if let Some(reply) = crate::messaging::pairing_offer_public(
                    store,
                    "whatsapp_cloud",
                    &event.sender_id,
                    &event.sender_name,
                ) {
                    let client = reqwest::Client::new();
                    if let Err(e) = whatsapp_send(&client, cfg, &event.chat_id, &reply).await {
                        eprintln!("[whatsapp_cloud] pairing reply failed: {e}");
                    }
                }
            }
            continue;
        }
        // Pre-dispatch plugin gate (hermes ordering parity).
        if !crate::messaging::pre_gateway_dispatch_gate_public(&mut event).await {
            continue;
        }
        let outcome = match dispatcher.handle_event(event.clone()).await {
            Ok(outcome) => outcome,
            Err(e) => crate::messaging::DispatchOutcome {
                reply: format!("error: {e}"),
                transcript_echoes: Vec::new(),
            },
        };
        let client = reqwest::Client::new();
        for echo in &outcome.transcript_echoes {
            if let Err(e) = whatsapp_send(&client, cfg, &event.chat_id, echo).await {
                eprintln!("[whatsapp_cloud] transcript echo failed: {e}");
            }
        }
        let reply = outcome.reply;
        let (reply_text, media_paths) = crate::messaging::extract_media_tags(&reply);
        // P704: ledger-protected reply delivery.
        dispatcher
            .try_send_with_ledger("whatsapp_cloud", &event.chat_id, &reply_text, || async {
                match whatsapp_send(&client, cfg, &event.chat_id, &reply_text).await {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        eprintln!("[whatsapp_cloud] reply failed: {e}");
                        Err(e.to_string())
                    }
                }
            })
            .await;
        for path in &media_paths {
            match whatsapp_upload_media(&client, cfg, path).await {
                Ok(media_id) => {
                    if let Err(e) =
                        whatsapp_send_media(&client, cfg, &event.chat_id, &media_id, path).await
                    {
                        eprintln!("[whatsapp_cloud] media delivery failed: {e}");
                    }
                }
                Err(e) => eprintln!("[whatsapp_cloud] media upload failed: {e}"),
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Microsoft Graph change-notification ingress (hermes msgraph_webhook.py)
// ---------------------------------------------------------------------------

/// `[messaging.msgraph]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MsGraphConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Shared secret Graph echoes back on every notification — required
    /// (hermes refuses to start without it).
    #[serde(default)]
    pub client_state: String,
    /// Resource patterns accepted for dispatch (hermes
    /// `accepted_resources`): exact, sub-path prefix (`path` matches
    /// `path/...` too) or wildcard prefix (`path*`). Empty = accept all.
    #[serde(default)]
    pub accepted_resources: Vec<String>,
    /// Custom prompt template (hermes `extra.prompt`): `{dotted.path}`
    /// placeholders resolve against the notification payload; empty
    /// renders the pretty-printed JSON dump instead.
    #[serde(default)]
    pub prompt: String,
    /// Receipt dedup ledger size (hermes `max_seen_receipts`, default
    /// 5000).
    #[serde(default)]
    pub max_seen_receipts: usize,
}

/// hermes `DEFAULT_MAX_SEEN_RECEIPTS`.
const MSGRAPH_DEFAULT_MAX_SEEN_RECEIPTS: usize = 5000;

/// Graph subscription validation: echo `validationToken` as `text/plain`
/// (hermes `_handle_validation`).
pub fn msgraph_validation_token(query: &[(String, String)]) -> Option<String> {
    query
        .iter()
        .find(|(k, _)| k == "validationToken")
        .map(|(_, v)| v.clone())
        .filter(|v| !v.is_empty())
}

/// Verify one notification's clientState (hermes `_verify_client_state`).
pub fn msgraph_state_ok(notification: &Value, expected: &str) -> bool {
    notification
        .get("clientState")
        .and_then(|v| v.as_str())
        .map(|state| constant_time_eq_bytes(state.as_bytes(), expected.as_bytes()))
        .unwrap_or(false)
}

/// hermes `_build_receipt_key`.
pub fn msgraph_receipt_key(notification: &Value) -> Option<String> {
    let explicit_id = notification
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if explicit_id.is_empty() {
        None
    } else {
        Some(format!("id:{explicit_id}"))
    }
}

fn msgraph_receipts() -> &'static Mutex<(HashSet<String>, VecDeque<String>)> {
    static RECEIPTS: std::sync::OnceLock<Mutex<(HashSet<String>, VecDeque<String>)>> =
        std::sync::OnceLock::new();
    RECEIPTS.get_or_init(|| Mutex::new((HashSet::new(), VecDeque::new())))
}

/// hermes `_has_seen_receipt` + `_remember_receipt` — FIFO-evicting
/// dedup ledger. Returns true when the key was already seen.
fn msgraph_remember_receipt(key: &str, max_seen: usize) -> bool {
    let mut guard = msgraph_receipts().lock().unwrap();
    if guard.0.contains(key) {
        return true;
    }
    guard.0.insert(key.to_string());
    guard.1.push_back(key.to_string());
    let cap = max_seen.max(1);
    while guard.1.len() > cap {
        if let Some(oldest) = guard.1.pop_front() {
            guard.0.remove(&oldest);
        }
    }
    false
}

/// hermes `_normalize_resource_value`.
fn normalize_resource_value(resource: &str) -> String {
    resource.trim().trim_matches('/').to_string()
}

/// hermes `_resource_accepted` — exact, sub-path prefix, or `prefix*`
/// wildcard matching.
pub fn msgraph_resource_accepted(resource: &str, accepted_resources: &[String]) -> bool {
    if accepted_resources.is_empty() {
        return true;
    }
    let normalized_resource = normalize_resource_value(resource);
    for pattern in accepted_resources {
        let normalized_pattern = normalize_resource_value(pattern);
        if normalized_pattern.is_empty() {
            continue;
        }
        if let Some(prefix) = normalized_pattern.strip_suffix('*') {
            let prefix = prefix.trim_end_matches('/');
            if normalized_resource == prefix
                || normalized_resource.starts_with(&format!("{prefix}/"))
            {
                return true;
            }
            continue;
        }
        if normalized_resource == normalized_pattern
            || normalized_resource.starts_with(&format!("{normalized_pattern}/"))
        {
            return true;
        }
    }
    false
}

/// hermes `json.dumps(..., sort_keys=True)` — serde maps keep insertion
/// order, so rebuild through BTreeMap recursively.
fn sorted_json(value: &Value, pretty: bool) -> String {
    fn to_sorted(value: &Value) -> Value {
        match value {
            Value::Object(map) => {
                let sorted: std::collections::BTreeMap<String, Value> =
                    map.iter().map(|(k, v)| (k.clone(), to_sorted(v))).collect();
                Value::Object(sorted.into_iter().collect())
            }
            Value::Array(items) => Value::Array(items.iter().map(to_sorted).collect()),
            other => other.clone(),
        }
    }
    let sorted = to_sorted(value);
    if pretty {
        serde_json::to_string_pretty(&sorted).unwrap_or_default()
    } else {
        serde_json::to_string(&sorted).unwrap_or_default()
    }
}

fn sha1_hex(data: &str) -> String {
    use sha1::Digest;
    let digest = sha1::Sha1::digest(data.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// hermes `_render_template` — resolve `{dotted.path}` placeholders
/// against the payload map; unresolved placeholders stay literal,
/// dict/list values render as compact JSON truncated to 2000 chars.
pub fn msgraph_render_template(template: &str, payload: &Value) -> String {
    let re = regex::Regex::new(r"\{([a-zA-Z0-9_.]+)\}").expect("static template regex");
    re.replace_all(template, |caps: &regex::Captures| {
        let key = &caps[1];
        let mut value: &Value = payload;
        for part in key.split('.') {
            match value.get(part) {
                Some(next) => value = next,
                None => return format!("{{{key}}}"),
            }
        }
        match value {
            Value::Object(_) | Value::Array(_) => {
                let compact = serde_json::to_string(value).unwrap_or_default();
                compact.chars().take(2000).collect()
            }
            Value::String(text) => text.clone(),
            other => other.to_string(),
        }
    })
    .to_string()
}

/// hermes `_render_prompt` — custom template or the pretty-printed JSON
/// dump (4000-char budget).
pub fn msgraph_render_prompt(notification: &Value, template: &str) -> String {
    if !template.trim().is_empty() {
        let payload = json!({
            "notification": notification,
            "resource": notification.get("resource").cloned().unwrap_or(Value::String(String::new())),
            "change_type": notification.get("changeType").cloned().unwrap_or(Value::String(String::new())),
            "subscription_id": notification.get("subscriptionId").cloned().unwrap_or(Value::String(String::new())),
        });
        return msgraph_render_template(template, &payload);
    }
    let rendered: String = sorted_json(notification, true).chars().take(4000).collect();
    format!("Microsoft Graph change notification:\n\n```json\n{rendered}\n```")
}

/// Batch outcome — hermes accepted/duplicate/auth-rejected/other-rejected
/// counters drive the HTTP status (202 / 403 / 400).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MsGraphOutcome {
    pub accepted: usize,
    pub duplicates: usize,
    pub auth_rejected: usize,
    pub other_rejected: usize,
}

/// Handle a Graph notification batch: validate clientState per
/// notification, filter resources, dedup receipts, and dispatch accepted
/// ones as internal events (hermes `_handle_notification`).
pub async fn msgraph_handle_webhook(
    cfg: &MsGraphConfig,
    dispatcher: &Arc<Dispatcher>,
    raw_body: &[u8],
    query: &[(String, String)],
) -> Result<MsGraphOutcome> {
    if raw_body.len() > MAX_WEBHOOK_BODY_BYTES {
        return Err(AgentError::config("msgraph webhook body too large"));
    }
    // Graph may tack a validationToken onto a POST right after subscription.
    if msgraph_validation_token(query).is_some() {
        return Ok(MsGraphOutcome::default());
    }
    let payload: Value = serde_json::from_slice(raw_body)
        .map_err(|e| AgentError::config(format!("msgraph webhook parse: {e}")))?;
    let Some(notifications) = payload.get("value").and_then(|v| v.as_array()) else {
        return Err(AgentError::config(
            "msgraph webhook payload missing value array",
        ));
    };
    let max_seen = if cfg.max_seen_receipts == 0 {
        MSGRAPH_DEFAULT_MAX_SEEN_RECEIPTS
    } else {
        cfg.max_seen_receipts
    };
    let mut outcome = MsGraphOutcome::default();
    for notification in notifications {
        if !notification.is_object() {
            outcome.other_rejected += 1;
            continue;
        }
        let resource = notification
            .get("resource")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !msgraph_resource_accepted(resource, &cfg.accepted_resources) {
            outcome.other_rejected += 1;
            continue;
        }
        if cfg.client_state.is_empty() || !msgraph_state_ok(notification, &cfg.client_state) {
            outcome.auth_rejected += 1;
            continue;
        }
        let receipt_key = msgraph_receipt_key(notification);
        if let Some(key) = &receipt_key {
            if msgraph_remember_receipt(key, max_seen) {
                outcome.duplicates += 1;
                continue;
            }
        }
        outcome.accepted += 1;
        let subscription_id = notification
            .get("subscriptionId")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        // hermes `_build_message_event`: receipt id, else sha1 over the
        // sorted notification JSON.
        let message_id = receipt_key
            .unwrap_or_else(|| format!("sha1:{}", sha1_hex(&sorted_json(notification, false))));
        let event = MessageEvent {
            platform: "msgraph".into(),
            chat_id: format!("msgraph:{subscription_id}"),
            sender_id: "msgraph".into(),
            sender_name: "Microsoft Graph".into(),
            text: msgraph_render_prompt(notification, &cfg.prompt),
            message_id,
            attachments: Vec::new(),
        };
        if let Err(e) = dispatcher.handle_event(event).await {
            eprintln!("[msgraph] dispatch failed: {e}");
        }
    }
    Ok(outcome)
}

// ---------------------------------------------------------------------------
// Generic webhook platform (hermes webhook.py)
// ---------------------------------------------------------------------------

/// `[messaging.webhook]` — inbound webhooks from external services
/// (GitHub, GitLab, Svix/AgentMail, monitoring, inter-agent pings).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WebhookConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Per-route fixed-window rate limit (requests/minute, hermes default 30).
    #[serde(default = "default_webhook_rate_limit")]
    pub rate_limit: u32,
    /// Route table (`[[messaging.webhook.routes]]`).
    #[serde(default)]
    pub routes: Vec<WebhookRoute>,
}

fn default_webhook_rate_limit() -> u32 {
    30
}

/// One webhook route (hermes `platforms.webhook.extra.routes.<name>`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WebhookRoute {
    /// Endpoint name — mounted at `/webhooks/hook/<name>`.
    pub name: String,
    /// HMAC secret. REQUIRED (hermes validates at startup);
    /// `INSECURE_NO_AUTH` disables verification (testing only).
    #[serde(default)]
    pub secret: String,
    /// Optional event-type filter (matched against X-Webhook-Event /
    /// X-GitHub-Event / X-Gitlab-Event headers).
    #[serde(default)]
    pub events: Vec<String>,
    /// Prompt template; `{body}` and `{event}` are substituted.
    #[serde(default = "default_webhook_prompt")]
    pub prompt: String,
    /// Where the response goes: `log` (default), `telegram`, `discord`,
    /// `slack`, `whatsapp_cloud`.
    #[serde(default = "default_webhook_deliver")]
    pub deliver: String,
    /// Chat/channel/number for platform delivery targets.
    #[serde(default)]
    pub deliver_chat: String,
    /// Skip the agent: the rendered prompt IS the delivered message
    /// (hermes deliver_only — zero-LLM push notifications).
    #[serde(default)]
    pub deliver_only: bool,
}

fn default_webhook_prompt() -> String {
    "Incoming webhook ({event}): {body}".to_string()
}

fn default_webhook_deliver() -> String {
    "log".to_string()
}

/// Replay window for timestamped signatures (hermes: 300s).
const SIGNATURE_REPLAY_WINDOW_SECS: i64 = 300;
/// Idempotency cache TTL (hermes `_idempotency_ttl` = 1h).
const IDEMPOTENCY_TTL_SECS: u64 = 3600;
/// Svix/base64 marker.
const SVIX_SECRET_PREFIX: &str = "whsec_";

/// Multi-scheme signature verification (hermes `_validate_signature`):
/// Svix, GitHub, GitLab, generic V2 (timestamp-bound), generic V1
/// (legacy). V2 presence commits — never downgrades to V1.
pub fn webhook_signature_ok(
    route_name: &str,
    body: &[u8],
    headers: &[(String, String)],
    secret: &str,
    now_secs: i64,
) -> bool {
    if secret == "INSECURE_NO_AUTH" {
        return true;
    }
    if secret.is_empty() {
        return false;
    }
    let header = |name: &str| -> String {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let hmac_hex = |data: &[u8]| -> String {
        let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else {
            return String::new();
        };
        mac.update(data);
        mac.finalize()
            .into_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    };

    // Svix / AgentMail: svix-id / svix-timestamp / svix-signature
    // ("v1,<base64>" list), signed content "{id}.{timestamp}.{body}".
    let svix_id = header("svix-id");
    let svix_timestamp = header("svix-timestamp");
    let svix_signature = header("svix-signature");
    if !svix_id.is_empty() || !svix_timestamp.is_empty() || !svix_signature.is_empty() {
        if svix_id.is_empty() || svix_timestamp.is_empty() || svix_signature.is_empty() {
            return false;
        }
        let Ok(ts) = svix_timestamp.parse::<i64>() else {
            return false;
        };
        if (now_secs - ts).abs() > SIGNATURE_REPLAY_WINDOW_SECS {
            return false;
        }
        // whsec_ secrets are base64-encoded keys; plain secrets used as-is.
        let key: Vec<u8> = if let Some(encoded) = secret.strip_prefix(SVIX_SECRET_PREFIX) {
            use base64::Engine;
            match base64::engine::general_purpose::STANDARD.decode(encoded) {
                Ok(key) => key,
                Err(_) => return false,
            }
        } else {
            secret.as_bytes().to_vec()
        };
        let signed = format!("{svix_id}.{svix_timestamp}.")
            .into_bytes()
            .iter()
            .cloned()
            .chain(body.iter().cloned())
            .collect::<Vec<u8>>();
        let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(&key) else {
            return false;
        };
        mac.update(&signed);
        let expected = mac.finalize().into_bytes();
        use base64::Engine;
        let expected_b64 = base64::engine::general_purpose::STANDARD.encode(expected);
        return svix_signature.split_whitespace().any(|candidate| {
            candidate
                .strip_prefix("v1,")
                .map(|digest| constant_time_eq_bytes(digest.as_bytes(), expected_b64.as_bytes()))
                .unwrap_or(false)
        });
    }

    // GitHub: X-Hub-Signature-256 = sha256=<hex>.
    let gh_sig = header("x-hub-signature-256");
    if !gh_sig.is_empty() {
        let expected = format!("sha256={}", hmac_hex(body));
        return constant_time_eq_bytes(gh_sig.as_bytes(), expected.as_bytes());
    }

    // GitLab: X-Gitlab-Token = <plain secret>.
    let gl_token = header("x-gitlab-token");
    if !gl_token.is_empty() {
        return constant_time_eq_bytes(gl_token.as_bytes(), secret.as_bytes());
    }

    // Generic V2: timestamp-bound (replay-protected). Presence commits —
    // no V1 fallback (hermes downgrade guard).
    let v2_sig = header("x-webhook-signature-v2");
    if !v2_sig.is_empty() {
        let v2_timestamp = header("x-webhook-timestamp");
        if v2_timestamp.is_empty() {
            tracing::warn!(
                "[webhook] route '{route_name}' sent X-Webhook-Signature-V2 with no X-Webhook-Timestamp — rejecting"
            );
            return false;
        }
        let Ok(ts) = v2_timestamp.parse::<i64>() else {
            return false;
        };
        if (now_secs - ts).abs() > SIGNATURE_REPLAY_WINDOW_SECS {
            tracing::warn!(
                "[webhook] route '{route_name}' generic HMAC V2 timestamp outside replay window"
            );
            return false;
        }
        let mut signed = v2_timestamp.as_bytes().to_vec();
        signed.push(b'.');
        signed.extend_from_slice(body);
        let expected = hmac_hex(&signed);
        return constant_time_eq_bytes(v2_sig.as_bytes(), expected.as_bytes());
    }

    // Generic V1 (legacy, body-only — replay-vulnerable; hermes warns once).
    let v1_sig = header("x-webhook-signature");
    if !v1_sig.is_empty() {
        let expected = hmac_hex(body);
        return constant_time_eq_bytes(v1_sig.as_bytes(), expected.as_bytes());
    }

    false
}

/// Route event-filter check (hermes header-based event filtering).
pub fn webhook_event_allowed(route: &WebhookRoute, headers: &[(String, String)]) -> (bool, String) {
    let event = ["x-webhook-event", "x-github-event", "x-gitlab-event"]
        .iter()
        .find_map(|name| {
            headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.clone())
        })
        .unwrap_or_default();
    if route.events.is_empty() {
        return (true, event);
    }
    (route.events.iter().any(|allowed| *allowed == event), event)
}

/// Render the route prompt template (hermes prompt formatting).
pub fn webhook_render_prompt(template: &str, event: &str, body: &str) -> String {
    template.replace("{event}", event).replace("{body}", body)
}

/// Per-process webhook runtime state (rate limits + idempotency).
pub struct WebhookRuntime {
    /// route -> (window_start_epoch_min, count)
    pub rate_windows: tokio::sync::Mutex<std::collections::HashMap<String, (u64, u32)>>,
    /// delivery-id -> processed-at epoch secs
    pub seen_deliveries: tokio::sync::Mutex<std::collections::HashMap<String, u64>>,
}

impl Default for WebhookRuntime {
    fn default() -> Self {
        Self {
            rate_windows: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            seen_deliveries: tokio::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }
}

/// Fixed-window rate limit (hermes per-route limiter).
pub async fn webhook_rate_limited(
    runtime: &WebhookRuntime,
    route_name: &str,
    limit: u32,
    now_secs: u64,
) -> bool {
    let window = now_secs / 60;
    let mut windows = runtime.rate_windows.lock().await;
    let entry = windows.entry(route_name.to_string()).or_insert((window, 0));
    if entry.0 != window {
        *entry = (window, 0);
    }
    entry.1 += 1;
    entry.1 > limit
}

/// Idempotency: true when this delivery id was already processed within
/// the TTL (hermes `_seen_deliveries`).
pub async fn webhook_already_seen(
    runtime: &WebhookRuntime,
    delivery_id: &str,
    now_secs: u64,
) -> bool {
    if delivery_id.is_empty() {
        return false;
    }
    let mut seen = runtime.seen_deliveries.lock().await;
    seen.retain(|_, ts| now_secs.saturating_sub(*ts) < IDEMPOTENCY_TTL_SECS);
    if seen.contains_key(delivery_id) {
        return true;
    }
    seen.insert(delivery_id.to_string(), now_secs);
    false
}

/// Deliver a webhook response to the configured target (hermes `deliver`).
/// Unknown/misconfigured targets fall back to the log (hermes behavior).
pub async fn webhook_deliver(
    config: &crate::config::UlncLawConfig,
    route: &WebhookRoute,
    text: &str,
) {
    if text.trim().is_empty() {
        return;
    }
    let client = reqwest::Client::new();
    match route.deliver.as_str() {
        "telegram" => {
            let cfg = &config.messaging.telegram;
            let token = crate::messaging::resolve_telegram_token_public(cfg);
            match token {
                Some(token) => {
                    crate::messaging::telegram_send_public(
                        &client,
                        &token,
                        &route.deliver_chat,
                        text,
                    )
                    .await
                }
                None => eprintln!("[webhook] telegram delivery unavailable: no bot token"),
            }
        }
        "discord" => {
            let cfg = &config.messaging.discord;
            let token = crate::messaging::resolve_discord_token_public(cfg);
            match token {
                Some(token) => {
                    crate::messaging::discord_send_public(&token, &route.deliver_chat, text).await
                }
                None => eprintln!("[webhook] discord delivery unavailable: no bot token"),
            }
        }
        "slack" => {
            let cfg = &config.messaging.slack;
            let token = crate::messaging::resolve_slack_bot_token_public(cfg);
            match token {
                Some(token) => {
                    crate::messaging::slack_send_public(&token, &route.deliver_chat, text).await
                }
                None => eprintln!("[webhook] slack delivery unavailable: no bot token"),
            }
        }
        "whatsapp_cloud" => {
            let cfg = &config.messaging.whatsapp_cloud;
            if let Err(e) = whatsapp_send(&client, cfg, &route.deliver_chat, text).await {
                eprintln!("[webhook] whatsapp delivery failed: {e}");
            }
        }
        _ => {
            // "log" and anything unrecognized (hermes fallback).
            eprintln!("[webhook] route '{}': {}", route.name, text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wa_cfg() -> WhatsAppCloudConfig {
        WhatsAppCloudConfig {
            enabled: true,
            access_token: "token".into(),
            phone_number_id: "123456".into(),
            verify_token: "seekrit".into(),
            app_secret: "app-secret".into(),
            allowed_sender_ids: vec!["5511999990000".into()],
            graph_version: "v20.0".into(),
        }
    }

    #[test]
    fn verify_handshake_echoes_challenge() {
        let cfg = wa_cfg();
        let query = vec![
            ("hub.mode".to_string(), "subscribe".to_string()),
            ("hub.verify_token".to_string(), "seekrit".to_string()),
            ("hub.challenge".to_string(), "1158201444".to_string()),
        ];
        assert_eq!(whatsapp_verify(&cfg, &query).unwrap(), "1158201444");

        let bad = vec![
            ("hub.mode".to_string(), "subscribe".to_string()),
            ("hub.verify_token".to_string(), "wrong".to_string()),
            ("hub.challenge".to_string(), "x".to_string()),
        ];
        assert!(whatsapp_verify(&cfg, &bad).is_err());
    }

    #[test]
    fn signature_verification_matches_meta_scheme() {
        let body = br#"{"object":"whatsapp_business_account"}"#;
        // Compute the expected signature with the same algorithm.
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        let mut mac = Hmac::<Sha256>::new_from_slice(b"app-secret").unwrap();
        mac.update(body);
        let hex: String = mac
            .finalize()
            .into_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert!(whatsapp_signature_ok(
            body,
            &format!("sha256={hex}"),
            "app-secret"
        ));
        assert!(!whatsapp_signature_ok(
            body,
            "sha256=deadbeef",
            "app-secret"
        ));
        assert!(!whatsapp_signature_ok(body, "", "app-secret"));
        // No app_secret configured → verification disabled (hermes).
        assert!(whatsapp_signature_ok(body, "", ""));
    }

    #[test]
    fn parses_text_messages_and_skips_others() {
        let payload = json!({
            "object": "whatsapp_business_account",
            "entry": [{
                "id": "1",
                "changes": [{
                    "field": "messages",
                    "value": {
                        "messages": [
                            {"from": "5511999990000", "id": "wamid.1", "type": "text",
                             "text": {"body": "olá"}},
                            {"from": "5511999990000", "id": "wamid.2", "type": "image"},
                        ]
                    }
                }]
            }]
        });
        let events = whatsapp_parse_messages(&payload);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].platform, "whatsapp_cloud");
        assert_eq!(events[0].chat_id, "5511999990000");
        assert_eq!(events[0].text, "olá");
        assert_eq!(events[0].message_id, "wamid.1");
    }

    #[test]
    fn parse_full_captures_media_and_captions() {
        let payload: Value = serde_json::from_str(
            r#"{"entry":[{"changes":[{"value":{"messages":[
                {"type":"text","from":"5511999990000","id":"wamid.t",
                 "text":{"body":"hello"}},
                {"type":"image","from":"5511999990000","id":"wamid.i",
                 "image":{"id":"media-1","mime_type":"image/jpeg","caption":"look"}},
                {"type":"document","from":"5511999990000","id":"wamid.d",
                 "document":{"id":"media-2","mime_type":"application/pdf","filename":"r.pdf"}},
                {"type":"audio","from":"5511999990000","id":"wamid.a",
                 "audio":{"id":"media-3","mime_type":"audio/ogg"}},
                {"type":"location","from":"5511999990000","id":"wamid.l",
                 "location":{"latitude":1.0}}
            ]}}]}]}
        "#,
        )
        .unwrap();
        let parsed = whatsapp_parse_messages_full(&payload);
        // location is skipped; text + image + document + audio remain.
        assert_eq!(parsed.len(), 4);
        let (text_event, text_media) = &parsed[0];
        assert_eq!(text_event.text, "hello");
        assert!(text_media.is_empty());
        let (image_event, image_media) = &parsed[1];
        assert_eq!(image_event.text, "look");
        assert_eq!(image_media.len(), 1);
        assert_eq!(image_media[0].media_id, "media-1");
        assert_eq!(image_media[0].kind, "image");
        let (doc_event, doc_media) = &parsed[2];
        assert_eq!(doc_event.text, "");
        assert_eq!(doc_media[0].filename, "r.pdf");
        assert_eq!(doc_media[0].mime, "application/pdf");
        let (audio_event, audio_media) = &parsed[3];
        assert_eq!(audio_event.text, "");
        assert_eq!(audio_media[0].kind, "audio");
        assert_eq!(audio_media[0].mime, "audio/ogg");
    }

    #[test]
    fn media_caps_match_meta_limits() {
        assert_eq!(whatsapp_media_cap("image"), 5 * 1024 * 1024);
        assert_eq!(whatsapp_media_cap("video"), 16 * 1024 * 1024);
        assert_eq!(whatsapp_media_cap("audio"), 16 * 1024 * 1024);
        assert_eq!(whatsapp_media_cap("sticker"), 500 * 1024);
        assert_eq!(whatsapp_media_cap("document"), 100 * 1024 * 1024);
    }

    #[test]
    fn parse_text_view_strips_media() {
        let payload: Value = serde_json::from_str(
            r#"{"entry":[{"changes":[{"value":{"messages":[
                {"type":"image","from":"5511999990000","id":"wamid.i",
                 "image":{"id":"media-1","mime_type":"image/jpeg"}}
            ]}}]}]}
        "#,
        )
        .unwrap();
        let events = whatsapp_parse_messages(&payload);
        assert_eq!(events.len(), 1);
        assert!(events[0].attachments.is_empty());
    }

    #[test]
    fn graph_url_uses_version_and_phone_id() {
        let cfg = wa_cfg();
        assert_eq!(
            cfg.graph_url("messages"),
            "https://graph.facebook.com/v20.0/messages"
        );
    }

    fn headers(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn hmac_sha256_hex(secret: &str, data: &[u8]) -> String {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(data);
        mac.finalize()
            .into_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    #[test]
    fn webhook_signature_github_scheme() {
        let body = br#"{"action":"opened"}"#;
        let sig = format!("sha256={}", hmac_sha256_hex("gh-secret", body));
        let ok = headers(&[("X-Hub-Signature-256", &sig)]);
        assert!(webhook_signature_ok("gh", body, &ok, "gh-secret", 0));
        let bad = headers(&[("X-Hub-Signature-256", "sha256=00")]);
        assert!(!webhook_signature_ok("gh", body, &bad, "gh-secret", 0));
    }

    #[test]
    fn webhook_signature_gitlab_and_insecure() {
        let body = b"x";
        let ok = headers(&[("X-Gitlab-Token", "gl-secret")]);
        assert!(webhook_signature_ok("gl", body, &ok, "gl-secret", 0));
        // INSECURE_NO_AUTH bypasses verification entirely (testing only).
        assert!(webhook_signature_ok("gl", body, &[], "INSECURE_NO_AUTH", 0));
        // No header + real secret → reject.
        assert!(!webhook_signature_ok("gl", body, &[], "gl-secret", 0));
    }

    #[test]
    fn webhook_signature_v2_timestamp_binding() {
        let body = br#"{"n":1}"#;
        let now = 1_700_000_000i64;
        let ts = now.to_string();
        let mut signed = ts.as_bytes().to_vec();
        signed.push(b'.');
        signed.extend_from_slice(body);
        let sig = hmac_sha256_hex("w-secret", &signed);
        let ok = headers(&[
            ("X-Webhook-Signature-V2", &sig),
            ("X-Webhook-Timestamp", &ts),
        ]);
        assert!(webhook_signature_ok("w", body, &ok, "w-secret", now));
        // Outside the 300s replay window.
        assert!(!webhook_signature_ok("w", body, &ok, "w-secret", now + 301));
        // V2 without timestamp must NOT fall back to V1.
        let no_ts = headers(&[
            ("X-Webhook-Signature-V2", &sig),
            ("X-Webhook-Signature", &hmac_sha256_hex("w-secret", body)),
        ]);
        assert!(!webhook_signature_ok("w", body, &no_ts, "w-secret", now));
    }

    #[test]
    fn webhook_signature_v1_legacy() {
        let body = br#"legacy"#;
        let sig = hmac_sha256_hex("w-secret", body);
        let ok = headers(&[("X-Webhook-Signature", &sig)]);
        assert!(webhook_signature_ok("w", body, &ok, "w-secret", 0));
    }

    #[test]
    fn webhook_signature_svix_scheme() {
        use base64::Engine;
        let body = br#"{"email":"x@y.z"}"#;
        let key = b"0123456789abcdef0123456789abcdef";
        let secret = format!(
            "whsec_{}",
            base64::engine::general_purpose::STANDARD.encode(key)
        );
        let now = 1_700_000_000i64;
        let signed = format!("msg_abc.{now}.")
            .into_bytes()
            .iter()
            .cloned()
            .chain(body.iter().cloned())
            .collect::<Vec<u8>>();
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        let mut mac = Hmac::<Sha256>::new_from_slice(key).unwrap();
        mac.update(&signed);
        let digest = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
        let ok = headers(&[
            ("svix-id", "msg_abc"),
            ("svix-timestamp", &now.to_string()),
            ("svix-signature", &format!("v1,{digest}")),
        ]);
        assert!(webhook_signature_ok("svix", body, &ok, &secret, now));
        // Stale timestamp rejected.
        assert!(!webhook_signature_ok(
            "svix",
            body,
            &ok,
            &secret,
            now + 1000
        ));
    }

    #[test]
    fn webhook_event_filter_and_prompt_render() {
        let route = WebhookRoute {
            name: "gh".into(),
            events: vec!["push".into()],
            prompt: "got {event}: {body}".into(),
            ..Default::default()
        };
        let (allowed, event) =
            webhook_event_allowed(&route, &headers(&[("X-GitHub-Event", "push")]));
        assert!(allowed);
        assert_eq!(event, "push");
        let (denied, _) = webhook_event_allowed(&route, &headers(&[("X-GitHub-Event", "issues")]));
        assert!(!denied);
        // No filter → everything passes.
        let open = WebhookRoute {
            name: "x".into(),
            ..Default::default()
        };
        let (allowed, _) = webhook_event_allowed(&open, &[]);
        assert!(allowed);
        assert_eq!(
            webhook_render_prompt("got {event}: {body}", "push", "{}"),
            "got push: {}"
        );
    }

    #[tokio::test]
    async fn webhook_rate_limit_and_idempotency() {
        let runtime = WebhookRuntime::default();
        let now = 1_700_000_000u64;
        for idx in 0..3 {
            assert!(!webhook_rate_limited(&runtime, "r", 3, now + idx).await);
        }
        assert!(webhook_rate_limited(&runtime, "r", 3, now).await);
        // New window resets.
        assert!(!webhook_rate_limited(&runtime, "r", 3, now + 61).await);

        assert!(!webhook_already_seen(&runtime, "deliv-1", now).await);
        assert!(webhook_already_seen(&runtime, "deliv-1", now + 5).await);
        // Empty ids are never deduped.
        assert!(!webhook_already_seen(&runtime, "", now).await);
        // Expired entries are forgotten.
        assert!(!webhook_already_seen(&runtime, "deliv-1", now + IDEMPOTENCY_TTL_SECS + 1).await);
    }

    #[test]
    fn msgraph_validation_and_state() {
        let query = vec![("validationToken".to_string(), "abc123".to_string())];
        assert_eq!(msgraph_validation_token(&query).as_deref(), Some("abc123"));
        assert!(msgraph_validation_token(&[]).is_none());

        let notification = json!({"clientState": "s3cr3t", "resource": "teams/1"});
        assert!(msgraph_state_ok(&notification, "s3cr3t"));
        assert!(!msgraph_state_ok(&notification, "other"));
        assert!(!msgraph_state_ok(&json!({}), "s3cr3t"));
    }

    #[test]
    fn msgraph_receipt_key_and_fifo_dedup() {
        assert_eq!(
            msgraph_receipt_key(&json!({"id": "n1"})).as_deref(),
            Some("id:n1")
        );
        assert_eq!(msgraph_receipt_key(&json!({"id": "  "})), None);
        assert_eq!(msgraph_receipt_key(&json!({})), None);
        // Unique prefixes keep the global ledger isolated per test.
        assert!(!msgraph_remember_receipt("t206-a", 2));
        assert!(msgraph_remember_receipt("t206-a", 2));
        assert!(!msgraph_remember_receipt("t206-b", 2));
        // Cap 2 → adding a third evicts the oldest (t206-a).
        assert!(!msgraph_remember_receipt("t206-c", 2));
        assert!(!msgraph_remember_receipt("t206-a", 2));
    }

    #[test]
    fn msgraph_resource_filter_matching() {
        let empty: Vec<String> = Vec::new();
        assert!(msgraph_resource_accepted("anything", &empty));
        let patterns: Vec<String> = vec!["users/me/messages".into(), "teams/*".into()];
        assert!(msgraph_resource_accepted("users/me/messages", &patterns));
        // Sub-path prefix matches like hermes.
        assert!(msgraph_resource_accepted(
            "users/me/messages/extra",
            &patterns
        ));
        assert!(msgraph_resource_accepted("/users/me/messages/", &patterns));
        // Wildcard: exact prefix or prefix/<sub>.
        assert!(msgraph_resource_accepted("teams", &patterns));
        assert!(msgraph_resource_accepted("teams/123", &patterns));
        assert!(!msgraph_resource_accepted("teamsx", &patterns));
        assert!(!msgraph_resource_accepted(
            "users/other/messages",
            &patterns
        ));
    }

    #[test]
    fn msgraph_render_template_resolves_dotted_paths() {
        let payload = json!({
            "resource": "teams/1",
            "notification": {"changeType": "created", "nested": {"deep": "v"}},
        });
        assert_eq!(
            msgraph_render_template("r={resource} c={notification.changeType}", &payload),
            "r=teams/1 c=created"
        );
        assert_eq!(
            msgraph_render_template("d={notification.nested.deep}", &payload),
            "d=v"
        );
        // Missing paths stay literal.
        assert_eq!(
            msgraph_render_template("x={nope.gone}", &payload),
            "x={nope.gone}"
        );
        // Dict values render as compact JSON.
        let rendered = msgraph_render_template("n={notification.nested}", &payload);
        assert_eq!(rendered, "n={\"deep\":\"v\"}");
    }

    #[test]
    fn msgraph_render_prompt_default_and_template() {
        let notification = json!({"b": 2, "a": 1, "resource": "r", "changeType": "created", "subscriptionId": "s1"});
        let prompt = msgraph_render_prompt(&notification, "");
        assert!(prompt.starts_with("Microsoft Graph change notification:\n\n```json\n"));
        // Sorted keys: "a" precedes "b" in the pretty dump.
        let a_pos = prompt.find("\"a\": 1").unwrap();
        let b_pos = prompt.find("\"b\": 2").unwrap();
        assert!(a_pos < b_pos);
        let templated = msgraph_render_prompt(&notification, "{change_type} on {resource}");
        assert_eq!(templated, "created on r");
    }

    #[test]
    fn msgraph_sorted_json_is_deterministic() {
        let value = json!({"z": [3, {"y": 1, "x": 2}], "a": "b"});
        assert_eq!(
            sorted_json(&value, false),
            r#"{"a":"b","z":[3,{"x":2,"y":1}]}"#
        );
        // Stable input for the sha1 message-id fallback.
        assert_eq!(sha1_hex("abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
    }
    // ------------------------------------------------------------------
    // WhatsApp interactive messages (hermes whatsapp_cloud parity)
    // ------------------------------------------------------------------

    #[test]
    fn label_truncation_respects_caps() {
        assert_eq!(whatsapp_truncate_label("short", 20), "short");
        assert_eq!(
            whatsapp_truncate_label("exactly-twenty-chars", 20),
            "exactly-twenty-chars"
        );
        let long = "a".repeat(30);
        let out = whatsapp_truncate_label(&long, 20);
        assert_eq!(out.chars().count(), 20);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn body_truncation_caps_at_1024() {
        let long = "b".repeat(2000);
        let out = whatsapp_truncate_body(&long);
        assert_eq!(out.chars().count(), 1024);
        assert!(out.ends_with("..."));
        assert_eq!(whatsapp_truncate_body("fine"), "fine");
    }

    #[test]
    fn clarify_payload_button_mode() {
        let choices: Vec<String> = vec!["Alpha option".into(), "Beta option".into()];
        let payload = whatsapp_build_clarify_interactive("cid1", "Pick one", &choices).unwrap();
        assert_eq!(payload["type"], "button");
        let body = payload
            .pointer("/body/text")
            .and_then(|v| v.as_str())
            .unwrap();
        assert!(body.contains("❓ Pick one"));
        assert!(body.contains("1. Alpha option"));
        assert!(body.contains("2. Beta option"));
        let buttons = payload
            .pointer("/action/buttons")
            .and_then(|v| v.as_array())
            .unwrap();
        assert_eq!(buttons.len(), 2);
        assert_eq!(buttons[0].pointer("/reply/id").unwrap(), "cl:cid1:0");
        assert_eq!(buttons[0].pointer("/reply/title").unwrap(), "1");
        assert_eq!(buttons[1].pointer("/reply/id").unwrap(), "cl:cid1:1");
    }

    #[test]
    fn clarify_payload_list_mode_with_other_row() {
        let choices: Vec<String> = (1..=4).map(|i| format!("Choice {}", i)).collect();
        let payload = whatsapp_build_clarify_interactive("cid2", "Pick", &choices).unwrap();
        assert_eq!(payload["type"], "list");
        let rows = payload
            .pointer("/action/sections/0/rows")
            .and_then(|v| v.as_array())
            .unwrap();
        assert_eq!(rows.len(), 5); // 4 choices + Other
        assert_eq!(rows[0]["id"], "cl:cid2:0");
        assert_eq!(rows[0]["description"], "Choice 1");
        assert_eq!(rows[4]["id"], "cl:cid2:other");
        assert_eq!(rows[4]["title"], "✏️ Other");
        assert_eq!(payload.pointer("/action/button").unwrap(), "Choose");
    }

    #[test]
    fn clarify_payload_open_ended_is_none() {
        assert!(whatsapp_build_clarify_interactive("cid3", "Tell me", &[]).is_none());
        let blanks: Vec<String> = vec!["  ".into()];
        assert!(whatsapp_build_clarify_interactive("cid3", "Tell me", &blanks).is_none());
    }

    fn interactive_payload(button_id: &str, title: &str, list: bool) -> Value {
        let kind = if list { "list_reply" } else { "button_reply" };
        json!({"entry": [{"changes": [{"value": {"messages": [{
            "type": "interactive",
            "from": "15550001111",
            "id": "wamid.test",
            "interactive": {kind: {"id": button_id, "title": title}},
        }]}}]}]})
    }

    #[test]
    fn parse_interactive_taps_button_and_list() {
        let button =
            whatsapp_parse_interactive_replies(&interactive_payload("cl:abc:1", "2", false));
        assert_eq!(button.len(), 1);
        assert_eq!(button[0].button_id, "cl:abc:1");
        assert_eq!(button[0].title, "2");
        assert_eq!(button[0].sender_id, "15550001111");

        let list = whatsapp_parse_interactive_replies(&interactive_payload(
            "cl:abc:other",
            "✏️ Other",
            true,
        ));
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].button_id, "cl:abc:other");

        // Text messages are not taps.
        let text = json!({"entry": [{"changes": [{"value": {"messages": [
            {"type": "text", "from": "1", "text": {"body": "hi"}},
        ]}}]}]});
        assert!(whatsapp_parse_interactive_replies(&text).is_empty());
    }

    #[tokio::test]
    async fn tap_dispatch_resolves_clarify_with_choice_text() {
        let _guard = crate::clarify_gateway::test_lock().lock().unwrap();
        crate::clarify_gateway::reset_for_tests();
        let handle = crate::clarify_gateway::register(
            "platform-whatsapp_cloud-15550001111",
            "Pick",
            &["First".to_string(), "Second".to_string()],
            false,
        );
        let cfg = WhatsAppCloudConfig::default();
        let tap = WaInteractiveTap {
            sender_id: "15550001111".into(),
            chat_id: "15550001111".into(),
            button_id: format!("cl:{}:1", handle.clarify_id),
            title: "2".into(),
        };
        assert!(whatsapp_dispatch_interactive_tap(&cfg, &tap).await);
        // The waiter received the mapped choice text, not the title.
        assert_eq!(handle.rx.await.unwrap(), "Second");
    }

    #[tokio::test]
    async fn tap_dispatch_other_flips_to_text_mode() {
        let _guard = crate::clarify_gateway::test_lock().lock().unwrap();
        crate::clarify_gateway::reset_for_tests();
        let handle = crate::clarify_gateway::register(
            "platform-whatsapp_cloud-2",
            "Pick",
            &["A".to_string()],
            false,
        );
        let cfg = WhatsAppCloudConfig::default();
        let tap = WaInteractiveTap {
            sender_id: "2".into(),
            chat_id: "2".into(),
            button_id: format!("cl:{}:other", handle.clarify_id),
            title: "✏️ Other".into(),
        };
        // send fails (no token) but the flip still claims the tap.
        assert!(whatsapp_dispatch_interactive_tap(&cfg, &tap).await);
        assert!(
            crate::clarify_gateway::pending_for_session("platform-whatsapp_cloud-2")
                .unwrap()
                .awaiting_text
        );
    }

    #[tokio::test]
    async fn stale_tap_falls_back() {
        let _guard = crate::clarify_gateway::test_lock().lock().unwrap();
        crate::clarify_gateway::reset_for_tests();
        let cfg = WhatsAppCloudConfig::default();
        let tap = WaInteractiveTap {
            sender_id: "3".into(),
            chat_id: "3".into(),
            button_id: "cl:nonexistent:0".into(),
            title: "1".into(),
        };
        assert!(!whatsapp_dispatch_interactive_tap(&cfg, &tap).await);
        // Unrecognized prefixes fall back too.
        let appr = WaInteractiveTap {
            button_id: "appr:xyz:approve".into(),
            ..tap.clone()
        };
        assert!(!whatsapp_dispatch_interactive_tap(&cfg, &appr).await);
    }

    // ------------------------------------------------------------------
    // BlueBubbles (hermes bluebubbles.py)
    // ------------------------------------------------------------------

    fn bb_cfg() -> BlueBubblesConfig {
        BlueBubblesConfig {
            enabled: true,
            server_url: "http://127.0.0.1:1234".into(),
            password: "p@ss word".into(),
            allowed_chat_ids: vec![],
            webhook_url: "http://127.0.0.1:8765/webhooks/bluebubbles".into(),
        }
    }

    #[test]
    fn bluebubbles_parse_json_dm() {
        let body = r#"{
            "type": "new-message",
            "data": {
                "guid": "msg-1",
                "text": "hello there",
                "chatGuid": "iMessage;-;+15551234567",
                "chatIdentifier": "+15551234567",
                "isGroup": false,
                "isFromMe": false,
                "handle": {"address": "+15551234567"}
            }
        }"#;
        let event = bluebubbles_parse_webhook(body).unwrap().unwrap();
        assert_eq!(event.chat_id, "iMessage;-;+15551234567");
        assert_eq!(event.sender_id, "+15551234567");
        assert_eq!(event.text, "hello there");
        assert!(!event.is_group);
        assert!(event.attachments.is_empty());
    }

    #[test]
    fn bluebubbles_parse_group_via_guid_marker() {
        let body = r#"{
            "type": "new-message",
            "data": {
                "text": "hey team",
                "chatGuid": "iMessage;+;chat123",
                "chatIdentifier": "chat123",
                "handle": {"address": "+15550001111"}
            }
        }"#;
        let event = bluebubbles_parse_webhook(body).unwrap().unwrap();
        assert!(event.is_group);
    }

    #[test]
    fn bluebubbles_parse_form_fallback() {
        let json_str = r#"{"type":"new-message","data":{"text":"form body","chatGuid":"iMessage;-;bob@example.com","chatIdentifier":"bob@example.com","handle":{"address":"bob@example.com"}}}"#;
        let body = format!("payload={}", urlencode_component(json_str));
        let event = bluebubbles_parse_webhook(&body).unwrap().unwrap();
        assert_eq!(event.text, "form body");
        assert_eq!(event.sender_id, "bob@example.com");
    }

    #[test]
    fn bluebubbles_skips_from_me() {
        let body = r#"{"type":"new-message","data":{"text":"my own message","chatGuid":"iMessage;-;x","handle":{"address":"x"},"isFromMe":true}}"#;
        assert!(bluebubbles_parse_webhook(body).unwrap().is_none());
    }

    #[test]
    fn bluebubbles_skips_tapbacks() {
        for code in [2000, 2003, 3005] {
            let body = format!(
                r#"{{"type":"new-message","data":{{"text":"","chatGuid":"iMessage;-;x","handle":{{"address":"x"}},"associatedMessageType":{code}}}}}"#
            );
            assert!(bluebubbles_parse_webhook(&body).unwrap().is_none());
        }
    }

    #[test]
    fn bluebubbles_skips_non_message_events() {
        let body = r#"{"type":"typing-indicator","data":{"chatGuid":"iMessage;-;x"}}"#;
        assert!(bluebubbles_parse_webhook(body).unwrap().is_none());
    }

    #[test]
    fn bluebubbles_chats_array_v19_fallback() {
        let body = r#"{
            "type": "new-message",
            "data": {
                "text": "hi from v1.9",
                "chats": [{"guid": "iMessage;-;alice@example.com", "chatIdentifier": "alice@example.com"}],
                "handle": {"address": "alice@example.com"}
            }
        }"#;
        let event = bluebubbles_parse_webhook(body).unwrap().unwrap();
        assert_eq!(event.chat_id, "iMessage;-;alice@example.com");
        assert_eq!(event.text, "hi from v1.9");
    }

    #[test]
    fn bluebubbles_captures_attachments() {
        let body = r#"{
            "type": "new-message",
            "data": {
                "text": "",
                "chatGuid": "iMessage;-;x",
                "handle": {"address": "x"},
                "attachments": [{"guid": "att-1", "mimeType": "image/jpeg"}, {"guid": ""}]
            }
        }"#;
        let event = bluebubbles_parse_webhook(body).unwrap().unwrap();
        assert_eq!(
            event.attachments,
            vec![("att-1".to_string(), "image/jpeg".to_string())]
        );
    }

    #[test]
    fn bluebubbles_missing_fields_error() {
        let body = r#"{"type":"new-message","data":{"text":""}}"#;
        let err = bluebubbles_parse_webhook(body).unwrap_err();
        assert!(err.to_string().contains("missing message fields"));
        let empty_form = bluebubbles_parse_webhook("foo=bar").unwrap_err();
        assert!(empty_form.to_string().contains("empty payload"));
    }

    #[test]
    fn bluebubbles_url_decode_component() {
        assert_eq!(url_decode_component("hello%20world"), "hello world");
        assert_eq!(url_decode_component("a+b"), "a b");
        assert_eq!(url_decode_component("p%40ss%20word"), "p@ss word");
        assert_eq!(url_decode_component("bad%ZZ"), "bad%ZZ");
        assert_eq!(url_decode_component("trailing%2"), "trailing%2");
    }

    #[test]
    fn bluebubbles_api_url_carries_password() {
        let cfg = bb_cfg();
        let url = bluebubbles_api_url(&cfg, "/api/v1/ping").unwrap();
        assert_eq!(
            url,
            "http://127.0.0.1:1234/api/v1/ping?password=p%40ss%20word"
        );
        let with_query = bluebubbles_api_url(&cfg, "/api/v1/message/text?guid=abc").unwrap();
        assert!(with_query.contains("?guid=abc&password=p%40ss%20word"));
        let mut no_password = cfg.clone();
        no_password.password = String::new();
        std::env::remove_var("BLUEBUBBLES_PASSWORD");
        assert!(bluebubbles_api_url(&no_password, "/api/v1/ping").is_none());
    }

    #[test]
    fn bluebubbles_guid_cache_lru() {
        let cache = BlueBubblesGuidCache::default();
        cache.put("first", "iMessage;-;first");
        cache.put("second", "iMessage;-;second");
        assert_eq!(cache.get("first").as_deref(), Some("iMessage;-;first"));
        // Fill past the cap: 500 entries total, "second" is now oldest.
        for i in 0..499 {
            cache.put(&format!("k{i}"), &format!("guid-{i}"));
        }
        assert!(cache.get("second").is_none());
        assert_eq!(cache.get("first").as_deref(), Some("iMessage;-;first"));
        assert_eq!(cache.get("k498").as_deref(), Some("guid-498"));
    }

    #[tokio::test]
    async fn bluebubbles_resolve_guid_passthrough_and_cache() {
        let cfg = bb_cfg();
        let client = reqwest::Client::new();
        let cache = BlueBubblesGuidCache::default();
        // Raw GUIDs (contain ';') pass through without network.
        let guid =
            bluebubbles_resolve_chat_guid(&client, &cfg, &cache, "iMessage;-;+15551234567").await;
        assert_eq!(guid.as_deref(), Some("iMessage;-;+15551234567"));
        // Pre-seeded cache entries resolve without network too.
        cache.put("bob@example.com", "iMessage;-;bob@example.com");
        let cached = bluebubbles_resolve_chat_guid(&client, &cfg, &cache, "bob@example.com").await;
        assert_eq!(cached.as_deref(), Some("iMessage;-;bob@example.com"));
        assert!(bluebubbles_resolve_chat_guid(&client, &cfg, &cache, "   ")
            .await
            .is_none());
    }
}

// ---------------------------------------------------------------------------
// WhatsApp interactive messages (hermes whatsapp_cloud.py interactive half)
// ---------------------------------------------------------------------------

/// WhatsApp caps quick-reply button titles at 20 chars and list-row
/// titles at 24 (descriptions at 72). Truncate with an ellipsis counted
/// toward the limit (hermes `_truncate_button_label`).
pub fn whatsapp_truncate_label(text: &str, limit: usize) -> String {
    let text = text.trim();
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= limit {
        return text.to_string();
    }
    let cut = limit.saturating_sub(1).max(1);
    let mut out: String = chars[..cut].iter().collect();
    out.push('…');
    out
}

/// `interactive.body.text` caps at 1024 chars (hermes `_truncate_body`).
pub fn whatsapp_truncate_body(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= 1024 {
        return text.to_string();
    }
    let mut out: String = chars[..1021].iter().collect();
    out.push_str("...");
    out
}

/// Build the clarify `interactive` payload (hermes `send_clarify`):
/// 1–3 choices → `type=button` with numeric labels (full choice text in
/// the body so long options survive the 20-char label cap); 4+ →
/// `type=list` with a per-row description and a final "✏️ Other" row.
/// Button ids carry `cl:<clarify_id>:<idx|other>` for the inbound tap
/// dispatch. Returns None for open-ended prompts (plain text instead).
pub fn whatsapp_build_clarify_interactive(
    clarify_id: &str,
    question: &str,
    choices: &[String],
) -> Option<Value> {
    if choices.is_empty() {
        return None;
    }
    let choices: Vec<&str> = choices
        .iter()
        .map(|c| c.trim())
        .filter(|c| !c.is_empty())
        .take(10)
        .collect();
    if choices.is_empty() {
        return None;
    }
    let option_lines: Vec<String> = choices
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{}. {}", i + 1, c))
        .collect();
    let body_text = whatsapp_truncate_body(&format!(
        "❓ {}\n\n{}",
        question.trim(),
        option_lines.join("\n")
    ));
    if choices.len() <= 3 {
        let buttons: Vec<Value> = (0..choices.len())
            .map(|idx| {
                json!({
                    "type": "reply",
                    "reply": {
                        "id": format!("cl:{}:{}", clarify_id, idx),
                        "title": whatsapp_truncate_label(&format!("{}", idx + 1), 20),
                    },
                })
            })
            .collect();
        Some(json!({
            "type": "button",
            "body": {"text": body_text},
            "action": {"buttons": buttons},
        }))
    } else {
        let mut rows: Vec<Value> = choices
            .iter()
            .enumerate()
            .map(|(idx, choice)| {
                json!({
                    "id": format!("cl:{}:{}", clarify_id, idx),
                    "title": whatsapp_truncate_label(&format!("{}", idx + 1), 24),
                    "description": whatsapp_truncate_label(choice, 72),
                })
            })
            .collect();
        rows.push(json!({
            "id": format!("cl:{}:other", clarify_id),
            "title": "✏️ Other",
            "description": "Type your own answer",
        }));
        Some(json!({
            "type": "list",
            "body": {"text": body_text},
            "action": {
                "button": "Choose",
                "sections": [{"title": "Options", "rows": rows}],
            },
        }))
    }
}

/// POST an interactive message payload (hermes `_post_interactive`).
/// Returns the Graph message id on success.
pub async fn whatsapp_send_interactive(
    client: &reqwest::Client,
    cfg: &WhatsAppCloudConfig,
    to: &str,
    interactive: &Value,
) -> Result<Option<String>> {
    let url = cfg.graph_url("messages");
    let payload = json!({
        "messaging_product": "whatsapp",
        "recipient_type": "individual",
        "to": to,
        "type": "interactive",
        "interactive": interactive,
    });
    let response = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", cfg.access_token))
        .json(&payload)
        .send()
        .await
        .map_err(|e| AgentError::Tool(format!("whatsapp interactive send: {e}")))?;
    let status = response.status();
    let body: Value = response.json().await.unwrap_or_else(|_| json!({}));
    if !status.is_success() {
        let message = body
            .pointer("/error/message")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error");
        let code = body.pointer("/error/code").and_then(|v| v.as_i64());
        let err = match code {
            Some(code) => format!("graph error {} (HTTP {}): {}", code, status, message),
            None => format!("HTTP {}: {}", status, message),
        };
        return Err(AgentError::Tool(err));
    }
    Ok(body
        .pointer("/messages/0/id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string()))
}

/// PlatformSender for WhatsApp Cloud: text via Graph `messages`,
/// clarify via native interactive buttons/list.
pub struct WhatsAppSender {
    pub cfg: WhatsAppCloudConfig,
}

#[async_trait::async_trait]
impl crate::messaging::PlatformSender for WhatsAppSender {
    async fn send_text(&self, chat_id: &str, text: &str) {
        let client = reqwest::Client::new();
        if let Err(e) = whatsapp_send(&client, &self.cfg, chat_id, text).await {
            eprintln!("[whatsapp_cloud] clarify text send failed: {e}");
        }
    }

    async fn send_clarify(
        &self,
        chat_id: &str,
        clarify_id: &str,
        question: &str,
        choices: &[String],
    ) -> bool {
        let Some(interactive) = whatsapp_build_clarify_interactive(clarify_id, question, choices)
        else {
            return false;
        };
        let client = reqwest::Client::new();
        match whatsapp_send_interactive(&client, &self.cfg, chat_id, &interactive).await {
            Ok(_) => true,
            Err(e) => {
                eprintln!("[whatsapp_cloud] interactive clarify rejected: {e}");
                false
            }
        }
    }
}

/// An inbound interactive tap (button_reply or list_reply).
#[derive(Debug, Clone)]
pub struct WaInteractiveTap {
    pub sender_id: String,
    pub chat_id: String,
    pub button_id: String,
    pub title: String,
}

/// Extract interactive replies from a webhook payload (hermes inbound
/// `interactive` parsing: button_reply / list_reply carry id+title in
/// different sub-objects).
pub fn whatsapp_parse_interactive_replies(payload: &Value) -> Vec<WaInteractiveTap> {
    let mut taps = Vec::new();
    let Some(entries) = payload.get("entry").and_then(|v| v.as_array()) else {
        return taps;
    };
    for entry in entries {
        let Some(changes) = entry.get("changes").and_then(|v| v.as_array()) else {
            continue;
        };
        for change in changes {
            let Some(messages) = change.pointer("/value/messages").and_then(|v| v.as_array())
            else {
                continue;
            };
            for message in messages {
                if message.get("type").and_then(|v| v.as_str()) != Some("interactive") {
                    continue;
                }
                let from = message
                    .get("from")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if from.is_empty() {
                    continue;
                }
                let inner = message
                    .pointer("/interactive/button_reply")
                    .or_else(|| message.pointer("/interactive/list_reply"));
                let Some(inner) = inner else { continue };
                let button_id = inner
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if button_id.is_empty() {
                    continue;
                }
                taps.push(WaInteractiveTap {
                    sender_id: from.clone(),
                    chat_id: from,
                    button_id,
                    title: inner
                        .get("title")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                });
            }
        }
    }
    taps
}

/// Route an inbound interactive tap to the matching resolver (hermes
/// `_dispatch_interactive_reply`). Returns true when the tap was claimed
/// (no further dispatch); false falls back to treating the button title
/// as a normal text message (stale-tap / restart semantics).
pub async fn whatsapp_dispatch_interactive_tap(
    cfg: &WhatsAppCloudConfig,
    tap: &WaInteractiveTap,
) -> bool {
    if let Some(rest) = tap.button_id.strip_prefix("cl:") {
        let Some((clarify_id, choice)) = rest.split_once(':') else {
            return false;
        };
        if choice == "other" {
            if crate::clarify_gateway::mark_awaiting_text(clarify_id) {
                let client = reqwest::Client::new();
                if let Err(e) =
                    whatsapp_send(&client, cfg, &tap.chat_id, "✏️ Type your answer:").await
                {
                    eprintln!("[whatsapp_cloud] clarify other-prompt failed: {e}");
                }
                return true;
            }
            return false;
        }
        return crate::clarify_gateway::resolve_tap(clarify_id, choice, &tap.title);
    }
    // appr:/sc: prefixes belong to gateway approval / slash-confirm
    // flows that ulnclaw serves over HTTP; taps on them fall through to
    // text dispatch (hermes fallback semantics).
    false
}

// ---------------------------------------------------------------------------
// BlueBubbles iMessage platform (hermes bluebubbles.py)
// ---------------------------------------------------------------------------

/// hermes MAX_TEXT_LENGTH — iMessage bubble chunk cap.
pub const BLUEBUBBLES_MAX_TEXT_LENGTH: usize = 4000;
const BLUEBUBBLES_GUID_CACHE_SIZE: usize = 500;

/// `[messaging.bluebubbles]` — BlueBubbles macOS iMessage server (hermes
/// `platforms.bluebubbles`). Inbound events arrive on the gateway's
/// `/webhooks/bluebubbles` route; outbound rides the BlueBubbles REST API.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BlueBubblesConfig {
    pub enabled: bool,
    /// BlueBubbles server base URL (fallback `BLUEBUBBLES_SERVER_URL`).
    pub server_url: String,
    /// Server password (fallback `BLUEBUBBLES_PASSWORD`); carried as a
    /// query param because the BlueBubbles webhook API cannot send
    /// custom headers (hermes note).
    pub password: String,
    /// Chat ids allowed to talk to the bot (chat GUIDs or identifiers);
    /// empty refuses all (hermes pairing semantics shared gate).
    pub allowed_chat_ids: Vec<String>,
    /// Externally reachable URL of this gateway's
    /// `/webhooks/bluebubbles` route — registered with the BlueBubbles
    /// server at startup so it knows where to POST events.
    pub webhook_url: String,
}

pub fn bluebubbles_server_url(cfg: &BlueBubblesConfig) -> Option<String> {
    let trimmed = cfg.server_url.trim();
    if !trimmed.is_empty() {
        return Some(trimmed.trim_end_matches('/').to_string());
    }
    std::env::var("BLUEBUBBLES_SERVER_URL")
        .ok()
        .map(|v| v.trim().trim_end_matches('/').to_string())
        .filter(|v| !v.is_empty())
}

pub fn bluebubbles_password(cfg: &BlueBubblesConfig) -> Option<String> {
    let trimmed = cfg.password.trim();
    if !trimmed.is_empty() {
        return Some(trimmed.to_string());
    }
    std::env::var("BLUEBUBBLES_PASSWORD")
        .ok()
        .filter(|v| !v.trim().is_empty())
}

fn bluebubbles_api_url(cfg: &BlueBubblesConfig, path: &str) -> Option<String> {
    let base = bluebubbles_server_url(cfg)?;
    let password = bluebubbles_password(cfg)?;
    let sep = if path.contains('?') { '&' } else { '?' };
    Some(format!(
        "{}{}{}password={}",
        base,
        path,
        sep,
        urlencode_component(&password)
    ))
}

fn urlencode_component(value: &str) -> String {
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

/// hermes `_MESSAGE_EVENTS`.
pub const BLUEBUBBLES_MESSAGE_EVENTS: &[&str] = &["new-message", "message", "updated-message"];
/// hermes `_TAPBACK_ADDED` ∪ `_TAPBACK_REMOVED` associatedMessageType codes.
pub const BLUEBUBBLES_TAPBACK_CODES: &[i64] = &[
    2000, 2001, 2002, 2003, 2004, 2005, 3000, 3001, 3002, 3003, 3004, 3005,
];

/// Parsed BlueBubbles webhook event ready for dispatch.
#[derive(Debug, Clone)]
pub struct BlueBubblesEvent {
    pub chat_id: String,
    pub chat_name: String,
    pub sender_id: String,
    pub text: String,
    pub is_group: bool,
    /// (guid, mimeType) attachment references to download.
    pub attachments: Vec<(String, String)>,
}

/// hermes `_extract_payload_record`.
fn bluebubbles_extract_record(payload: &Value) -> Value {
    match payload.get("data") {
        Some(Value::Object(_)) => payload["data"].clone(),
        Some(Value::Array(items)) => items
            .iter()
            .find(|item| item.is_object())
            .cloned()
            .unwrap_or_else(|| payload.clone()),
        _ => {
            if payload
                .get("message")
                .map(|v| v.is_object())
                .unwrap_or(false)
            {
                payload["message"].clone()
            } else {
                payload.clone()
            }
        }
    }
}

fn first_str<'a>(candidates: impl IntoIterator<Item = Option<&'a Value>>) -> String {
    for candidate in candidates {
        if let Some(value) = candidate {
            if let Some(s) = value.as_str() {
                if !s.is_empty() {
                    return s.to_string();
                }
            }
        }
    }
    String::new()
}

/// Parse one BlueBubbles webhook body (JSON, or form-encoded with a
/// `payload`/`data`/`message` field — hermes fallback). Returns
/// `Ok(None)` for events to acknowledge silently (non-message events,
/// from-me, tapbacks, incomplete records), `Err` for malformed bodies.
pub fn bluebubbles_parse_webhook(body: &str) -> Result<Option<BlueBubblesEvent>> {
    let payload: Value = if let Ok(value) = serde_json::from_str(body) {
        value
    } else {
        // Form-encoded fallback (hermes parse_qs path).
        let mut payload_str = String::new();
        for pair in body.split('&') {
            let Some((key, value)) = pair.split_once('=') else {
                continue;
            };
            if matches!(key, "payload" | "data" | "message") {
                payload_str = url_decode_component(value);
                break;
            }
        }
        if payload_str.is_empty() {
            return Err(AgentError::config("bluebubbles webhook: empty payload"));
        }
        serde_json::from_str(&payload_str)
            .map_err(|e| AgentError::config(format!("bluebubbles webhook parse: {e}")))?
    };

    let event_type = first_str([payload.get("type"), payload.get("event")]);
    if !event_type.is_empty() && !BLUEBUBBLES_MESSAGE_EVENTS.contains(&event_type.as_str()) {
        return Ok(None);
    }

    let record = bluebubbles_extract_record(&payload);
    let is_from_me = ["isFromMe", "fromMe", "is_from_me"]
        .iter()
        .any(|key| record.get(*key).and_then(|v| v.as_bool()).unwrap_or(false));
    if is_from_me {
        return Ok(None);
    }
    // Tapback reactions delivered as messages.
    if let Some(code) = record.get("associatedMessageType").and_then(|v| v.as_i64()) {
        if BLUEBUBBLES_TAPBACK_CODES.contains(&code) {
            return Ok(None);
        }
    }

    let text = first_str([
        record.get("text"),
        record.get("message"),
        record.get("body"),
    ]);

    let mut attachments = Vec::new();
    if let Some(items) = record.get("attachments").and_then(|v| v.as_array()) {
        for att in items {
            let Some(guid) = att.get("guid").and_then(|v| v.as_str()) else {
                continue;
            };
            if guid.is_empty() {
                continue;
            }
            let mime = att
                .get("mimeType")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_lowercase();
            attachments.push((guid.to_string(), mime));
        }
    }

    if text.trim().is_empty() && !attachments.is_empty() {
        // hermes: media-only inbound carries an "(attachment)" marker.
    }

    let mut chat_guid = first_str([
        record.get("chatGuid"),
        payload.get("chatGuid"),
        record.get("chat_guid"),
        payload.get("chat_guid"),
        payload.get("guid"),
    ]);
    // BlueBubbles v1.9+ nests the chat GUID under chats[0].
    if chat_guid.is_empty() {
        if let Some(first) = record
            .get("chats")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
        {
            chat_guid = first_str([first.get("guid"), first.get("chatGuid")]);
        }
    }
    let chat_identifier = first_str([
        record.get("chatIdentifier"),
        record.get("identifier"),
        payload.get("chatIdentifier"),
        payload.get("identifier"),
    ]);
    let mut sender = String::new();
    if let Some(handle) = record.get("handle") {
        if let Some(address) = handle.get("address").and_then(|v| v.as_str()) {
            sender = address.to_string();
        }
    }
    if sender.is_empty() {
        sender = first_str([
            record.get("sender"),
            record.get("from"),
            record.get("address"),
        ]);
    }
    if sender.is_empty() {
        sender = chat_identifier.clone();
    }
    if sender.is_empty() {
        sender = chat_guid.clone();
    }
    let mut chat_identifier = chat_identifier;
    if chat_guid.is_empty() && chat_identifier.is_empty() && !sender.is_empty() {
        chat_identifier = sender.clone();
    }
    let chat_id = if !chat_guid.is_empty() {
        chat_guid.clone()
    } else {
        chat_identifier.clone()
    };
    if sender.is_empty() || chat_id.is_empty() || (text.trim().is_empty() && attachments.is_empty())
    {
        return Err(AgentError::config(
            "bluebubbles webhook: missing message fields",
        ));
    }
    let is_group = record
        .get("isGroup")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
        || chat_guid.contains(";+;");
    Ok(Some(BlueBubblesEvent {
        chat_id,
        chat_name: if chat_identifier.is_empty() {
            sender.clone()
        } else {
            chat_identifier
        },
        sender_id: sender,
        text,
        is_group,
        attachments,
    }))
}

fn url_decode_component(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() + 1 && i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

async fn bluebubbles_api_post(
    client: &reqwest::Client,
    cfg: &BlueBubblesConfig,
    path: &str,
    body: Value,
) -> Result<Value> {
    let url = bluebubbles_api_url(cfg, path)
        .ok_or_else(|| AgentError::config("bluebubbles: server_url/password not configured"))?;
    let response = client
        .post(&url)
        .json(&body)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| AgentError::Tool(format!("bluebubbles {path}: {e}")))?;
    let status = response.status();
    let value: Value = response.json().await.unwrap_or_else(|_| json!({}));
    if !status.is_success() {
        let message = value
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error");
        return Err(AgentError::Tool(format!(
            "bluebubbles {path} ({status}): {message}"
        )));
    }
    Ok(value)
}

async fn bluebubbles_api_get(
    client: &reqwest::Client,
    cfg: &BlueBubblesConfig,
    path: &str,
) -> Result<Value> {
    let url = bluebubbles_api_url(cfg, path)
        .ok_or_else(|| AgentError::config("bluebubbles: server_url/password not configured"))?;
    let response = client
        .get(&url)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| AgentError::Tool(format!("bluebubbles {path}: {e}")))?;
    let status = response.status();
    let value: Value = response.json().await.unwrap_or_else(|_| json!({}));
    if !status.is_success() {
        return Err(AgentError::Tool(format!(
            "bluebubbles {path}: HTTP {status}"
        )));
    }
    Ok(value)
}

/// LRU chat-GUID cache (hermes `_guid_cache`, cap 500).
pub struct BlueBubblesGuidCache {
    entries: std::sync::Mutex<std::collections::HashMap<String, String>>,
    order: std::sync::Mutex<Vec<String>>,
}

impl Default for BlueBubblesGuidCache {
    fn default() -> Self {
        Self {
            entries: std::sync::Mutex::new(HashMap::new()),
            order: std::sync::Mutex::new(Vec::new()),
        }
    }
}

impl BlueBubblesGuidCache {
    fn get(&self, key: &str) -> Option<String> {
        let value = self.entries.lock().unwrap().get(key).cloned()?;
        let mut order = self.order.lock().unwrap();
        order.retain(|k| k != key);
        order.push(key.to_string());
        Some(value)
    }

    fn put(&self, key: &str, value: &str) {
        self.entries
            .lock()
            .unwrap()
            .insert(key.to_string(), value.to_string());
        let mut order = self.order.lock().unwrap();
        order.retain(|k| k != key);
        order.push(key.to_string());
        while order.len() > BLUEBUBBLES_GUID_CACHE_SIZE {
            let oldest = order.remove(0);
            self.entries.lock().unwrap().remove(&oldest);
        }
    }
}

/// Resolve an email/phone/chat-identifier to a BlueBubbles chat GUID
/// (hermes `_resolve_chat_guid`): raw GUIDs (contain `;`) pass through;
/// otherwise strict chatIdentifier match from `/api/v1/chat/query` —
/// participant membership is deliberately NOT a fallback (hermes #24157).
pub async fn bluebubbles_resolve_chat_guid(
    client: &reqwest::Client,
    cfg: &BlueBubblesConfig,
    cache: &BlueBubblesGuidCache,
    target: &str,
) -> Option<String> {
    let target = target.trim();
    if target.is_empty() {
        return None;
    }
    if target.contains(';') {
        return Some(target.to_string());
    }
    if let Some(guid) = cache.get(target) {
        return Some(guid);
    }
    let payload = bluebubbles_api_post(
        client,
        cfg,
        "/api/v1/chat/query",
        json!({"limit": 100, "offset": 0}),
    )
    .await
    .ok()?;
    for chat in payload
        .get("data")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
    {
        let guid = first_str([chat.get("guid"), chat.get("chatGuid")]);
        let identifier = first_str([chat.get("chatIdentifier"), chat.get("identifier")]);
        if identifier == target && !guid.is_empty() {
            cache.put(target, &guid);
            return Some(guid);
        }
    }
    None
}

/// Download one attachment into the media cache (hermes
/// `_download_attachment`).
pub async fn bluebubbles_download_attachment(
    client: &reqwest::Client,
    cfg: &BlueBubblesConfig,
    attachment_guid: &str,
    declared_mime: &str,
) -> Option<MediaAttachmentRef> {
    let url = bluebubbles_api_url(
        cfg,
        &format!(
            "/api/v1/attachment/{}/download",
            urlencode_component(attachment_guid)
        ),
    )?;
    let response = client
        .get(&url)
        .timeout(std::time::Duration::from_secs(60))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let data = response.bytes().await.ok()?.to_vec();
    let mime = if declared_mime.trim().is_empty() {
        crate::signal::sniff_mime(&data).to_string()
    } else {
        declared_mime.to_string()
    };
    let path =
        crate::media_cache::cache_media_bytes(&crate::config::ulnclaw_home(), &data, &mime, "")
            .ok()?;
    Some(MediaAttachmentRef {
        path,
        mime,
        bytes: data.len() as u64,
    })
}

/// Cached attachment reference (converted to a messaging MediaAttachment
/// at the dispatch boundary).
#[derive(Debug, Clone)]
pub struct MediaAttachmentRef {
    pub path: std::path::PathBuf,
    pub mime: String,
    pub bytes: u64,
}

/// Send text (hermes `send`): paragraph-split bubbles, 4000-char cap,
/// GUID resolution, chat creation for address-looking targets.
pub async fn bluebubbles_send_text(
    client: &reqwest::Client,
    cfg: &BlueBubblesConfig,
    cache: &BlueBubblesGuidCache,
    chat_id: &str,
    text: &str,
) -> Result<()> {
    if text.trim().is_empty() {
        return Ok(());
    }
    let mut chunks: Vec<String> = Vec::new();
    let paragraphs: Vec<&str> = text
        .split("\n\n")
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    for paragraph in if paragraphs.is_empty() {
        vec![text.trim()]
    } else {
        paragraphs
    } {
        if paragraph.chars().count() <= BLUEBUBBLES_MAX_TEXT_LENGTH {
            chunks.push(paragraph.to_string());
        } else {
            chunks.extend(crate::messaging::chunk_text(
                paragraph,
                BLUEBUBBLES_MAX_TEXT_LENGTH,
            ));
        }
    }
    for chunk in chunks {
        let guid = match bluebubbles_resolve_chat_guid(client, cfg, cache, chat_id).await {
            Some(guid) => guid,
            None => {
                // Address-looking target → create a fresh DM with the
                // first chunk (hermes `_create_chat_for_handle`).
                if chat_id.contains('@') || chat_id.starts_with('+') {
                    let result = bluebubbles_api_post(
                        client,
                        cfg,
                        "/api/v1/chat/new",
                        json!({
                            "addresses": [chat_id],
                            "message": chunk,
                            "tempGuid": format!("temp-{}", now_millis()),
                        }),
                    )
                    .await?;
                    if result.get("error").is_some() {
                        return Err(AgentError::Tool(format!(
                            "bluebubbles chat/new: {}",
                            result["error"]
                        )));
                    }
                    return Ok(());
                }
                return Err(AgentError::Tool(format!(
                    "BlueBubbles chat not found for target: {chat_id}"
                )));
            }
        };
        bluebubbles_api_post(
            client,
            cfg,
            "/api/v1/message/text",
            json!({
                "chatGuid": guid,
                "tempGuid": format!("temp-{}", now_millis()),
                "message": chunk,
            }),
        )
        .await?;
    }
    Ok(())
}

fn now_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Send a file attachment via multipart upload (hermes
/// `_send_attachment`).
pub async fn bluebubbles_send_attachment(
    client: &reqwest::Client,
    cfg: &BlueBubblesConfig,
    cache: &BlueBubblesGuidCache,
    chat_id: &str,
    path: &std::path::Path,
) -> Result<()> {
    let Some(guid) = bluebubbles_resolve_chat_guid(client, cfg, cache, chat_id).await else {
        return Err(AgentError::Tool(format!("Chat not found: {chat_id}")));
    };
    let file_name = path
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_else(|| "attachment.bin".into());
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|e| AgentError::Tool(format!("bluebubbles attachment read: {e}")))?;
    let part = reqwest::multipart::Part::bytes(bytes)
        .file_name(file_name.clone())
        .mime_str("application/octet-stream")
        .map_err(|e| AgentError::Tool(format!("bluebubbles multipart: {e}")))?;
    let form = reqwest::multipart::Form::new()
        .part("attachment", part)
        .text("chatGuid", guid)
        .text("name", file_name)
        .text("tempGuid", uuid::Uuid::new_v4().simple().to_string());
    let url = bluebubbles_api_url(cfg, "/api/v1/message/attachment")
        .ok_or_else(|| AgentError::config("bluebubbles: server_url/password not configured"))?;
    let response = client
        .post(&url)
        .multipart(form)
        .timeout(std::time::Duration::from_secs(120))
        .send()
        .await
        .map_err(|e| AgentError::Tool(format!("bluebubbles attachment upload: {e}")))?;
    let status = response.status();
    let value: Value = response.json().await.unwrap_or_else(|_| json!({}));
    if status.is_success() && value.get("status").and_then(|v| v.as_i64()).unwrap_or(200) == 200 {
        return Ok(());
    }
    Err(AgentError::Tool(format!(
        "bluebubbles attachment upload ({}): {}",
        status,
        value
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("Attachment upload failed")
    )))
}

/// Register the webhook URL with the BlueBubbles server, deduplicating
/// against existing registrations (hermes `_register_webhook` crash
/// resilience).
pub async fn bluebubbles_register_webhook(
    client: &reqwest::Client,
    cfg: &BlueBubblesConfig,
) -> Result<()> {
    if cfg.webhook_url.trim().is_empty() {
        return Err(AgentError::config(
            "bluebubbles: webhook_url not configured — the server needs the externally \
             reachable URL of this gateway's /webhooks/bluebubbles route",
        ));
    }
    if let Ok(existing) = bluebubbles_api_get(client, cfg, "/api/v1/webhook").await {
        let urls: Vec<String> = existing
            .get("data")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
            .iter()
            .filter_map(|item| item.get("url").and_then(|v| v.as_str()).map(String::from))
            .collect();
        if urls.iter().any(|u| u == cfg.webhook_url.trim()) {
            return Ok(());
        }
    }
    bluebubbles_api_post(
        client,
        cfg,
        "/api/v1/webhook",
        json!({
            "url": cfg.webhook_url.trim(),
            "events": ["new-message", "updated-message"],
        }),
    )
    .await?;
    Ok(())
}

/// Startup handshake + webhook registration (hermes `connect`): ping,
/// server info log, webhook registration, sender registration.
pub async fn bluebubbles_startup(cfg: BlueBubblesConfig) {
    let client = reqwest::Client::new();
    match bluebubbles_api_get(&client, &cfg, "/api/v1/ping").await {
        Ok(_) => {}
        Err(e) => {
            eprintln!("[bluebubbles] cannot reach server: {e}");
            return;
        }
    }
    if let Ok(info) = bluebubbles_api_get(&client, &cfg, "/api/v1/server/info").await {
        let version = info
            .pointer("/data/server_version")
            .or_else(|| info.pointer("/data/version"))
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        eprintln!("[bluebubbles] connected (server v{version})");
    }
    match bluebubbles_register_webhook(&client, &cfg).await {
        Ok(()) => eprintln!("[bluebubbles] webhook registered: {}", cfg.webhook_url),
        Err(e) => eprintln!("[bluebubbles] webhook registration failed: {e}"),
    }
    crate::messaging::register_platform_sender("bluebubbles", Arc::new(BlueBubblesSender { cfg }));
}

/// PlatformSender for BlueBubbles (clarify prompts + echoes).
pub struct BlueBubblesSender {
    pub cfg: BlueBubblesConfig,
}

#[async_trait::async_trait]
impl crate::messaging::PlatformSender for BlueBubblesSender {
    async fn send_text(&self, chat_id: &str, text: &str) {
        let client = reqwest::Client::new();
        static CACHE: std::sync::OnceLock<BlueBubblesGuidCache> = std::sync::OnceLock::new();
        let cache = CACHE.get_or_init(BlueBubblesGuidCache::default);
        if let Err(e) = bluebubbles_send_text(&client, &self.cfg, cache, chat_id, text).await {
            eprintln!("[bluebubbles] send failed: {e}");
        }
    }
}

/// Full inbound webhook handling: parse → authz union (allowlist OR
/// pairing) → attachment downloads → dispatch → reply.
pub async fn bluebubbles_handle_webhook(
    cfg: &BlueBubblesConfig,
    dispatcher: &Arc<crate::messaging::Dispatcher>,
    pairing: Option<&crate::pairing::PairingStore>,
    body: &str,
    password_param: Option<&str>,
    password_header: Option<&str>,
) -> Result<()> {
    let Some(password) = bluebubbles_password(cfg) else {
        return Err(AgentError::config("bluebubbles: password not configured"));
    };
    let supplied = password_param.or(password_header).unwrap_or("");
    if supplied != password {
        return Err(AgentError::config("bluebubbles webhook: unauthorized"));
    }
    let event = match bluebubbles_parse_webhook(body)? {
        Some(event) => event,
        None => return Ok(()), // silent ack (from-me / tapback / non-message)
    };
    let authorized = cfg.allowed_chat_ids.iter().any(|allowed| {
        *allowed == event.chat_id || *allowed == event.sender_id || *allowed == event.chat_name
    }) || pairing
        .map(|store| store.is_approved("bluebubbles", &event.sender_id))
        .unwrap_or(false);
    if !authorized {
        eprintln!(
            "[bluebubbles] refusing message from {} — add the chat GUID/identifier to \
             messaging.bluebubbles.allowed_chat_ids or approve a pairing code",
            event.sender_id
        );
        if let Some(store) = pairing {
            if let Some(reply) = crate::messaging::pairing_offer_public(
                store,
                "bluebubbles",
                &event.sender_id,
                &event.sender_id,
            ) {
                let client = reqwest::Client::new();
                static CACHE: std::sync::OnceLock<BlueBubblesGuidCache> =
                    std::sync::OnceLock::new();
                let cache = CACHE.get_or_init(BlueBubblesGuidCache::default);
                if let Err(e) =
                    bluebubbles_send_text(&client, cfg, cache, &event.chat_id, &reply).await
                {
                    eprintln!("[bluebubbles] pairing reply failed: {e}");
                }
            }
        }
        return Ok(());
    }

    let client = reqwest::Client::new();
    let mut message_event = crate::messaging::MessageEvent {
        platform: "bluebubbles".into(),
        chat_id: event.chat_id.clone(),
        sender_id: event.sender_id.clone(),
        sender_name: event.chat_name.clone(),
        text: if event.text.trim().is_empty() && !event.attachments.is_empty() {
            "(attachment)".to_string()
        } else {
            event.text.clone()
        },
        message_id: String::new(),
        attachments: Vec::new(),
    };
    for (guid, mime) in &event.attachments {
        match bluebubbles_download_attachment(&client, cfg, guid, mime).await {
            Some(att) => message_event
                .attachments
                .push(crate::messaging::MediaAttachment {
                    path: att.path,
                    mime: att.mime,
                    bytes: att.bytes,
                    original_name: String::new(),
                }),
            None => eprintln!("[bluebubbles] failed to download attachment {guid}"),
        }
    }
    if !crate::messaging::pre_gateway_dispatch_gate_public(&mut message_event).await {
        return Ok(());
    }
    let outcome = match dispatcher.handle_event(message_event).await {
        Ok(outcome) => outcome,
        Err(e) => crate::messaging::DispatchOutcome {
            reply: format!("error: {e}"),
            transcript_echoes: Vec::new(),
        },
    };
    static CACHE: std::sync::OnceLock<BlueBubblesGuidCache> = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(BlueBubblesGuidCache::default);
    for echo in &outcome.transcript_echoes {
        if let Err(e) = bluebubbles_send_text(&client, cfg, cache, &event.chat_id, echo).await {
            eprintln!("[bluebubbles] transcript echo failed: {e}");
        }
    }
    let (reply_text, media_paths) = crate::messaging::extract_media_tags(&outcome.reply);
    if !reply_text.trim().is_empty() {
        // P704: ledger-protected reply delivery.
        dispatcher
            .try_send_with_ledger("bluebubbles", &event.chat_id, &reply_text, || async {
                match bluebubbles_send_text(&client, cfg, cache, &event.chat_id, &reply_text).await
                {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        eprintln!("[bluebubbles] reply failed: {e}");
                        Err(e.to_string())
                    }
                }
            })
            .await;
    }
    for path in &media_paths {
        if let Err(e) = bluebubbles_send_attachment(&client, cfg, cache, &event.chat_id, path).await
        {
            eprintln!("[bluebubbles] media delivery failed: {e}");
        }
    }
    Ok(())
}

//! Google Chat platform adapter — port of hermes
//! `plugins/platforms/google_chat` @ v2026.8.3 (adapter.py, HTTP
//! callback transport).
//!
//! Inbound events arrive at the gateway-mounted `/webhooks/googlechat`
//! route (hermes `GOOGLE_CHAT_HTTP_EVENTS_URL` mode). Requests carry a
//! Google-issued ID token in `Authorization: Bearer`; the token is
//! verified via Google's `tokeninfo` endpoint and must match the
//! configured audience + the bot's service-account email(s) — hermes'
//! local-cert verification path is replaced by the online tokeninfo
//! probe (documented divergence; the Pub/Sub streaming-pull inbound
//! mode needs gRPC and is not ported).
//!
//! Envelope handling mirrors hermes: only `MESSAGE` events dispatch,
//! `BOT` senders are skipped, message names dedup (300 s window),
//! `argumentText` wins over `text`, DIRECT_MESSAGE spaces map to DMs,
//! and thread names are carried so replies stay in-thread.
//!
//! Outbound rides the Chat REST API with service-account credentials:
//! an RS256-signed JWT assertion (`jsonwebtoken`) exchanged at the
//! key's `token_uri` for an access token (cached until ~5 min before
//! expiry), then `POST /v1/{space}/messages` with 4000-char chunks
//! (hermes `_MAX_TEXT_LENGTH`). Native file attachments use the
//! per-user OAuth `media.upload` flow (hermes `oauth.py`): users grant
//! the `chat.messages.create` scope once via `/setup-files`, and the
//! bot uploads + posts attachments AS the user. Typing-card patching
//! is not ported.

use crate::messaging::{Dispatcher, MessageEvent};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// hermes `_MAX_TEXT_LENGTH` for Chat messages.
const MAX_MESSAGE_LENGTH: usize = 4000;
const DEDUP_WINDOW_SECS: u64 = 300;
const DEDUP_MAX_SIZE: usize = 1000;
const API_TIMEOUT: Duration = Duration::from_secs(30);
const CHAT_SCOPE: &str = "https://www.googleapis.com/auth/chat.bot";
const TOKENINFO_URL: &str = "https://oauth2.googleapis.com/tokeninfo";

/// hermes `_TYPING_CONSUMED_SENTINEL` — slot value kept after send()
/// patches the typing marker into the real reply, so late marker
/// attempts no-op until the next turn clears it.
const TYPING_CONSUMED_SENTINEL: &str = "<consumed>";
/// Pub/Sub pull transport (hermes streaming_pull replacement — REST
/// pull speaks the same subscription without gRPC).
const PUBSUB_SCOPE: &str = "https://www.googleapis.com/auth/pubsub";
const PUBSUB_API_BASE: &str = "https://pubsub.googleapis.com/v1";
/// hermes `_MAX_RECONNECT_ATTEMPTS` / `_RECONNECT_BASE_DELAY` /
/// `_RECONNECT_MAX_DELAY` (full-jitter exponential backoff).
const MAX_RECONNECT_ATTEMPTS: u32 = 10;
const RECONNECT_BASE_DELAY_MS: u64 = 2_000;
const RECONNECT_MAX_DELAY_MS: u64 = 120_000;
/// hermes `GOOGLE_CHAT_MAX_MESSAGES` default.
const PULL_MAX_MESSAGES: u64 = 1;
/// Idle pacing between empty REST pulls (streaming pull blocks; REST
/// must not hot-spin).
const PULL_IDLE_DELAY: Duration = Duration::from_secs(1);
/// Chat media upload base (discovery `chat.media().upload`). Google
/// hard-rejects service-account auth on this endpoint — only user
/// OAuth tokens work.
const UPLOAD_API_BASE: &str = "https://chat.googleapis.com/upload/v1";
/// Chat REST base for user-authed `messages.create`.
const CHAT_API_BASE: &str = "https://chat.googleapis.com/v1";
/// Cache-key sentinel for the legacy single-user token slot (hermes
/// `_LEGACY_USER_IDENTITY`).
const LEGACY_USER_IDENTITY: &str = "__legacy__";

/// `[messaging.google_chat]` — Google Chat adapter (hermes
/// `platforms.google_chat` plugin config + `GOOGLE_CHAT_*` env vars).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GoogleChatConfig {
    pub enabled: bool,
    /// Path to the service-account JSON key (fallback
    /// `GOOGLE_CHAT_SERVICE_ACCOUNT_FILE`).
    pub service_account_file: String,
    /// Expected audience for inbound ID tokens (fallback
    /// `GOOGLE_CHAT_HTTP_EVENTS_AUDIENCE`) — usually the webhook URL.
    pub http_events_audience: String,
    /// Comma list of service-account emails allowed to push events
    /// (fallback `GOOGLE_CHAT_HTTP_EVENTS_SERVICE_ACCOUNT`).
    pub http_events_service_account: String,
    /// Sender emails or user ids allowed to talk to the bot (fallback
    /// `GOOGLE_CHAT_ALLOWED_USERS`).
    pub allowed_users: Vec<String>,
    /// Cron/notification delivery space (fallback
    /// `GOOGLE_CHAT_HOME_CHANNEL`).
    pub home_channel: String,
    /// Pub/Sub subscription (`projects/<p>/subscriptions/<s>`) for the
    /// pull transport (fallback `GOOGLE_CHAT_PUBSUB_SUBSCRIPTION`).
    pub pubsub_subscription: String,
    /// Typing-marker text override (fallback
    /// `GOOGLE_CHAT_TYPING_STATUS_TEXT`; hermes `typing_status_text`).
    /// Default marker is "ulnclaw is thinking…".
    pub typing_status_text: Option<String>,
}

impl Default for GoogleChatConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            service_account_file: String::new(),
            http_events_audience: String::new(),
            http_events_service_account: String::new(),
            allowed_users: Vec::new(),
            home_channel: String::new(),
            pubsub_subscription: String::new(),
            typing_status_text: None,
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

/// Chat API base — normally `https://chat.googleapis.com/v1`; the
/// `GOOGLE_CHAT_API_BASE` override exists for tests and corporate
/// proxies (mirrors the other adapters' *_API_BASE pattern).
fn google_chat_api_base() -> String {
    env_trim("GOOGLE_CHAT_API_BASE").unwrap_or_else(|| CHAT_API_BASE.to_string())
}

/// Resolved runtime settings (env > config, hermes precedence).
#[derive(Debug, Clone)]
pub struct ResolvedGoogleChat {
    pub service_account_file: String,
    pub http_events_audience: String,
    pub http_events_service_account: String,
    pub allowed_users: Vec<String>,
    pub home_channel: String,
    pub pubsub_subscription: String,
    pub typing_status_text: Option<String>,
}

impl GoogleChatConfig {
    pub fn resolve(&self) -> ResolvedGoogleChat {
        ResolvedGoogleChat {
            service_account_file: env_trim("GOOGLE_CHAT_SERVICE_ACCOUNT_FILE")
                .unwrap_or_else(|| self.service_account_file.clone()),
            http_events_audience: env_trim("GOOGLE_CHAT_HTTP_EVENTS_AUDIENCE")
                .unwrap_or_else(|| self.http_events_audience.clone()),
            http_events_service_account: env_trim("GOOGLE_CHAT_HTTP_EVENTS_SERVICE_ACCOUNT")
                .unwrap_or_else(|| self.http_events_service_account.clone()),
            allowed_users: env_list("GOOGLE_CHAT_ALLOWED_USERS")
                .unwrap_or_else(|| self.allowed_users.clone()),
            home_channel: env_trim("GOOGLE_CHAT_HOME_CHANNEL")
                .unwrap_or_else(|| self.home_channel.clone()),
            pubsub_subscription: env_trim("GOOGLE_CHAT_PUBSUB_SUBSCRIPTION")
                .unwrap_or_else(|| self.pubsub_subscription.trim().to_string()),
            typing_status_text: env_trim("GOOGLE_CHAT_TYPING_STATUS_TEXT")
                .or_else(|| self.typing_status_text.clone()),
        }
    }
}

/// Parsed service-account key (the fields the JWT flow needs).
#[derive(Debug, Clone)]
pub struct ServiceAccountKey {
    pub client_email: String,
    pub token_uri: String,
    pub private_key_pem: String,
}

/// Parse a Google service-account JSON key file.
pub fn parse_service_account_key(json_str: &str) -> Result<ServiceAccountKey, String> {
    let value: Value =
        serde_json::from_str(json_str).map_err(|e| format!("invalid key JSON: {e}"))?;
    let client_email = value
        .get("client_email")
        .and_then(|v| v.as_str())
        .ok_or("key missing client_email")?
        .to_string();
    let token_uri = value
        .get("token_uri")
        .and_then(|v| v.as_str())
        .unwrap_or("https://oauth2.googleapis.com/token")
        .to_string();
    let private_key_pem = value
        .get("private_key")
        .and_then(|v| v.as_str())
        .ok_or("key missing private_key")?
        .to_string();
    Ok(ServiceAccountKey {
        client_email,
        token_uri,
        private_key_pem,
    })
}

/// Build the RS256-signed JWT assertion for the service-account flow.
pub fn build_jwt_assertion(key: &ServiceAccountKey) -> Result<String, String> {
    build_jwt_assertion_scoped(key, CHAT_SCOPE)
}

/// Scope-parameterized variant (the Pub/Sub transport claims the
/// `pubsub` scope).
pub fn build_jwt_assertion_scoped(key: &ServiceAccountKey, scope: &str) -> Result<String, String> {
    use jsonwebtoken::{EncodingKey, Header};
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let claims = json!({
        "iss": key.client_email,
        "scope": scope,
        "aud": key.token_uri,
        "iat": now,
        "exp": now + 3600,
    });
    let encoding = EncodingKey::from_rsa_pem(key.private_key_pem.as_bytes())
        .map_err(|e| format!("private key parse: {e}"))?;
    jsonwebtoken::encode(
        &Header::new(jsonwebtoken::Algorithm::RS256),
        &claims,
        &encoding,
    )
    .map_err(|e| format!("jwt encode: {e}"))
}

struct Runtime {
    cfg: ResolvedGoogleChat,
    client: reqwest::Client,
    key: Option<ServiceAccountKey>,
    /// Cached access token + expiry.
    token: Mutex<Option<(String, Instant)>>,
    /// Pub/Sub-scoped token slot.
    pubsub_token: Mutex<Option<(String, Instant)>>,
    /// Message-name dedup (hermes MessageDeduplicator).
    dedup: Mutex<HashMap<String, u64>>,
    /// chat_id -> thread name for in-thread replies.
    threads: Mutex<HashMap<String, String>>,
    /// chat_id -> most recent inbound sender email (hermes
    /// `_last_sender_by_chat`) — per-user OAuth routing key.
    last_sender: Mutex<HashMap<String, String>>,
    /// Per-user OAuth token cache keyed by lowercased email plus the
    /// `__legacy__` slot (hermes `_user_creds_by_email` +
    /// `_user_credentials`).
    user_tokens: Mutex<HashMap<String, Option<crate::google_chat_oauth::UserToken>>>,
    /// chat_id -> typing-card message name or the consumed sentinel
    /// (hermes `_typing_messages`).
    typing_slots: Mutex<HashMap<String, String>>,
}

impl Runtime {
    async fn is_duplicate(&self, name: &str) -> bool {
        if name.is_empty() {
            return false;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut dedup = self.dedup.lock().await;
        dedup.retain(|_, ts| now.saturating_sub(*ts) < DEDUP_WINDOW_SECS);
        if dedup.contains_key(name) {
            return true;
        }
        if dedup.len() >= DEDUP_MAX_SIZE {
            let mut entries: Vec<(String, u64)> = dedup.drain().collect();
            entries.sort_by_key(|(_, ts)| *ts);
            entries.truncate(DEDUP_MAX_SIZE / 2);
            *dedup = entries.into_iter().collect();
        }
        dedup.insert(name.to_string(), now);
        false
    }

    /// Service-account access token with cache.
    async fn access_token(&self) -> Result<String, String> {
        {
            let cached = self.token.lock().await;
            if let Some((token, expiry)) = cached.as_ref() {
                if Instant::now() < *expiry {
                    return Ok(token.clone());
                }
            }
        }
        let key = self
            .key
            .as_ref()
            .ok_or("no service-account key configured")?;
        let assertion = build_jwt_assertion(key)?;
        let resp = self
            .client
            .post(&key.token_uri)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                ("assertion", assertion.as_str()),
            ])
            .send()
            .await
            .map_err(|e| format!("token request: {e}"))?;
        let status = resp.status();
        let payload: Value = resp.json().await.unwrap_or(json!({}));
        if status.as_u16() >= 400 {
            return Err(format!("token request failed ({status}): {payload}"));
        }
        let token = payload
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or("token response missing access_token")?
            .to_string();
        let expires_in = payload
            .get("expires_in")
            .and_then(|v| v.as_u64())
            .unwrap_or(3600);
        *self.token.lock().await = Some((
            token.clone(),
            Instant::now() + Duration::from_secs(expires_in.saturating_sub(300)),
        ));
        Ok(token)
    }

    /// hermes `_create_message` — POST /v1/{space}/messages.
    async fn create_message(
        &self,
        space_name: &str,
        text: &str,
        thread_name: Option<&str>,
    ) -> Result<(), String> {
        self.create_message_returning_name(space_name, text, thread_name)
            .await
            .map(|_| ())
    }

    /// hermes `_create_message` returning the created message's resource
    /// name — the typing-card flow needs it to patch the marker in-place
    /// once the reply is ready.
    async fn create_message_returning_name(
        &self,
        space_name: &str,
        text: &str,
        thread_name: Option<&str>,
    ) -> Result<String, String> {
        let token = self.access_token().await?;
        let url = format!("{}/{space_name}/messages", google_chat_api_base());
        let mut body = json!({ "text": text });
        if let Some(thread) = thread_name {
            body["thread"] = json!({ "name": thread });
        }
        let resp = self
            .client
            .post(&url)
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("message create: {e}"))?;
        if resp.status().as_u16() >= 400 {
            let status = resp.status();
            let err = resp.text().await.unwrap_or_default();
            return Err(format!(
                "message create failed ({status}): {}",
                &err[..err.len().min(300)]
            ));
        }
        let payload: Value = resp.json().await.unwrap_or(json!({}));
        Ok(payload
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string())
    }

    /// Chat API `messages.patch` — rewrite a message's text in-place
    /// (hermes `_patch_message`); avoids the "Message deleted by its
    /// author" tombstone a delete+create would leave.
    async fn patch_message_text(&self, message_name: &str, text: &str) -> Result<(), String> {
        let token = self.access_token().await?;
        let url = format!("{}/{message_name}?updateMask=text", google_chat_api_base());
        let resp = self
            .client
            .patch(&url)
            .bearer_auth(token)
            .json(&json!({ "text": text }))
            .send()
            .await
            .map_err(|e| format!("message patch: {e}"))?;
        if resp.status().as_u16() >= 400 {
            let status = resp.status();
            let err = resp.text().await.unwrap_or_default();
            return Err(format!(
                "message patch failed ({status}): {}",
                &err[..err.len().min(300)]
            ));
        }
        Ok(())
    }

    /// Typing-marker text — configured override wins (hermes
    /// `typing_status_text`).
    fn typing_marker_text(&self) -> String {
        self.cfg
            .typing_status_text
            .clone()
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| "ulnclaw is thinking…".to_string())
    }

    /// Post the visible "…is thinking" marker card (hermes `send_typing`).
    /// ulnclaw awaits the create before dispatching the turn — hermes
    /// runs it as a shielded background task against its `_keep_typing`
    /// timer and needs in-flight/orphan bookkeeping for cancellation
    /// races; the awaited create has no such race, so the slot is always
    /// claimed by the time the reply starts.
    async fn send_typing_marker(&self, chat_id: &str, thread_name: Option<&str>) {
        {
            let mut slots = self.typing_slots.lock().await;
            match slots.get(chat_id) {
                // Previous turn's consumed sentinel — clear it so this
                // turn gets a fresh marker.
                Some(existing) if existing == TYPING_CONSUMED_SENTINEL => {
                    slots.remove(chat_id);
                }
                // Live marker already up — bail (hermes slot check).
                Some(_) => return,
                None => {}
            }
        }
        match self
            .create_message_returning_name(chat_id, &self.typing_marker_text(), thread_name)
            .await
        {
            Ok(name) if !name.is_empty() => {
                let mut slots = self.typing_slots.lock().await;
                slots.entry(chat_id.to_string()).or_insert(name);
            }
            Ok(_) => {}
            Err(e) => eprintln!("[google_chat] typing marker failed: {e}"),
        }
    }

    /// Pop the typing slot (hermes send() `_typing_messages.pop`): a
    /// live marker name comes back for patching; the consumed sentinel
    /// maps to None.
    async fn take_typing_slot(&self, chat_id: &str) -> Option<String> {
        let slot = self.typing_slots.lock().await.remove(chat_id)?;
        if slot == TYPING_CONSUMED_SENTINEL {
            None
        } else {
            Some(slot)
        }
    }

    /// Mark the slot consumed (hermes `_TYPING_CONSUMED_SENTINEL`) after
    /// patching the marker into the reply, so late marker attempts no-op
    /// until the next turn.
    async fn mark_typing_consumed(&self, chat_id: &str) {
        self.typing_slots
            .lock()
            .await
            .insert(chat_id.to_string(), TYPING_CONSUMED_SENTINEL.to_string());
    }
}

/// Reply delivery with hermes `send()` typing-card semantics: the first
/// chunk PATCHes the pending "…is thinking" marker in-place (no delete
/// tombstone), remaining chunks create new messages; a patch failure
/// degrades to creating a fresh message (hermes 404 fallback).
async fn send_patching_typing(
    runtime: &Runtime,
    chat_id: &str,
    text: &str,
    thread_name: Option<&str>,
) {
    let typing_card = runtime.take_typing_slot(chat_id).await;
    let mut patched = false;
    let chunks = crate::messaging::chunk_text(text, MAX_MESSAGE_LENGTH);
    for (idx, chunk) in chunks.iter().enumerate() {
        if idx == 0 {
            if let Some(name) = typing_card.as_deref() {
                match runtime.patch_message_text(name, chunk).await {
                    Ok(()) => {
                        patched = true;
                        continue;
                    }
                    Err(e) => eprintln!(
                        "[google_chat] typing-card patch failed, creating new message: {e}"
                    ),
                }
            }
        }
        if let Err(e) = runtime.create_message(chat_id, chunk, thread_name).await {
            eprintln!("[google_chat] send failed: {e}");
        }
    }
    if patched {
        runtime.mark_typing_consumed(chat_id).await;
    }
}

static RUNTIME: std::sync::OnceLock<Arc<Runtime>> = std::sync::OnceLock::new();

/// Register the adapter (called from `run_messaging` when enabled).
pub fn register(cfg: &GoogleChatConfig) {
    let resolved = cfg.resolve();
    let key = if resolved.service_account_file.is_empty() {
        eprintln!(
            "[google_chat] no service-account file configured (GOOGLE_CHAT_SERVICE_ACCOUNT_FILE) — outbound sends disabled"
        );
        None
    } else {
        match std::fs::read_to_string(&resolved.service_account_file) {
            Ok(contents) => match parse_service_account_key(&contents) {
                Ok(key) => Some(key),
                Err(e) => {
                    eprintln!("[google_chat] failed to parse service-account key: {e}");
                    None
                }
            },
            Err(e) => {
                eprintln!(
                    "[google_chat] cannot read {}: {e} — outbound sends disabled",
                    resolved.service_account_file
                );
                None
            }
        }
    };
    let runtime = Arc::new(Runtime {
        client: reqwest::Client::builder()
            .timeout(API_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new()),
        cfg: resolved,
        key,
        token: Mutex::new(None),
        pubsub_token: Mutex::new(None),
        dedup: Mutex::new(HashMap::new()),
        threads: Mutex::new(HashMap::new()),
        last_sender: Mutex::new(HashMap::new()),
        user_tokens: Mutex::new(HashMap::new()),
        typing_slots: Mutex::new(HashMap::new()),
    });
    let _ = RUNTIME.set(runtime.clone());
    crate::messaging::register_platform_sender(
        "google_chat",
        Arc::new(GoogleChatSender {
            runtime: runtime.clone(),
        }),
    );
}

fn runtime() -> Option<Arc<Runtime>> {
    RUNTIME.get().cloned()
}

/// hermes `verify_http_event_request` — tokeninfo-based ID token
/// verification (audience + service-account email match).
pub async fn verify_http_event_request(
    client: &reqwest::Client,
    auth_header: &str,
    audience: &str,
    expected_emails_csv: &str,
) -> Result<(), String> {
    if audience.is_empty() || expected_emails_csv.trim().is_empty() {
        return Err("google_chat_http_events_not_configured".into());
    }
    let Some(token) = auth_header.strip_prefix("Bearer ").map(|t| t.trim()) else {
        return Err("missing_google_bearer".into());
    };
    if token.is_empty() {
        return Err("missing_google_bearer".into());
    }
    let resp = client
        .get(TOKENINFO_URL)
        .query(&[("id_token", token)])
        .send()
        .await
        .map_err(|e| format!("tokeninfo request: {e}"))?;
    if !resp.status().is_success() {
        return Err("invalid_google_bearer".into());
    }
    let claims: Value = resp.json().await.unwrap_or(json!({}));
    let aud = claims.get("aud").and_then(|v| v.as_str()).unwrap_or("");
    if aud != audience {
        return Err("invalid_google_bearer".into());
    }
    let email = claims
        .get("email")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();
    let expected: Vec<String> = expected_emails_csv
        .split(',')
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    if email.is_empty() || !expected.iter().any(|e| e == &email) {
        return Err("unexpected_google_bearer_identity".into());
    }
    Ok(())
}

/// Webhook response handed back to the gateway route.
pub struct GoogleChatWebhookResponse {
    pub status: u16,
    pub body: Value,
}

fn ack() -> GoogleChatWebhookResponse {
    GoogleChatWebhookResponse {
        status: 200,
        body: json!({}),
    }
}

/// Gateway webhook entry point, mounted at `/webhooks/googlechat`.
pub async fn google_chat_handle_webhook(
    dispatcher: &Arc<Dispatcher>,
    pairing: Option<&crate::pairing::PairingStore>,
    body: &[u8],
    headers: &[(String, String)],
) -> GoogleChatWebhookResponse {
    let Some(runtime) = runtime() else {
        return GoogleChatWebhookResponse {
            status: 503,
            body: json!({ "error": "google_chat adapter not registered" }),
        };
    };
    let auth_header = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    if let Err(reason) = verify_http_event_request(
        &runtime.client,
        &auth_header,
        &runtime.cfg.http_events_audience,
        &runtime.cfg.http_events_service_account,
    )
    .await
    {
        let status = if reason == "google_chat_http_events_not_configured" {
            501
        } else {
            401
        };
        return GoogleChatWebhookResponse {
            status,
            body: json!({ "error": reason }),
        };
    }
    let envelope: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => {
            return GoogleChatWebhookResponse {
                status: 400,
                body: json!({ "error": "invalid event JSON" }),
            }
        }
    };
    process_envelope(&runtime, dispatcher, pairing, &envelope).await
}

/// Shared envelope pipeline for the HTTP webhook and the Pub/Sub pull
/// transport (hermes `dispatch_http_event` / `_on_pubsub_message`
/// converge on the same dispatch path).
async fn process_envelope(
    runtime: &Arc<Runtime>,
    dispatcher: &Arc<Dispatcher>,
    pairing: Option<&crate::pairing::PairingStore>,
    envelope: &Value,
) -> GoogleChatWebhookResponse {
    let event_type = envelope.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if event_type != "MESSAGE" {
        // ADDED_TO_SPACE / REMOVED_FROM_SPACE / CARD_CLICKED are acked.
        return ack();
    }
    let message = envelope.get("message").cloned().unwrap_or(json!({}));
    // Bot senders never loop back.
    if message
        .pointer("/sender/type")
        .and_then(|v| v.as_str())
        .map(|t| t == "BOT")
        .unwrap_or(false)
    {
        return ack();
    }
    let msg_name = message
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if runtime.is_duplicate(&msg_name).await {
        return ack();
    }
    let space = envelope
        .get("space")
        .or_else(|| message.get("space"))
        .cloned()
        .unwrap_or(json!({}));
    let space_name = space
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let space_type = space.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if space_name.is_empty() {
        return ack();
    }
    let thread_name = message
        .pointer("/thread/name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let text = message
        .get("argumentText")
        .or_else(|| message.get("text"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let sender = message.get("sender").cloned().unwrap_or(json!({}));
    let sender_id = sender
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let sender_email = sender
        .get("email")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let sender_display = sender
        .get("displayName")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| sender_email.clone());
    let _is_dm = matches!(space_type, "DIRECT_MESSAGE" | "DM");

    // Allowlist ∪ pairing gate (email or user resource name).
    let authorized = runtime.cfg.allowed_users.iter().any(|u| {
        u == "*"
            || (!sender_email.is_empty() && u.eq_ignore_ascii_case(&sender_email))
            || u == &sender_id
    });
    if !authorized {
        if let Some(store) = pairing {
            let key = if sender_email.is_empty() {
                sender_id.clone()
            } else {
                sender_email.clone()
            };
            if !store.is_approved("google_chat", &key) {
                if let Some(code_msg) = crate::messaging::pairing_offer_public(
                    store,
                    "google_chat",
                    &key,
                    &sender_display,
                ) {
                    let _ = runtime.create_message(&space_name, &code_msg, None).await;
                }
                return ack();
            }
        } else {
            eprintln!("[google_chat] unauthorized sender {sender_email} — add to allowed_users");
            return ack();
        }
    }
    // Per-chat sender tracking for per-user OAuth routing (hermes
    // `_last_sender_by_chat`), then the bot-local `/setup-files`
    // intercept — an OAuth setup command, never an agent prompt.
    if !sender_email.is_empty() {
        runtime
            .last_sender
            .lock()
            .await
            .insert(space_name.clone(), sender_email.trim().to_lowercase());
    }
    if text.starts_with("/setup-files") {
        let thread_opt = if thread_name.is_empty() {
            None
        } else {
            Some(thread_name.as_str())
        };
        handle_setup_files(runtime, &space_name, thread_opt, &sender_email, &text).await;
        return ack();
    }
    if text.is_empty() {
        return ack();
    }
    // Reply threading: keep answers in the inbound thread when one
    // exists (hermes group behavior; DM thread heuristics simplified).
    if !thread_name.is_empty() {
        runtime
            .threads
            .lock()
            .await
            .insert(space_name.clone(), thread_name.clone());
    }
    let event = MessageEvent {
        platform: "google_chat".into(),
        chat_id: space_name.clone(),
        sender_id: if sender_id.is_empty() {
            sender_email.clone()
        } else {
            sender_id
        },
        sender_name: sender_display,
        text: text.clone(),
        message_id: msg_name,
        attachments: Vec::new(),
    };
    let mut gate_check = event.clone();
    if !crate::messaging::pre_gateway_dispatch_gate_public(&mut gate_check).await {
        return ack();
    }
    // Typing marker card — posted before the turn starts so the reply
    // can patch it in-place (hermes send_typing before processing).
    {
        let thread = runtime.threads.lock().await.get(&space_name).cloned();
        runtime
            .send_typing_marker(&space_name, thread.as_deref())
            .await;
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
    let (reply_text, media) = crate::messaging::extract_media_tags(&full);
    let reply_text = reply_text.trim().to_string();
    if !reply_text.is_empty() {
        // P705: ledger-protected reply delivery.
        dispatcher
            .send_with_ledger("google_chat", &space_name, &reply_text, || async {
                let thread = runtime.threads.lock().await.get(&space_name).cloned();
                send_patching_typing(runtime, &space_name, &reply_text, thread.as_deref()).await;
            })
            .await;
    } else if let Some(name) = runtime.take_typing_slot(&space_name).await {
        // No reply text — finalize the marker instead of stranding a
        // "thinking" card (hermes on_processing_complete "(interrupted)"
        // patch on the failure/cancellation path).
        if let Err(e) = runtime.patch_message_text(&name, "(interrupted)").await {
            eprintln!("[google_chat] typing-card cleanup failed: {e}");
        }
    }
    // MEDIA: tags — native attachment delivery via per-user OAuth
    // (hermes `_send_file` per media file).
    for path in &media {
        let thread = runtime.threads.lock().await.get(&space_name).cloned();
        if let Err(e) = send_file(runtime, &space_name, path, None, thread.as_deref()).await {
            eprintln!("[google_chat] media delivery failed: {e}");
        }
    }
    ack()
}

struct GoogleChatSender {
    runtime: Arc<Runtime>,
}

#[async_trait::async_trait]
impl crate::messaging::PlatformSender for GoogleChatSender {
    async fn send_text(&self, chat_id: &str, text: &str) {
        let thread = self.runtime.threads.lock().await.get(chat_id).cloned();
        send_patching_typing(&self.runtime, chat_id, text, thread.as_deref()).await;
    }
}

// ---------------------------------------------------------------------------
// Per-user OAuth attachment delivery (hermes `oauth.py` + `_send_file` +
// `_handle_setup_files_command`)
// ---------------------------------------------------------------------------

/// Extension-based MIME guess for MEDIA: deliveries (hermes passes
/// per-method hints; the generic path infers from the filename).
pub fn guess_mime(path: &std::path::Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") | Some("jpg") | Some("jpeg") | Some("gif") | Some("webp") | Some("bmp") => {
            "image/*"
        }
        Some("mp4") | Some("mov") | Some("webm") | Some("mkv") => "video/mp4",
        Some("mp3") => "audio/mpeg",
        Some("ogg") | Some("opus") => "audio/ogg",
        Some("wav") => "audio/wav",
        Some("pdf") => "application/pdf",
        Some("txt") | Some("md") | Some("log") | Some("csv") => "text/plain",
        Some("json") => "application/json",
        Some("zip") => "application/zip",
        _ => "application/octet-stream",
    }
}

/// Argument portion of a `/setup-files` message ("" for bare status).
pub fn setup_files_arg(raw_text: &str) -> String {
    let mut parts = raw_text.splitn(2, char::is_whitespace);
    parts.next();
    parts.next().unwrap_or("").trim().to_string()
}

/// Upload endpoint for a space (hermes `media().upload(parent=...)`).
pub fn upload_url(space_name: &str) -> String {
    format!("{UPLOAD_API_BASE}/{space_name}/attachments:upload")
}

/// `messages.create` URL; `messageReplyOption` is required for
/// `thread.name` in the body to be honored (hermes `_create_message`
/// API quirk).
pub fn attachment_create_url(space_name: &str, threaded: bool) -> String {
    let mut url = format!("{CHAT_API_BASE}/{space_name}/messages");
    if threaded {
        url.push_str("?messageReplyOption=REPLY_MESSAGE_FALLBACK_TO_NEW_THREAD");
    }
    url
}

/// Body for the attachment-bearing `messages.create` (hermes
/// `_send_file` body assembly).
pub fn attachment_message_body(
    attachment_ref: Value,
    caption: Option<&str>,
    thread_name: Option<&str>,
) -> Value {
    let mut body = json!({ "attachment": [{ "attachmentDataRef": attachment_ref }] });
    if let Some(caption) = caption.filter(|c| !c.is_empty()) {
        body["text"] = json!(caption);
    }
    if let Some(thread) = thread_name {
        body["thread"] = json!({ "name": thread });
    }
    body
}

/// Text notice when user OAuth is unavailable (hermes
/// `_post_attachment_fallback`; English rendering of the original).
pub fn attachment_fallback_text(
    path: &std::path::Path,
    filename: &str,
    caption: Option<&str>,
) -> String {
    let mut lines: Vec<String> = Vec::new();
    if let Some(caption) = caption.filter(|c| !c.is_empty()) {
        lines.push(caption.to_string());
    }
    lines.extend([
        format!("⚠️ I couldn't attach **{filename}**."),
        "Google Chat only allows file attachments when the bot has your explicit permission (user OAuth). It's a one-time consent you grant from this chat.".to_string(),
        "**To enable it:** send `/setup-files` and follow the instructions.".to_string(),
        format!("In the meantime the file is on the host: `{}`", path.display()),
    ]);
    lines.join("\n")
}

/// Token-cache lookup with refresh-on-expiry (hermes
/// `_load_per_user_chat_api` + `refresh_or_none`: a failed refresh
/// evicts the slot).
async fn cached_user_token(
    runtime: &Arc<Runtime>,
    home: &std::path::Path,
    email: Option<&str>,
) -> Option<String> {
    let cache_key = email
        .map(|e| e.to_string())
        .unwrap_or_else(|| LEGACY_USER_IDENTITY.to_string());
    {
        let mut cache = runtime.user_tokens.lock().await;
        if let Some(slot) = cache.get(&cache_key) {
            match slot {
                Some(token) if !token.expired() => return Some(token.access_token.clone()),
                Some(token) => {
                    let refreshed = crate::google_chat_oauth::refresh_token(
                        home,
                        &runtime.client,
                        token,
                        email,
                    )
                    .await;
                    return match refreshed {
                        Some(t) => {
                            let access = t.access_token.clone();
                            cache.insert(cache_key, Some(t));
                            Some(access)
                        }
                        None => {
                            cache.remove(&cache_key);
                            None
                        }
                    };
                }
                None => return None,
            }
        }
    }
    let creds =
        crate::google_chat_oauth::load_user_credentials(home, &runtime.client, email).await?;
    let access = creds.access_token.clone();
    runtime
        .user_tokens
        .lock()
        .await
        .insert(cache_key, Some(creds));
    Some(access)
}

/// Resolve the user OAuth token for an outbound attachment (hermes
/// `_acquire_user_chat_api`): per-user token for the chat's most
/// recent sender, then the legacy single-user slot. Returns
/// `(access_token, identity)`.
async fn acquire_user_token(
    runtime: &Arc<Runtime>,
    sender_email: Option<&str>,
) -> Option<(String, String)> {
    let home = crate::config::ulnclaw_home();
    let per_user = sender_email
        .map(|e| e.trim().to_lowercase())
        .filter(|e| !e.is_empty());
    if let Some(email) = per_user.as_deref() {
        if let Some(token) = cached_user_token(runtime, &home, Some(email)).await {
            return Some((token, email.to_string()));
        }
    }
    if let Some(token) = cached_user_token(runtime, &home, None).await {
        return Some((token, LEGACY_USER_IDENTITY.to_string()));
    }
    None
}

/// hermes `_send_file` — native Chat attachment via user OAuth:
/// multipart upload to `attachments:upload`, then `messages.create`
/// referencing the returned `attachmentDataRef`. BOTH calls use the
/// user token — Google rejects service accounts on media.upload.
async fn send_file(
    runtime: &Arc<Runtime>,
    space_name: &str,
    path: &std::path::Path,
    caption: Option<&str>,
    thread_name: Option<&str>,
) -> Result<(), String> {
    if !path.is_file() {
        return Err(format!("file not found: {}", path.display()));
    }
    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("upload.bin")
        .to_string();
    let sender_email = runtime.last_sender.lock().await.get(space_name).cloned();
    let Some((user_token, identity)) = acquire_user_token(runtime, sender_email.as_deref()).await
    else {
        // No user OAuth — surface setup instructions in chat instead of
        // silently failing.
        let notice = attachment_fallback_text(path, &filename, caption);
        return runtime
            .create_message(space_name, &notice, thread_name)
            .await
            .map_err(|e| format!("attachment fallback notice: {e}"));
    };
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let form = reqwest::multipart::Form::new()
        .part(
            "metadata",
            reqwest::multipart::Part::bytes(
                json!({ "filename": filename }).to_string().into_bytes(),
            )
            .mime_str("application/json")
            .map_err(|e| format!("multipart metadata: {e}"))?,
        )
        .part(
            "file",
            reqwest::multipart::Part::bytes(bytes)
                .file_name(filename.clone())
                .mime_str(guess_mime(path))
                .map_err(|e| format!("multipart media: {e}"))?,
        );
    let resp = runtime
        .client
        .post(upload_url(space_name))
        .bearer_auth(&user_token)
        .multipart(form)
        .send()
        .await
        .map_err(|e| format!("media upload: {e}"))?;
    let status = resp.status().as_u16();
    if status == 401 || status == 403 {
        eprintln!(
            "[google_chat] media.upload auth failure for identity={identity} (token revoked or scope missing) — falling back to text notice"
        );
        runtime.user_tokens.lock().await.remove(&identity);
        let notice = attachment_fallback_text(path, &filename, caption);
        return runtime
            .create_message(space_name, &notice, thread_name)
            .await
            .map_err(|e| format!("attachment fallback notice: {e}"));
    }
    let body_text = resp.text().await.unwrap_or_default();
    if status >= 400 {
        return Err(format!(
            "media upload failed ({status}): {}",
            &body_text[..body_text.len().min(300)]
        ));
    }
    let upload_resp: Value = serde_json::from_str(&body_text).unwrap_or(json!({}));
    let attachment_ref = upload_resp
        .get("attachmentDataRef")
        .cloned()
        .ok_or("upload returned no attachmentDataRef")?;
    let body = attachment_message_body(attachment_ref, caption, thread_name);
    let resp = runtime
        .client
        .post(attachment_create_url(space_name, thread_name.is_some()))
        .bearer_auth(&user_token)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("attachment message create: {e}"))?;
    if resp.status().as_u16() >= 400 {
        let status = resp.status();
        let err = resp.text().await.unwrap_or_default();
        return Err(format!(
            "attachment message create failed ({status}): {}",
            &err[..err.len().min(300)]
        ));
    }
    Ok(())
}

/// Reply helper for the setup flow (send failures are logged, never
/// fatal — hermes `_reply`).
async fn setup_reply(runtime: &Arc<Runtime>, space: &str, thread: Option<&str>, text: &str) {
    if let Err(e) = runtime.create_message(space, text, thread).await {
        eprintln!("[google_chat] /setup-files reply failed: {e}");
    }
}

/// hermes `_handle_setup_files_command` — in-chat per-user OAuth setup.
/// Subcommands: bare status, `start`, `revoke`, `<code-or-url>`
/// exchange. The sender email is the per-user token slot key.
async fn handle_setup_files(
    runtime: &Arc<Runtime>,
    space_name: &str,
    thread_name: Option<&str>,
    sender_email: &str,
    raw_text: &str,
) {
    let home = crate::config::ulnclaw_home();
    let sender_key_owned = sender_email.trim().to_lowercase();
    let sender_key: Option<&str> = if sender_key_owned.is_empty() {
        None
    } else {
        Some(sender_key_owned.as_str())
    };
    let cache_key = sender_key
        .map(|k| k.to_string())
        .unwrap_or_else(|| LEGACY_USER_IDENTITY.to_string());
    let arg = setup_files_arg(raw_text);

    if arg.is_empty() {
        // Status: what's configured and what to do next.
        if crate::google_chat_oauth::token_path(&home, sender_key).exists()
            && crate::google_chat_oauth::load_user_credentials(&home, &runtime.client, sender_key)
                .await
                .is_some()
        {
            let who = sender_key.unwrap_or("shared (legacy)");
            let path = crate::google_chat_oauth::token_path(&home, sender_key);
            setup_reply(
                runtime,
                space_name,
                thread_name,
                &format!(
                    "✅ Native attachment delivery is **active** for `{who}`.\nToken: `{}`\nSend `/setup-files revoke` to disable.",
                    path.display()
                ),
            )
            .await;
            return;
        }
        if !crate::google_chat_oauth::client_secret_path(&home).exists() {
            setup_reply(
                runtime,
                space_name,
                thread_name,
                "🔧 Native attachment delivery is **not configured**.\n\
                 **Step 1 (one-time, on the host):** create OAuth client credentials at \
                 https://console.cloud.google.com/apis/credentials → *Create credentials* → \
                 *OAuth client ID* → *Desktop app*. Download the JSON. Then on the host run:\n\
                 ```\n\
                 ulnclaw google-chat-oauth client-secret /path/to/client_secret.json\n\
                 ```\n\
                 **Step 2:** come back here and send `/setup-files start`.",
            )
            .await;
            return;
        }
        setup_reply(
            runtime,
            space_name,
            thread_name,
            "🔧 Client credentials are stored but you haven't authorized yet. Send `/setup-files start` to begin.",
        )
        .await;
        return;
    }

    if arg == "start" {
        if !crate::google_chat_oauth::client_secret_path(&home).exists() {
            setup_reply(
                runtime,
                space_name,
                thread_name,
                "⚠️ No client credentials stored for this profile. Send `/setup-files` (no args) for setup instructions.",
            )
            .await;
            return;
        }
        match crate::google_chat_oauth::start_auth(&home, sender_key) {
            Ok(auth_url) => {
                setup_reply(
                    runtime,
                    space_name,
                    thread_name,
                    &format!(
                        "1. Open this URL in your browser and authorize:\n{auth_url}\n\n\
                         2. After clicking *Allow*, your browser will fail to load \
                         `http://localhost:1/?...&code=...`. That's expected.\n\n\
                         3. Copy the entire failed URL from the browser's URL bar and paste it \
                         back here as: `/setup-files <PASTE_URL>` (or just the `code=...` value).\n\n\
                         Tip: the URL contains your access grant — keep it private."
                    ),
                )
                .await;
            }
            Err(e) => {
                setup_reply(runtime, space_name, thread_name, &format!("❌ Error: {e}")).await;
            }
        }
        return;
    }

    if arg == "revoke" {
        let output = crate::google_chat_oauth::revoke(&home, &runtime.client, sender_key).await;
        // Scope the eviction to this sender's slot (hermes: Bob revoking
        // must not break Alice's token nor the shared legacy fallback).
        runtime.user_tokens.lock().await.remove(&cache_key);
        setup_reply(
            runtime,
            space_name,
            thread_name,
            &format!("✅ Done.\n```\n{output}\n```"),
        )
        .await;
        return;
    }

    // Anything else is the pasted auth code or failed-redirect URL.
    match crate::google_chat_oauth::exchange_code(&home, &runtime.client, &arg, sender_key).await {
        Ok(scope) => {
            if let Some(creds) =
                crate::google_chat_oauth::load_user_credentials(&home, &runtime.client, sender_key)
                    .await
            {
                runtime
                    .user_tokens
                    .lock()
                    .await
                    .insert(cache_key, Some(creds));
                setup_reply(
                    runtime,
                    space_name,
                    thread_name,
                    "✅ Authorized! Native attachment delivery is now active. Try asking me to send you a file.",
                )
                .await;
            } else {
                setup_reply(
                    runtime,
                    space_name,
                    thread_name,
                    &format!("✅ Authorized (scope: {scope})."),
                )
                .await;
            }
        }
        Err(e) => {
            setup_reply(
                runtime,
                space_name,
                thread_name,
                &format!(
                    "❌ Token exchange failed: {e}\nSend `/setup-files start` to get a fresh OAuth URL."
                ),
            )
            .await;
        }
    }
}

// ---------------------------------------------------------------------------
// Pub/Sub pull transport (hermes streaming_pull supervisor +
// `_on_pubsub_message`, REST pull in place of gRPC)
// ---------------------------------------------------------------------------

impl Runtime {
    /// Pub/Sub-scoped service-account token (cached separately from the
    /// Chat API token).
    async fn pubsub_access_token(&self) -> Result<String, String> {
        {
            let cached = self.pubsub_token.lock().await;
            if let Some((token, expiry)) = cached.as_ref() {
                if Instant::now() < *expiry {
                    return Ok(token.clone());
                }
            }
        }
        let key = self
            .key
            .as_ref()
            .ok_or("no service-account key configured")?;
        let assertion = build_jwt_assertion_scoped(key, PUBSUB_SCOPE)?;
        let resp = self
            .client
            .post(&key.token_uri)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                ("assertion", assertion.as_str()),
            ])
            .send()
            .await
            .map_err(|e| format!("pubsub token request: {e}"))?;
        let status = resp.status();
        let payload: Value = resp.json().await.unwrap_or(json!({}));
        if status.as_u16() >= 400 {
            return Err(format!("pubsub token request failed ({status}): {payload}"));
        }
        let token = payload
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or("pubsub token response missing access_token")?
            .to_string();
        let expires_in = payload
            .get("expires_in")
            .and_then(|v| v.as_u64())
            .unwrap_or(3600);
        *self.pubsub_token.lock().await = Some((
            token.clone(),
            Instant::now() + Duration::from_secs(expires_in.saturating_sub(300)),
        ));
        Ok(token)
    }
}

/// Normalize the subscription to the `projects/<p>/subscriptions/<s>`
/// resource path (accepts bare names when the project prefix is
/// already present).
pub fn pubsub_subscription_path(subscription: &str) -> String {
    subscription.trim().to_string()
}

/// hermes `_extract_message_payload` — detect the three accepted
/// envelope formats and return `(message, space)`.
pub fn extract_message_payload(envelope: &Value, ce_type: &str) -> Option<(Value, Value)> {
    // Format 1 — Workspace Add-ons (canonical, ce-type driven).
    if let Some(wrapper) = envelope.pointer("/chat/messagePayload") {
        let msg = wrapper.get("message").cloned().unwrap_or(json!({}));
        let space = wrapper
            .get("space")
            .cloned()
            .or_else(|| msg.get("space").cloned())
            .unwrap_or(json!({}));
        return Some((msg, space));
    }
    // Format 2 — native Chat API Pub/Sub.
    if envelope
        .get("message")
        .map(|v| v.is_object())
        .unwrap_or(false)
    {
        if envelope.get("type").and_then(|v| v.as_str()) != Some("MESSAGE") {
            return None;
        }
        let msg = envelope.get("message").cloned().unwrap_or(json!({}));
        let space = envelope
            .get("space")
            .cloned()
            .or_else(|| msg.get("space").cloned())
            .unwrap_or(json!({}));
        return Some((msg, space));
    }
    // Format 3 — relay / flat.
    if envelope.get("event_type").is_some() || envelope.get("sender_email").is_some() {
        let event_type = envelope
            .get("event_type")
            .and_then(|v| v.as_str())
            .unwrap_or("MESSAGE");
        if event_type != "MESSAGE" {
            return None;
        }
        let sender_email = envelope
            .get("sender_email")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let sender_display = envelope
            .get("sender_display_name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                if sender_email.is_empty() {
                    "Unknown".to_string()
                } else {
                    sender_email.clone()
                }
            });
        let surrogate = format!(
            "users/relay-{}",
            if sender_email.is_empty() {
                "unknown".to_string()
            } else {
                sender_email.replace('@', "_at_").replace('.', "_")
            }
        );
        let text = envelope
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let sender_type = envelope
            .get("sender_type")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_uppercase())
            .filter(|s| s == "HUMAN" || s == "BOT")
            .unwrap_or_else(|| "HUMAN".into());
        let mut msg = json!({
            "name": envelope.get("message_name").and_then(|v| v.as_str()).unwrap_or(""),
            "sender": {
                "name": surrogate,
                "email": sender_email,
                "displayName": sender_display,
                "type": sender_type,
            },
            "text": text,
            "argumentText": text,
        });
        if let Some(thread_name) = envelope
            .get("thread_name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            msg["thread"] = json!({ "name": thread_name });
        }
        let space = json!({
            "name": envelope.get("space_name").and_then(|v| v.as_str()).unwrap_or(""),
            "spaceType": envelope.get("space_type").and_then(|v| v.as_str()).unwrap_or("SPACE"),
        });
        return Some((msg, space));
    }
    let _ = ce_type;
    None
}

/// One Pub/Sub delivery (hermes `_on_pubsub_message`): membership and
/// card events are logged + acked; message payloads normalize into the
/// webhook envelope shape and flow through `process_envelope`.
async fn process_pubsub_message(
    runtime: &Arc<Runtime>,
    dispatcher: &Arc<Dispatcher>,
    pairing: Option<&crate::pairing::PairingStore>,
    envelope: &Value,
    attributes: &Value,
) {
    let ce_type = attributes
        .get("ce-type")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if ce_type.contains("membership") || ce_type.contains("MEMBERSHIP") {
        if ce_type.contains("created") {
            if let Some(name) = envelope
                .pointer("/chat/membershipPayload/membership/member/name")
                .and_then(|v| v.as_str())
            {
                let member_type = envelope
                    .pointer("/chat/membershipPayload/membership/member/type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if member_type == "BOT" {
                    eprintln!("[google_chat] ADDED_TO_SPACE (bot id {name})");
                }
            }
        } else {
            eprintln!("[google_chat] REMOVED_FROM_SPACE");
        }
        return;
    }
    if ce_type.contains("widget") || ce_type.to_lowercase().contains("card") {
        eprintln!("[google_chat] card/widget event ack'd (v2 feature, deferred)");
        return;
    }
    let Some((msg, space)) = extract_message_payload(envelope, &ce_type) else {
        return;
    };
    // Synthesize the webhook-shaped envelope; BOT filter + dedup +
    // gating happen inside process_envelope.
    let mut message = msg;
    if let Some(obj) = message.as_object_mut() {
        if !obj.contains_key("space") && space.is_object() {
            obj.insert("space".into(), space.clone());
        }
    }
    let normalized = json!({
        "type": "MESSAGE",
        "message": message,
        "space": space,
    });
    process_envelope(runtime, dispatcher, pairing, &normalized).await;
}

/// One pull batch; errors classify as fatal (auth/permission) or
/// retryable (hermes supervisor exceptions).
async fn pull_batch(
    runtime: &Arc<Runtime>,
    dispatcher: &Arc<Dispatcher>,
    pairing: Option<&crate::pairing::PairingStore>,
    subscription: &str,
) -> Result<(), String> {
    let token = runtime.pubsub_access_token().await?;
    let path = pubsub_subscription_path(subscription);
    let resp = runtime
        .client
        .post(format!("{PUBSUB_API_BASE}/{path}:pull"))
        .bearer_auth(&token)
        .json(&json!({ "maxMessages": PULL_MAX_MESSAGES }))
        .timeout(API_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("pull request: {e}"))?;
    let status = resp.status().as_u16();
    if status == 401 {
        return Err("unauthenticated".into());
    }
    if status == 403 {
        return Err("permission_denied".into());
    }
    let body_text = resp.text().await.unwrap_or_default();
    if status >= 400 {
        return Err(format!(
            "pull HTTP {status}: {}",
            &body_text[..body_text.len().min(200)]
        ));
    }
    let payload: Value = serde_json::from_str(&body_text).unwrap_or(json!({}));
    let received = payload
        .get("receivedMessages")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if received.is_empty() {
        // REST pull pacing — streaming pull blocks server-side; the
        // REST equivalent must not hot-spin on an idle subscription.
        tokio::time::sleep(PULL_IDLE_DELAY).await;
        return Ok(());
    }
    let mut ack_ids: Vec<String> = Vec::new();
    for received_msg in &received {
        let ack_id = received_msg
            .get("ackId")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let data_b64 = received_msg
            .pointer("/message/data")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let envelope: Value = base64::engine::general_purpose::STANDARD
            .decode(data_b64)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or(json!({}));
        let attributes = received_msg
            .pointer("/message/attributes")
            .cloned()
            .unwrap_or(json!({}));
        process_pubsub_message(runtime, dispatcher, pairing, &envelope, &attributes).await;
        // hermes acks even malformed deliveries to avoid redelivery
        // loops.
        if !ack_id.is_empty() {
            ack_ids.push(ack_id);
        }
    }
    if !ack_ids.is_empty() {
        let token = runtime.pubsub_access_token().await?;
        let resp = runtime
            .client
            .post(format!("{PUBSUB_API_BASE}/{path}:acknowledge"))
            .bearer_auth(token)
            .json(&json!({ "ackIds": ack_ids }))
            .timeout(API_TIMEOUT)
            .send()
            .await;
        if let Ok(resp) = resp {
            if !resp.status().is_success() {
                eprintln!(
                    "[google_chat] pubsub acknowledge failed: HTTP {}",
                    resp.status()
                );
            }
        }
    }
    Ok(())
}

fn random_fraction() -> f64 {
    let mut bytes = [0u8; 4];
    crate::feishu::fill_random_bytes(&mut bytes);
    u32::from_le_bytes(bytes) as f64 / u32::MAX as f64
}

/// Pub/Sub pull supervisor (hermes `_run_supervisor`): full-jitter
/// exponential backoff, fatal on auth/permission errors or after 10
/// failed attempts.
pub async fn run_pubsub(
    dispatcher: Arc<Dispatcher>,
    pairing: Option<Arc<crate::pairing::PairingStore>>,
) {
    let Some(runtime) = runtime() else {
        return;
    };
    let subscription = runtime.cfg.pubsub_subscription.trim().to_string();
    if subscription.is_empty() {
        return;
    }
    if runtime.key.is_none() {
        eprintln!(
            "[google_chat] pubsub subscription configured but no service-account key — pull transport not started"
        );
        return;
    }
    eprintln!("[google_chat] pub/sub pull transport started on {subscription}");
    let mut attempt: u32 = 0;
    loop {
        match pull_batch(&runtime, &dispatcher, pairing.as_deref(), &subscription).await {
            Ok(()) => {
                if attempt > 0 {
                    eprintln!("[google_chat] pub/sub stream recovered after {attempt} attempt(s)");
                }
                attempt = 0;
            }
            Err(e) => {
                if e == "unauthenticated" {
                    eprintln!(
                        "[google_chat] pub/sub authentication failed (SA key invalid/revoked) — transport stopped"
                    );
                    return;
                }
                if e == "permission_denied" {
                    eprintln!(
                        "[google_chat] SA lacks pubsub.subscriber on the subscription — transport stopped"
                    );
                    return;
                }
                attempt += 1;
                eprintln!(
                    "[google_chat] pub/sub pull failed (attempt {attempt}/{MAX_RECONNECT_ATTEMPTS}): {e}"
                );
                if attempt >= MAX_RECONNECT_ATTEMPTS {
                    eprintln!("[google_chat] pub/sub reconnect failed {attempt} times; giving up");
                    return;
                }
                let delay_ms = RECONNECT_MAX_DELAY_MS
                    .min(RECONNECT_BASE_DELAY_MS.saturating_mul(2u64.saturating_pow(attempt - 1)));
                let jitter_ms = (random_fraction() * delay_ms as f64) as u64;
                tokio::time::sleep(Duration::from_millis(jitter_ms)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_account_key_parsing() {
        let key_json = json!({
            "type": "service_account",
            "client_email": "bot@project.iam.gserviceaccount.com",
            "token_uri": "https://oauth2.googleapis.com/token",
            "private_key": "-----BEGIN PRIVATE KEY-----\nabc\n-----END PRIVATE KEY-----\n",
        })
        .to_string();
        let key = parse_service_account_key(&key_json).unwrap();
        assert_eq!(key.client_email, "bot@project.iam.gserviceaccount.com");
        assert_eq!(key.token_uri, "https://oauth2.googleapis.com/token");
        assert!(key.private_key_pem.contains("PRIVATE KEY"));
    }

    #[test]
    fn service_account_key_missing_fields() {
        assert!(parse_service_account_key("{}").is_err());
        assert!(parse_service_account_key("not json").is_err());
        let no_pk = json!({"client_email": "a@b.c"}).to_string();
        assert!(parse_service_account_key(&no_pk).is_err());
    }

    #[test]
    fn jwt_assertion_needs_valid_rsa_key() {
        let key = ServiceAccountKey {
            client_email: "bot@p.iam.gserviceaccount.com".into(),
            token_uri: "https://oauth2.googleapis.com/token".into(),
            private_key_pem: "not a pem".into(),
        };
        assert!(build_jwt_assertion(&key).is_err());
    }

    #[tokio::test]
    async fn dedup_window() {
        let runtime = Runtime {
            client: reqwest::Client::new(),
            cfg: GoogleChatConfig::default().resolve(),
            key: None,
            token: Mutex::new(None),
            pubsub_token: Mutex::new(None),
            dedup: Mutex::new(HashMap::new()),
            threads: Mutex::new(HashMap::new()),
            last_sender: Mutex::new(HashMap::new()),
            user_tokens: Mutex::new(HashMap::new()),
            typing_slots: Mutex::new(HashMap::new()),
        };
        assert!(!runtime.is_duplicate("spaces/a/messages/1").await);
        assert!(runtime.is_duplicate("spaces/a/messages/1").await);
        assert!(!runtime.is_duplicate("spaces/a/messages/2").await);
    }

    #[tokio::test]
    async fn verify_requires_configuration() {
        let client = reqwest::Client::new();
        let err = verify_http_event_request(&client, "Bearer x", "", "").await;
        assert_eq!(err.unwrap_err(), "google_chat_http_events_not_configured");
        let err = verify_http_event_request(&client, "Basic x", "aud", "a@b.c").await;
        assert_eq!(err.unwrap_err(), "missing_google_bearer");
        let err = verify_http_event_request(&client, "Bearer ", "aud", "a@b.c").await;
        assert_eq!(err.unwrap_err(), "missing_google_bearer");
    }

    #[test]
    fn envelope_type_routing() {
        // Only MESSAGE dispatches; the rest ack.
        for (ty, dispatches) in [
            ("MESSAGE", true),
            ("ADDED_TO_SPACE", false),
            ("REMOVED_FROM_SPACE", false),
            ("CARD_CLICKED", false),
        ] {
            assert_eq!(ty == "MESSAGE", dispatches);
        }
    }

    #[test]
    fn sender_bot_skip_and_text_preference() {
        let msg: Value =
            serde_json::from_str(r#"{"sender":{"type":"BOT"},"text":"echo","argumentText":"arg"}"#)
                .unwrap();
        assert!(msg.pointer("/sender/type").and_then(|v| v.as_str()) == Some("BOT"));
        let text = msg
            .get("argumentText")
            .or_else(|| msg.get("text"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert_eq!(text, "arg");
    }

    #[test]
    fn dm_vs_group_space_types() {
        for (space_type, is_dm) in [
            ("DIRECT_MESSAGE", true),
            ("DM", true),
            ("ROOM", false),
            ("SPACE", false),
        ] {
            assert_eq!(matches!(space_type, "DIRECT_MESSAGE" | "DM"), is_dm);
        }
    }

    #[test]
    fn resolve_env_precedence() {
        let _guard = crate::models_dev::test_env_lock();
        std::env::set_var("GOOGLE_CHAT_ALLOWED_USERS", "a@x.com, b@x.com");
        std::env::set_var(
            "GOOGLE_CHAT_HTTP_EVENTS_AUDIENCE",
            "https://gw.example.com/webhooks/googlechat",
        );
        let cfg = GoogleChatConfig::default();
        let resolved = cfg.resolve();
        assert_eq!(
            resolved.allowed_users,
            vec!["a@x.com".to_string(), "b@x.com".to_string()]
        );
        assert_eq!(
            resolved.http_events_audience,
            "https://gw.example.com/webhooks/googlechat"
        );
        std::env::remove_var("GOOGLE_CHAT_ALLOWED_USERS");
        std::env::remove_var("GOOGLE_CHAT_HTTP_EVENTS_AUDIENCE");
    }

    #[test]
    fn chunk_limit_matches_hermes() {
        assert_eq!(MAX_MESSAGE_LENGTH, 4000);
        assert_eq!(CHAT_SCOPE, "https://www.googleapis.com/auth/chat.bot");
        assert_eq!(PUBSUB_SCOPE, "https://www.googleapis.com/auth/pubsub");
        assert_eq!(MAX_RECONNECT_ATTEMPTS, 10);
        assert_eq!(PULL_MAX_MESSAGES, 1);
    }

    #[test]
    fn extract_workspace_addons_format() {
        let envelope = json!({
            "chat": {
                "messagePayload": {
                    "message": { "name": "spaces/X/messages/1", "text": "hi" },
                    "space": { "name": "spaces/X" },
                },
            },
        });
        let (msg, space) =
            extract_message_payload(&envelope, "google.workspace.chat.message.v1.created").unwrap();
        assert_eq!(msg["text"], "hi");
        assert_eq!(space["name"], "spaces/X");
    }

    #[test]
    fn extract_native_chat_api_format() {
        let envelope = json!({
            "type": "MESSAGE",
            "message": { "text": "hello", "space": { "name": "spaces/Y" } },
            "space": { "name": "spaces/Y" },
        });
        let (msg, space) = extract_message_payload(&envelope, "").unwrap();
        assert_eq!(msg["text"], "hello");
        assert_eq!(space["name"], "spaces/Y");
        // Non-MESSAGE type is dropped.
        let envelope = json!({ "type": "ADDED_TO_SPACE", "message": {} });
        assert!(extract_message_payload(&envelope, "").is_none());
    }

    #[test]
    fn extract_relay_flat_format() {
        let envelope = json!({
            "event_type": "MESSAGE",
            "sender_email": "alice@example.com",
            "sender_display_name": "Alice",
            "text": "ping",
            "space_name": "spaces/Z",
            "thread_name": "spaces/Z/threads/T",
            "message_name": "spaces/Z/messages/M",
        });
        let (msg, space) = extract_message_payload(&envelope, "").unwrap();
        assert_eq!(msg["text"], "ping");
        assert_eq!(msg["sender"]["email"], "alice@example.com");
        // Default sender_type is HUMAN.
        assert_eq!(msg["sender"]["type"], "HUMAN");
        assert_eq!(msg["sender"]["name"], "users/relay-alice_at_example_com");
        assert_eq!(msg["thread"]["name"], "spaces/Z/threads/T");
        assert_eq!(space["name"], "spaces/Z");
        // Declared BOT sender_type is honored (self-filter).
        let envelope = json!({ "event_type": "MESSAGE", "sender_email": "bot@x.com", "text": "t", "sender_type": "bot" });
        let (msg, _) = extract_message_payload(&envelope, "").unwrap();
        assert_eq!(msg["sender"]["type"], "BOT");
    }

    #[test]
    fn extract_unknown_format_returns_none() {
        assert!(extract_message_payload(&json!({}), "").is_none());
        assert!(extract_message_payload(&json!({"foo": "bar"}), "").is_none());
    }

    #[test]
    fn resolve_pubsub_subscription_env() {
        let _guard = crate::models_dev::test_env_lock();
        std::env::set_var(
            "GOOGLE_CHAT_PUBSUB_SUBSCRIPTION",
            "projects/p/subscriptions/s",
        );
        let cfg = GoogleChatConfig::default();
        let resolved = cfg.resolve();
        assert_eq!(resolved.pubsub_subscription, "projects/p/subscriptions/s");
        std::env::remove_var("GOOGLE_CHAT_PUBSUB_SUBSCRIPTION");
    }

    #[test]
    fn setup_files_arg_parsing() {
        assert_eq!(setup_files_arg("/setup-files"), "");
        assert_eq!(setup_files_arg("/setup-files   "), "");
        assert_eq!(setup_files_arg("/setup-files start"), "start");
        assert_eq!(setup_files_arg("/setup-files revoke"), "revoke");
        let url = "http://localhost:1/?state=x&code=abc&scope=s";
        assert_eq!(setup_files_arg(&format!("/setup-files {url}")), url);
    }

    #[test]
    fn upload_and_create_urls() {
        assert_eq!(
            upload_url("spaces/AAA"),
            "https://chat.googleapis.com/upload/v1/spaces/AAA/attachments:upload"
        );
        assert_eq!(
            attachment_create_url("spaces/AAA", false),
            "https://chat.googleapis.com/v1/spaces/AAA/messages"
        );
        assert_eq!(
            attachment_create_url("spaces/AAA", true),
            "https://chat.googleapis.com/v1/spaces/AAA/messages?messageReplyOption=REPLY_MESSAGE_FALLBACK_TO_NEW_THREAD"
        );
    }

    #[test]
    fn attachment_message_body_assembly() {
        let body = attachment_message_body(json!({"resourceName": "r1"}), None, None);
        assert_eq!(
            body["attachment"][0]["attachmentDataRef"]["resourceName"],
            "r1"
        );
        assert!(body.get("text").is_none());
        assert!(body.get("thread").is_none());
        let body = attachment_message_body(
            json!({"resourceName": "r1"}),
            Some("caption"),
            Some("spaces/AAA/threads/T"),
        );
        assert_eq!(body["text"], "caption");
        assert_eq!(body["thread"]["name"], "spaces/AAA/threads/T");
        // Empty caption is omitted.
        let body = attachment_message_body(json!({}), Some(""), None);
        assert!(body.get("text").is_none());
    }

    #[test]
    fn fallback_notice_contents() {
        let text = attachment_fallback_text(
            std::path::Path::new("/tmp/report.pdf"),
            "report.pdf",
            Some("Here you go"),
        );
        assert!(text.starts_with("Here you go"));
        assert!(text.contains("**report.pdf**"));
        assert!(text.contains("/setup-files"));
        assert!(text.contains("/tmp/report.pdf"));
        let bare = attachment_fallback_text(std::path::Path::new("/tmp/x.bin"), "x.bin", None);
        assert!(bare.starts_with("⚠️"));
    }

    #[test]
    fn mime_guessing() {
        use std::path::Path;
        assert_eq!(guess_mime(Path::new("a.PNG")), "image/*");
        assert_eq!(guess_mime(Path::new("a.mp4")), "video/mp4");
        assert_eq!(guess_mime(Path::new("a.ogg")), "audio/ogg");
        assert_eq!(guess_mime(Path::new("a.pdf")), "application/pdf");
        assert_eq!(
            guess_mime(Path::new("a.unknown")),
            "application/octet-stream"
        );
        assert_eq!(guess_mime(Path::new("noext")), "application/octet-stream");
    }

    // -- Typing-card (messages.patch) parity ------------------------------

    /// Runtime with a pre-seeded access token so the tests skip the JWT
    /// service-account flow entirely.
    fn typing_runtime() -> Runtime {
        Runtime {
            client: reqwest::Client::new(),
            cfg: GoogleChatConfig::default().resolve(),
            key: None,
            token: Mutex::new(Some((
                "TOK".to_string(),
                Instant::now() + Duration::from_secs(3600),
            ))),
            pubsub_token: Mutex::new(None),
            dedup: Mutex::new(HashMap::new()),
            threads: Mutex::new(HashMap::new()),
            last_sender: Mutex::new(HashMap::new()),
            user_tokens: Mutex::new(HashMap::new()),
            typing_slots: Mutex::new(HashMap::new()),
        }
    }

    /// axum mock of the Chat API — logs (method, path, body); POST
    /// returns a deterministic message name for the typing-card patch.
    async fn spawn_chat_api(log: Arc<std::sync::Mutex<Vec<(String, String, Value)>>>) -> String {
        use axum::extract::State;
        use axum::routing::post;
        type Log = Arc<std::sync::Mutex<Vec<(String, String, Value)>>>;
        let app = axum::Router::new()
            .route(
                "/v1/*rest",
                post(
                    move |State(log): State<Log>,
                          axum::extract::Path(rest): axum::extract::Path<String>,
                          axum::Json(body): axum::Json<Value>| async move {
                        log.lock()
                            .unwrap()
                            .push(("POST".into(), rest.clone(), body));
                        axum::Json(json!({"name": "spaces/x/messages/m-1"}))
                    },
                )
                .patch(
                    move |State(log): State<Log>,
                          axum::extract::Path(rest): axum::extract::Path<String>,
                          axum::Json(body): axum::Json<Value>| async move {
                        log.lock()
                            .unwrap()
                            .push(("PATCH".into(), rest.clone(), body));
                        axum::Json(json!({}))
                    },
                ),
            )
            .with_state(log);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        // CHAT_API_BASE semantics: the base already carries the /v1
        // prefix.
        format!("http://{addr}/v1")
    }

    #[tokio::test]
    async fn typing_marker_created_then_reply_patches_in_place() {
        let _env_guard = crate::models_dev::test_env_lock();
        let log = Arc::new(std::sync::Mutex::new(Vec::<(String, String, Value)>::new()));
        let base = spawn_chat_api(log.clone()).await;
        std::env::set_var("GOOGLE_CHAT_API_BASE", &base);
        let runtime = typing_runtime();
        // Turn start → marker card posted into the thread.
        runtime
            .send_typing_marker("spaces/x", Some("spaces/x/threads/t1"))
            .await;
        {
            let reqs = log.lock().unwrap();
            assert_eq!(reqs.len(), 1);
            assert_eq!(reqs[0].0, "POST");
            assert_eq!(reqs[0].2["text"], "ulnclaw is thinking…");
            assert_eq!(reqs[0].2["thread"]["name"], "spaces/x/threads/t1");
        }
        // Reply arrives → the marker is patched in-place, no new message.
        send_patching_typing(&runtime, "spaces/x", "The answer.", None).await;
        {
            let reqs = log.lock().unwrap();
            assert_eq!(reqs.len(), 2);
            assert_eq!(reqs[1].0, "PATCH");
            assert_eq!(reqs[1].1, "spaces/x/messages/m-1");
            assert_eq!(reqs[1].2["text"], "The answer.");
        }
        // Slot consumed — a fresh marker for the next turn posts again.
        runtime.send_typing_marker("spaces/x", None).await;
        {
            let reqs = log.lock().unwrap();
            assert_eq!(reqs.len(), 3);
            assert_eq!(reqs[2].0, "POST");
        }
        std::env::remove_var("GOOGLE_CHAT_API_BASE");
    }

    #[tokio::test]
    async fn typing_marker_not_duplicated_when_live() {
        let _env_guard = crate::models_dev::test_env_lock();
        let log = Arc::new(std::sync::Mutex::new(Vec::<(String, String, Value)>::new()));
        let base = spawn_chat_api(log.clone()).await;
        std::env::set_var("GOOGLE_CHAT_API_BASE", &base);
        let runtime = typing_runtime();
        runtime.send_typing_marker("spaces/x", None).await;
        // Second call while the card is live is a no-op (hermes slot
        // check) — no duplicate "thinking…" card.
        runtime.send_typing_marker("spaces/x", None).await;
        assert_eq!(log.lock().unwrap().len(), 1);
        std::env::remove_var("GOOGLE_CHAT_API_BASE");
    }

    #[tokio::test]
    async fn multi_chunk_reply_patches_first_creates_rest() {
        let _env_guard = crate::models_dev::test_env_lock();
        let log = Arc::new(std::sync::Mutex::new(Vec::<(String, String, Value)>::new()));
        let base = spawn_chat_api(log.clone()).await;
        std::env::set_var("GOOGLE_CHAT_API_BASE", &base);
        let runtime = typing_runtime();
        runtime.send_typing_marker("spaces/x", None).await;
        // > MAX_MESSAGE_LENGTH forces two chunks: the first patches the
        // marker, the second creates a new message (hermes send()).
        let long = "a".repeat(MAX_MESSAGE_LENGTH + 10);
        send_patching_typing(&runtime, "spaces/x", &long, None).await;
        let reqs = log.lock().unwrap();
        let methods: Vec<String> = reqs.iter().map(|(m, _, _)| m.clone()).collect();
        assert_eq!(methods, vec!["POST", "PATCH", "POST"]);
    }

    #[tokio::test]
    async fn no_typing_card_falls_back_to_create() {
        let _env_guard = crate::models_dev::test_env_lock();
        let log = Arc::new(std::sync::Mutex::new(Vec::<(String, String, Value)>::new()));
        let base = spawn_chat_api(log.clone()).await;
        std::env::set_var("GOOGLE_CHAT_API_BASE", &base);
        let runtime = typing_runtime();
        // No marker posted → reply just creates a message.
        send_patching_typing(&runtime, "spaces/x", "plain reply", None).await;
        let reqs = log.lock().unwrap();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].0, "POST");
        assert_eq!(reqs[0].2["text"], "plain reply");
        std::env::remove_var("GOOGLE_CHAT_API_BASE");
    }

    #[test]
    fn typing_marker_text_override_and_default() {
        let _env_guard = crate::models_dev::test_env_lock();
        std::env::set_var("GOOGLE_CHAT_TYPING_STATUS_TEXT", "Cooking…");
        let runtime = typing_runtime();
        assert_eq!(runtime.typing_marker_text(), "Cooking…");
        std::env::remove_var("GOOGLE_CHAT_TYPING_STATUS_TEXT");
        let runtime = typing_runtime();
        assert_eq!(runtime.typing_marker_text(), "ulnclaw is thinking…");
    }
}

//! Microsoft Teams platform adapter — port of hermes
//! `plugins/platforms/teams` @ v2026.8.3 (adapter.py).
//!
//! hermes rides the `microsoft-teams-apps` Python SDK; this port speaks
//! the raw Bot Framework protocol directly. Inbound activities arrive
//! at the gateway-mounted `/webhooks/teams` route (hermes runs the SDK
//! aiohttp server on `TEAMS_PORT` 3978): `message` activities are
//! deduped by id, `<at>` mention HTML is stripped, conversation type
//! maps to dm/group/channel, and attachments download into the media
//! cache (file-consent `downloadUrl` payloads, `image/*` contentUrl,
//! generic contentUrl with the bot bearer; `text/html` mirrors and
//! `application/vnd.microsoft.card.*` are skipped — hermes rules).
//!
//! Outbound replies acquire a Bot Framework token via the OAuth2
//! client-credentials flow
//! (`login.microsoftonline.com/<tenant>/oauth2/v2.0/token`, scope
//! `https://api.botframework.com/.default`, cached until near expiry)
//! and POST `{serviceUrl}v3/conversations/<conv>/activities` markdown
//! message activities. The service URL is validated against the hermes
//! Bot Framework host allowlist (SSRF/token-exfiltration guard) and
//! conversation ids against the Bot Framework id character set.
//!
//! Exec approvals render as AdaptiveCard v1.4 prompts with
//! Action.Execute Allow/Deny buttons (hermes `send_exec_approval`);
//! button taps arrive as `adaptiveCard/action` invoke activities and
//! resolve the session's blocking approval with a default-deny
//! allowlist gate (hermes `_on_card_action`). Known differences: the
//! summary-writer incoming-webhook/Graph paths are not ported; typing
//! indicators are best-effort activity posts.

use crate::messaging::{Dispatcher, MediaAttachment, MessageEvent};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const MAX_MESSAGE_LENGTH: usize = 4000;
const DEDUP_WINDOW_SECS: u64 = 300;
const DEDUP_MAX_SIZE: usize = 1000;
const API_TIMEOUT: Duration = Duration::from_secs(15);
/// hermes `_DEFAULT_TEAMS_SERVICE_URL`.
const DEFAULT_TEAMS_SERVICE_URL: &str = "https://smba.trafficmanager.net/teams/";
/// hermes `_ALLOWED_TEAMS_SERVICE_HOSTS` (SSRF guard).
const ALLOWED_TEAMS_SERVICE_HOSTS: [&str; 2] = [
    "smba.trafficmanager.net",
    "smba.infra.gov.teams.microsoft.us",
];

/// `[messaging.teams]` — Teams adapter (hermes `platforms.teams`
/// plugin config + `TEAMS_*` env vars).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TeamsConfig {
    pub enabled: bool,
    /// Azure app registration client id (fallback `TEAMS_CLIENT_ID`).
    pub client_id: String,
    /// Client secret (fallback `TEAMS_CLIENT_SECRET`).
    pub client_secret: String,
    /// Directory tenant id (fallback `TEAMS_TENANT_ID`).
    pub tenant_id: String,
    /// Bot Framework service host (fallback `TEAMS_SERVICE_URL`); must
    /// be on the allowlist.
    pub service_url: String,
    /// User ids allowed to talk to the bot (fallback
    /// `TEAMS_ALLOWED_USERS`).
    pub allowed_users: Vec<String>,
    /// Cron/notification conversation id (fallback
    /// `TEAMS_HOME_CHANNEL`).
    pub home_channel: String,
}

impl Default for TeamsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            client_id: String::new(),
            client_secret: String::new(),
            tenant_id: String::new(),
            service_url: String::new(),
            allowed_users: Vec::new(),
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

/// Resolved runtime settings (env > config, hermes precedence).
#[derive(Debug, Clone)]
pub struct ResolvedTeams {
    pub client_id: String,
    pub client_secret: String,
    pub tenant_id: String,
    pub service_url: String,
    pub allowed_users: Vec<String>,
    pub home_channel: String,
}

impl TeamsConfig {
    pub fn resolve(&self) -> ResolvedTeams {
        ResolvedTeams {
            client_id: env_trim("TEAMS_CLIENT_ID").unwrap_or_else(|| self.client_id.clone()),
            client_secret: env_trim("TEAMS_CLIENT_SECRET")
                .unwrap_or_else(|| self.client_secret.clone()),
            tenant_id: env_trim("TEAMS_TENANT_ID").unwrap_or_else(|| self.tenant_id.clone()),
            service_url: env_trim("TEAMS_SERVICE_URL").unwrap_or_else(|| self.service_url.clone()),
            allowed_users: env_list("TEAMS_ALLOWED_USERS")
                .unwrap_or_else(|| self.allowed_users.clone()),
            home_channel: env_trim("TEAMS_HOME_CHANNEL")
                .unwrap_or_else(|| self.home_channel.clone()),
        }
    }
}

/// hermes `_validate_teams_service_url` — allowlisted hosts only.
pub fn validate_teams_service_url(raw: &str) -> Option<String> {
    let parsed = url::Url::parse(raw).ok()?;
    let host = parsed.host_str()?;
    if !ALLOWED_TEAMS_SERVICE_HOSTS.iter().any(|h| *h == host) {
        return None;
    }
    let mut normalized = raw.trim_end_matches('/').to_string();
    normalized.push('/');
    Some(normalized)
}

/// hermes `_TEAMS_CONV_ID_RE` — conservative Bot Framework id charset.
pub fn valid_conversation_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '@' | '-' | '_' | '.'))
}

/// Strip `<at>BotName</at>` mention HTML that Teams prepends.
pub fn strip_at_mentions(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("<at>") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 4..];
        match after.find("</at>") {
            Some(end) => {
                rest = &after[end + 5..];
                // Drop the whitespace that follows the mention tag.
                rest = rest.trim_start_matches(|c: char| c.is_whitespace());
            }
            None => {
                out.push_str("<at>");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out.trim().to_string()
}

struct Runtime {
    cfg: ResolvedTeams,
    client: reqwest::Client,
    /// Cached Bot Framework bearer token + expiry.
    token: Mutex<Option<(String, Instant)>>,
    /// conversation id -> serviceUrl captured from inbound activities
    /// (proactive sends use the activity's own serviceUrl).
    service_urls: Mutex<HashMap<String, String>>,
    /// Activity-id dedup (hermes MessageDeduplicator).
    dedup: Mutex<HashMap<String, u64>>,
}

impl Runtime {
    async fn is_duplicate(&self, activity_id: &str) -> bool {
        if activity_id.is_empty() {
            return false;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut dedup = self.dedup.lock().await;
        dedup.retain(|_, ts| now.saturating_sub(*ts) < DEDUP_WINDOW_SECS);
        if dedup.contains_key(activity_id) {
            return true;
        }
        if dedup.len() >= DEDUP_MAX_SIZE {
            let mut entries: Vec<(String, u64)> = dedup.drain().collect();
            entries.sort_by_key(|(_, ts)| *ts);
            entries.truncate(DEDUP_MAX_SIZE / 2);
            *dedup = entries.into_iter().collect();
        }
        dedup.insert(activity_id.to_string(), now);
        false
    }

    /// OAuth2 client-credentials token with cache (hermes
    /// `CachedTokenProvider`).
    async fn access_token(&self) -> Result<String, String> {
        {
            let cached = self.token.lock().await;
            if let Some((token, expiry)) = cached.as_ref() {
                if Instant::now() < *expiry {
                    return Ok(token.clone());
                }
            }
        }
        let url = format!(
            "https://login.microsoftonline.com/{}/oauth2/v2.0/token",
            self.cfg.tenant_id
        );
        let resp = self
            .client
            .post(&url)
            .form(&[
                ("grant_type", "client_credentials"),
                ("client_id", self.cfg.client_id.as_str()),
                ("client_secret", self.cfg.client_secret.as_str()),
                ("scope", "https://api.botframework.com/.default"),
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
            .ok_or_else(|| "token response missing access_token".to_string())?
            .to_string();
        let expires_in = payload
            .get("expires_in")
            .and_then(|v| v.as_u64())
            .unwrap_or(3600);
        *self.token.lock().await = Some((
            token.clone(),
            Instant::now() + Duration::from_secs(expires_in.saturating_sub(60)),
        ));
        Ok(token)
    }

    /// POST a message activity to a conversation (hermes send /
    /// standalone send).
    async fn send_activity(&self, conversation_id: &str, text: &str) -> Result<(), String> {
        let activity = json!({ "type": "message", "text": text, "textFormat": "markdown" });
        self.send_activity_payload(conversation_id, activity).await
    }

    /// POST an arbitrary activity payload (message / card / typing) to
    /// the conversation (hermes `_send_card` rides the same endpoint).
    async fn send_activity_payload(
        &self,
        conversation_id: &str,
        activity: Value,
    ) -> Result<(), String> {
        if !valid_conversation_id(conversation_id) {
            return Err("conversation id outside Bot Framework charset".into());
        }
        let service_url = {
            let urls = self.service_urls.lock().await;
            urls.get(conversation_id).cloned()
        }
        .map(|u| validate_teams_service_url(&u))
        .unwrap_or_else(|| validate_teams_service_url(&self.cfg.service_url))
        .ok_or_else(|| "service URL not on Bot Framework allowlist".to_string())?;
        let token = self.access_token().await?;
        let url = format!("{service_url}v3/conversations/{conversation_id}/activities");
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&token)
            .json(&activity)
            .send()
            .await
            .map_err(|e| format!("activity post: {e}"))?;
        if resp.status().as_u16() >= 400 {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!(
                "activity post failed ({status}): {}",
                &body[..body.len().min(300)]
            ));
        }
        Ok(())
    }

    /// Download an attachment with the bot bearer (hermes
    /// `_fetch_attachment_bytes`).
    async fn fetch_attachment(&self, url: &str, needs_auth: bool) -> Result<Vec<u8>, String> {
        let mut req = self.client.get(url);
        if needs_auth {
            let token = self.access_token().await?;
            req = req.bearer_auth(token);
        }
        let resp = req.send().await.map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("attachment HTTP {}", resp.status()));
        }
        resp.bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| e.to_string())
    }
}

static RUNTIME: std::sync::OnceLock<Arc<Runtime>> = std::sync::OnceLock::new();

/// Register the adapter (called from `run_messaging` when enabled).
pub fn register(cfg: &TeamsConfig) {
    let resolved = cfg.resolve();
    if resolved.client_id.is_empty()
        || resolved.client_secret.is_empty()
        || resolved.tenant_id.is_empty()
    {
        eprintln!(
            "[teams] enabled but TEAMS_CLIENT_ID/TEAMS_CLIENT_SECRET/TEAMS_TENANT_ID are not all set — webhook route will 503"
        );
    }
    let service_url = if resolved.service_url.is_empty() {
        DEFAULT_TEAMS_SERVICE_URL.to_string()
    } else {
        resolved.service_url
    };
    let runtime = Arc::new(Runtime {
        client: reqwest::Client::builder()
            .timeout(API_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new()),
        cfg: ResolvedTeams {
            service_url,
            ..resolved
        },
        token: Mutex::new(None),
        service_urls: Mutex::new(HashMap::new()),
        dedup: Mutex::new(HashMap::new()),
    });
    let _ = RUNTIME.set(runtime.clone());
    crate::messaging::register_platform_sender(
        "teams",
        Arc::new(TeamsSender {
            runtime: runtime.clone(),
        }),
    );
}

fn runtime() -> Option<Arc<Runtime>> {
    RUNTIME.get().cloned()
}

/// Webhook response handed back to the gateway route.
pub struct TeamsWebhookResponse {
    pub status: u16,
    pub body: Value,
}

/// Gateway webhook entry point (hermes `_on_message` + SDK route),
/// mounted at `/webhooks/teams`.
pub async fn teams_handle_webhook(
    dispatcher: &Arc<Dispatcher>,
    pairing: Option<&crate::pairing::PairingStore>,
    body: &[u8],
) -> TeamsWebhookResponse {
    let Some(runtime) = runtime() else {
        return TeamsWebhookResponse {
            status: 503,
            body: json!({ "error": "teams adapter not registered" }),
        };
    };
    let activity: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => {
            return TeamsWebhookResponse {
                status: 400,
                body: json!({ "error": "invalid activity JSON" }),
            }
        }
    };
    let activity_type = activity.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if activity_type == "invoke" {
        // adaptiveCard/action button taps resolve blocking approvals
        // (hermes `_on_card_action`); other invokes are acked.
        let name = activity.get("name").and_then(|v| v.as_str()).unwrap_or("");
        if name == "adaptiveCard/action" {
            return TeamsWebhookResponse {
                status: 200,
                body: card_action_response(&runtime.cfg, &activity),
            };
        }
        return TeamsWebhookResponse {
            status: 200,
            body: json!({}),
        };
    }
    if activity_type != "message" {
        // conversationUpdate etc. is acked without dispatch.
        return TeamsWebhookResponse {
            status: 200,
            body: json!({}),
        };
    }
    let activity_id = activity
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if runtime.is_duplicate(&activity_id).await {
        return TeamsWebhookResponse {
            status: 200,
            body: json!({}),
        };
    }
    let conversation_id = activity
        .pointer("/conversation/id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if conversation_id.is_empty() {
        return TeamsWebhookResponse {
            status: 200,
            body: json!({}),
        };
    }
    // Cache the activity's serviceUrl for proactive sends.
    if let Some(service_url) = activity.get("serviceUrl").and_then(|v| v.as_str()) {
        runtime
            .service_urls
            .lock()
            .await
            .insert(conversation_id.clone(), service_url.to_string());
    }
    let mut text = activity
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if text.contains("<at>") {
        text = strip_at_mentions(&text);
    }
    let user_id = activity
        .pointer("/from/aadObjectId")
        .and_then(|v| v.as_str())
        .or_else(|| activity.pointer("/from/id").and_then(|v| v.as_str()))
        .unwrap_or("")
        .to_string();
    let user_name = activity
        .pointer("/from/name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    // Allowlist ∪ pairing gate.
    if !runtime
        .cfg
        .allowed_users
        .iter()
        .any(|u| u == &user_id || u == "*")
    {
        if let Some(store) = pairing {
            if !store.is_approved("teams", &user_id) {
                if let Some(code_msg) =
                    crate::messaging::pairing_offer_public(store, "teams", &user_id, &user_name)
                {
                    let _ = runtime.send_activity(&conversation_id, &code_msg).await;
                }
                return TeamsWebhookResponse {
                    status: 200,
                    body: json!({}),
                };
            }
        } else {
            eprintln!("[teams] unauthorized sender {user_id} — add to allowed_users");
            return TeamsWebhookResponse {
                status: 200,
                body: json!({}),
            };
        }
    }

    // Attachments (hermes rules: skip html mirrors + cards).
    let mut attachments = Vec::new();
    if let Some(atts) = activity.get("attachments").and_then(|v| v.as_array()) {
        for att in atts {
            let content_type = att
                .get("contentType")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_lowercase();
            let content_url = att
                .get("contentUrl")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let att_name = att
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if matches!(content_type.as_str(), "text/html" | "text/plain") && content_url.is_empty()
            {
                continue;
            }
            if content_type.starts_with("application/vnd.microsoft.card") {
                continue;
            }
            if content_type == "application/vnd.microsoft.teams.file.download.info" {
                let download_url = att
                    .pointer("/content/downloadUrl")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if download_url.is_empty() {
                    continue;
                }
                // Pre-authed SharePoint URL — no bot bearer needed.
                match runtime.fetch_attachment(&download_url, false).await {
                    Ok(data) => cache_into(&mut attachments, &data, &att_name, ""),
                    Err(e) => eprintln!("[teams] file attachment failed: {e}"),
                }
                continue;
            }
            if content_url.is_empty() {
                continue;
            }
            let needs_auth = !content_url.starts_with("https://") || content_url.contains("smba.");
            match runtime.fetch_attachment(&content_url, needs_auth).await {
                Ok(data) => cache_into(&mut attachments, &data, &att_name, &content_type),
                Err(e) => eprintln!("[teams] attachment download failed: {e}"),
            }
        }
    }
    if text.trim().is_empty() && attachments.is_empty() {
        return TeamsWebhookResponse {
            status: 200,
            body: json!({}),
        };
    }
    let event = MessageEvent {
        platform: "teams".into(),
        chat_id: conversation_id.clone(),
        sender_id: user_id,
        sender_name: user_name,
        text,
        message_id: activity_id,
        attachments,
    };
    let mut gate_check = event.clone();
    if !crate::messaging::pre_gateway_dispatch_gate_public(&mut gate_check).await {
        return TeamsWebhookResponse {
            status: 200,
            body: json!({}),
        };
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
    let (reply_text, _media) = crate::messaging::extract_media_tags(&full);
    let reply_text = reply_text.trim().to_string();
    if !reply_text.is_empty() {
        // P705: ledger-protected reply delivery (all chunks).
        dispatcher
            .try_send_with_ledger("teams", &conversation_id, &reply_text, || async {
                let mut result: Result<(), String> = Ok(());
                for chunk in crate::messaging::chunk_text(&reply_text, MAX_MESSAGE_LENGTH) {
                    if let Err(e) = runtime.send_activity(&conversation_id, &chunk).await {
                        eprintln!("[teams] reply failed: {e}");
                        if result.is_ok() {
                            result = Err(e.to_string());
                        }
                    }
                }
                result
            })
            .await;
    }
    TeamsWebhookResponse {
        status: 200,
        body: json!({}),
    }
}

fn cache_into(attachments: &mut Vec<MediaAttachment>, data: &[u8], name: &str, mime: &str) {
    let mime = if mime.is_empty() {
        "application/octet-stream"
    } else {
        mime
    };
    match crate::media_cache::cache_media_bytes(&crate::config::ulnclaw_home(), data, mime, name) {
        Ok(path) => attachments.push(MediaAttachment {
            path,
            mime: mime.to_string(),
            bytes: data.len() as u64,
            original_name: name.to_string(),
        }),
        Err(e) => eprintln!("[teams] media cache failed: {e}"),
    }
}

/** Char-boundary truncation for card previews (hermes `command[:2000]`
body / `command[:200]` button-data shapes). */
fn truncate_for_card(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let cut: String = text.chars().take(max_chars).collect();
    format!("{cut}...")
}

/** AdaptiveCard v1.4 exec-approval prompt (hermes `send_exec_approval`):
Allow Once / Allow Session / Always Allow / Deny Action.Execute buttons
carrying the session key + truncated command in their data payload. */
pub fn build_approval_card(
    command: &str,
    description: &str,
    session_key: &str,
    allow_permanent: bool,
    allow_session: bool,
    smart_denied: bool,
) -> Value {
    let cmd_preview = truncate_for_card(command, 2000);
    let btn_data_base = json!({
        "session_key": session_key,
        "cmd": truncate_for_card(command, 200),
        "desc": description,
    });
    let action = |title: &str, verb_action: &str, style: Option<&str>| {
        let mut data = btn_data_base.clone();
        data["hermes_action"] = json!(verb_action);
        let mut action = json!({
            "type": "Action.Execute",
            "title": title,
            "verb": "hermes_approve",
            "data": data,
        });
        if let Some(style) = style {
            action["style"] = json!(style);
        }
        action
    };
    let mut actions = vec![action("Allow Once", "approve_once", Some("positive"))];
    if !smart_denied && allow_session {
        actions.push(action("Allow Session", "approve_session", None));
        if allow_permanent {
            actions.push(action("Always Allow", "approve_always", None));
        }
    }
    actions.push(action("Deny", "deny", Some("destructive")));
    let mut body = vec![
        json!({"type": "TextBlock", "text": "⚠️ Command Approval Required", "wrap": true, "weight": "Bolder"}),
        json!({"type": "TextBlock", "text": format!("```\n{cmd_preview}\n```"), "wrap": true}),
        json!({"type": "TextBlock", "text": format!("Reason: {description}"), "wrap": true, "isSubtle": true}),
    ];
    if smart_denied {
        body.push(json!({
            "type": "TextBlock",
            "text": "Smart DENY: owner override applies to this one operation only.",
            "wrap": true
        }));
    }
    json!({
        "type": "AdaptiveCard",
        "$schema": "http://adaptivecards.io/schemas/adaptive-card.json",
        "version": "1.4",
        "body": body,
        "actions": actions,
    })
}

/** Invoke response carrying a text message (hermes
`AdaptiveCardActionMessageResponse`). */
fn invoke_message_response(text: &str) -> Value {
    json!({
        "statusCode": 200,
        "type": "application/vnd.microsoft.activity.message",
        "value": text,
    })
}

/** Invoke response replacing the card (hermes
`AdaptiveCardActionCardResponse`). */
fn invoke_card_response(card: Value) -> Value {
    json!({
        "statusCode": 200,
        "type": "application/vnd.microsoft.card.adaptive",
        "value": card,
    })
}

/** Handle an `adaptiveCard/action` invoke — port of hermes
`_on_card_action`: default-deny allowlist gate, choice mapping,
blocking-approval check, resolution, replacement card. */
pub fn card_action_response(cfg: &ResolvedTeams, activity: &Value) -> Value {
    let data = activity
        .pointer("/value/action/data")
        .cloned()
        .unwrap_or(json!({}));
    let hermes_action = data
        .get("hermes_action")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let session_key = data
        .get("session_key")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if hermes_action.is_empty() || session_key.is_empty() {
        return invoke_message_response("Unknown action.");
    }
    // Default-deny: an empty allowlist means nobody may click approval
    // buttons (hermes: TEAMS_ALLOWED_USERS not configured → reject).
    if cfg.allowed_users.is_empty() {
        eprintln!("[teams] card action rejected: allowed_users not configured — default deny");
        return invoke_message_response(
            "⛔ Approval buttons require TEAMS_ALLOWED_USERS to be configured.",
        );
    }
    let clicker_id = activity
        .pointer("/from/aadObjectId")
        .and_then(|v| v.as_str())
        .or_else(|| activity.pointer("/from/id").and_then(|v| v.as_str()))
        .unwrap_or("");
    if !cfg
        .allowed_users
        .iter()
        .any(|u| u == "*" || u == clicker_id)
    {
        eprintln!("[teams] unauthorized card action by {clicker_id} — ignoring");
        return invoke_message_response("⛔ Not authorized.");
    }
    let choice = match hermes_action {
        "approve_once" => crate::approval_gateway::CHOICE_ONCE,
        "approve_session" => crate::approval_gateway::CHOICE_SESSION,
        "approve_always" => crate::approval_gateway::CHOICE_ALWAYS,
        "deny" => crate::approval_gateway::CHOICE_DENY,
        _ => return invoke_message_response("Unknown action."),
    };
    if !crate::approval_gateway::has_blocking(session_key) {
        return invoke_card_response(json!({
            "type": "AdaptiveCard",
            "$schema": "http://adaptivecards.io/schemas/adaptive-card.json",
            "version": "1.4",
            "body": [{
                "type": "TextBlock",
                "text": "⚠️ Approval already resolved or expired.",
                "wrap": true
            }],
        }));
    }
    crate::approval_gateway::resolve(session_key, choice);
    let label = match choice {
        crate::approval_gateway::CHOICE_ONCE => "✅ Allowed (once)",
        crate::approval_gateway::CHOICE_SESSION => "✅ Allowed (session)",
        crate::approval_gateway::CHOICE_ALWAYS => "✅ Always allowed",
        _ => "❌ Denied",
    };
    let cmd = data.get("cmd").and_then(|v| v.as_str()).unwrap_or("");
    let desc = data.get("desc").and_then(|v| v.as_str()).unwrap_or("");
    let mut body: Vec<Value> = Vec::new();
    if !cmd.is_empty() {
        body.push(json!({"type": "TextBlock", "text": "⚠️ Command Approval Required", "wrap": true, "weight": "Bolder"}));
        body.push(json!({"type": "TextBlock", "text": format!("```\n{cmd}\n```"), "wrap": true}));
    }
    if !desc.is_empty() {
        body.push(json!({"type": "TextBlock", "text": format!("Reason: {desc}"), "wrap": true, "isSubtle": true}));
    }
    body.push(json!({"type": "TextBlock", "text": label, "wrap": true, "weight": "Bolder"}));
    invoke_card_response(json!({
        "type": "AdaptiveCard",
        "$schema": "http://adaptivecards.io/schemas/adaptive-card.json",
        "version": "1.4",
        "body": body,
    }))
}

struct TeamsSender {
    runtime: Arc<Runtime>,
}

#[async_trait::async_trait]
impl crate::messaging::PlatformSender for TeamsSender {
    async fn send_text(&self, chat_id: &str, text: &str) {
        for chunk in crate::messaging::chunk_text(text, MAX_MESSAGE_LENGTH) {
            if let Err(e) = self.runtime.send_activity(chat_id, &chunk).await {
                eprintln!("[teams] send_text to {chat_id} failed: {e}");
            }
        }
    }

    async fn send_exec_approval(
        &self,
        chat_id: &str,
        command: &str,
        session_key: &str,
        description: &str,
        allow_permanent: bool,
        allow_session: bool,
        smart_denied: bool,
    ) -> bool {
        let card = build_approval_card(
            command,
            description,
            session_key,
            allow_permanent,
            allow_session,
            smart_denied,
        );
        let activity = json!({
            "type": "message",
            "attachments": [{
                "contentType": "application/vnd.microsoft.card.adaptive",
                "content": card,
            }],
        });
        match self.runtime.send_activity_payload(chat_id, activity).await {
            Ok(()) => true,
            Err(e) => {
                eprintln!("[teams] send_exec_approval to {chat_id} failed: {e}");
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_url_allowlist() {
        assert!(validate_teams_service_url("https://smba.trafficmanager.net/teams/").is_some());
        assert!(
            validate_teams_service_url("https://smba.infra.gov.teams.microsoft.us/amer/").is_some()
        );
        assert!(validate_teams_service_url("https://evil.example.com/teams/").is_none());
        assert!(validate_teams_service_url("not a url").is_none());
    }

    #[test]
    fn conversation_id_charset() {
        assert!(valid_conversation_id("19:meeting_abc123@thread.v2"));
        assert!(valid_conversation_id("a:1b-2_c.3"));
        assert!(!valid_conversation_id(""));
        assert!(!valid_conversation_id("../etc/passwd"));
        assert!(!valid_conversation_id("id with spaces"));
    }

    #[test]
    fn at_mention_stripping() {
        assert_eq!(
            strip_at_mentions("<at>HermesBot</at> hello there"),
            "hello there"
        );
        assert_eq!(
            strip_at_mentions("hi <at>Bot</at> middle <at>Bot</at> end"),
            "hi middle end"
        );
        assert_eq!(strip_at_mentions("no mentions"), "no mentions");
        // Unbalanced tag stays literal.
        assert_eq!(strip_at_mentions("<at>broken"), "<at>broken");
    }

    #[tokio::test]
    async fn dedup_window() {
        let cfg = TeamsConfig::default();
        let runtime = Runtime {
            client: reqwest::Client::new(),
            cfg: cfg.resolve(),
            token: Mutex::new(None),
            service_urls: Mutex::new(HashMap::new()),
            dedup: Mutex::new(HashMap::new()),
        };
        assert!(!runtime.is_duplicate("act1").await);
        assert!(runtime.is_duplicate("act1").await);
        assert!(!runtime.is_duplicate("act2").await);
        assert!(!runtime.is_duplicate("").await); // empty ids never dedup
    }

    #[test]
    fn resolve_env_precedence() {
        let _guard = crate::models_dev::test_env_lock();
        std::env::set_var("TEAMS_CLIENT_ID", "env-id");
        std::env::set_var("TEAMS_ALLOWED_USERS", "u1,u2");
        let cfg = TeamsConfig {
            client_id: "cfg-id".into(),
            ..Default::default()
        };
        let resolved = cfg.resolve();
        assert_eq!(resolved.client_id, "env-id");
        assert_eq!(
            resolved.allowed_users,
            vec!["u1".to_string(), "u2".to_string()]
        );
        std::env::remove_var("TEAMS_CLIENT_ID");
        std::env::remove_var("TEAMS_ALLOWED_USERS");
    }

    #[test]
    fn conversation_type_mapping() {
        // hermes maps conversationType personal/groupChat/channel.
        for (ct, expected) in [
            ("personal", "dm"),
            ("groupChat", "group"),
            ("channel", "channel"),
            ("", "dm"),
        ] {
            let chat_type = match ct {
                "personal" => "dm",
                "groupChat" => "group",
                "channel" => "channel",
                _ => "dm",
            };
            assert_eq!(chat_type, expected);
        }
    }

    #[test]
    fn attachment_skip_rules() {
        // text/html mirrors without contentUrl and card payloads skip.
        let html_mirror = json!({"contentType": "text/html"});
        let card = json!({"contentType": "application/vnd.microsoft.card.adaptive"});
        let ct = |v: &Value| {
            v.get("contentType")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_lowercase()
        };
        let skip_html = matches!(ct(&html_mirror).as_str(), "text/html" | "text/plain")
            && html_mirror.get("contentUrl").is_none();
        assert!(skip_html);
        assert!(ct(&card).starts_with("application/vnd.microsoft.card"));
    }

    fn resolved_with_users(users: &[&str]) -> ResolvedTeams {
        ResolvedTeams {
            client_id: String::new(),
            client_secret: String::new(),
            tenant_id: String::new(),
            service_url: DEFAULT_TEAMS_SERVICE_URL.to_string(),
            allowed_users: users.iter().map(|s| s.to_string()).collect(),
            home_channel: String::new(),
        }
    }

    fn card_activity(action: &str, session_key: &str, clicker: &str) -> Value {
        json!({
            "type": "invoke",
            "name": "adaptiveCard/action",
            "from": {"id": clicker, "aadObjectId": clicker},
            "value": {"action": {"verb": "hermes_approve", "data": {
                "hermes_action": action,
                "session_key": session_key,
                "cmd": "rm -rf /tmp/x",
                "desc": "dangerous command"
            }}}
        })
    }

    #[test]
    fn approval_card_shape() {
        let card = build_approval_card(
            "rm -rf /tmp/x",
            "dangerous command",
            "platform-teams-c1",
            true,
            true,
            false,
        );
        assert_eq!(card["version"], "1.4");
        assert_eq!(card["body"].as_array().unwrap().len(), 3);
        let actions = card["actions"].as_array().unwrap();
        assert_eq!(actions.len(), 4);
        assert_eq!(actions[0]["title"], "Allow Once");
        assert_eq!(actions[0]["style"], "positive");
        assert_eq!(actions[3]["title"], "Deny");
        assert_eq!(actions[3]["style"], "destructive");
        assert_eq!(actions[0]["data"]["session_key"], "platform-teams-c1");
        assert_eq!(actions[0]["data"]["hermes_action"], "approve_once");
        assert_eq!(actions[1]["data"]["hermes_action"], "approve_session");
        assert_eq!(actions[2]["data"]["hermes_action"], "approve_always");
        // smart_denied collapses to Allow Once + Deny only.
        let collapsed = build_approval_card("c", "d", "s", true, true, true);
        assert_eq!(collapsed["actions"].as_array().unwrap().len(), 2);
        assert_eq!(collapsed["body"].as_array().unwrap().len(), 4);
    }

    #[test]
    fn card_action_auth_gates() {
        // Empty allowlist: default deny.
        let resp = card_action_response(
            &resolved_with_users(&[]),
            &card_activity("approve_once", "teams-auth-1", "user-1"),
        );
        assert!(resp["value"]
            .as_str()
            .unwrap()
            .contains("TEAMS_ALLOWED_USERS"));
        // Unknown clicker.
        let resp = card_action_response(
            &resolved_with_users(&["someone-else"]),
            &card_activity("approve_once", "teams-auth-2", "user-1"),
        );
        assert_eq!(resp["value"], "⛔ Not authorized.");
        // Unknown verb.
        let resp = card_action_response(
            &resolved_with_users(&["user-1"]),
            &card_activity("bogus", "teams-auth-3", "user-1"),
        );
        assert_eq!(resp["value"], "Unknown action.");
        // Missing session_key.
        let no_key = json!({
            "from": {"id": "user-1"},
            "value": {"action": {"data": {"hermes_action": "approve_once"}}}
        });
        let resp = card_action_response(&resolved_with_users(&["user-1"]), &no_key);
        assert_eq!(resp["value"], "Unknown action.");
    }

    #[test]
    fn card_action_resolves_pending_approval() {
        let session = "teams-card-resolve";
        let mut handle = crate::approval_gateway::register(
            session,
            "rm -rf /tmp/x",
            "dangerous command",
            false,
            true,
            true,
        );
        let resp = card_action_response(
            &resolved_with_users(&["*"]),
            &card_activity("approve_session", session, "user-9"),
        );
        assert_eq!(handle.rx.try_recv().unwrap(), "session");
        assert_eq!(resp["type"], "application/vnd.microsoft.card.adaptive");
        let texts: Vec<&str> = resp["value"]["body"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["text"].as_str().unwrap())
            .collect();
        assert!(texts.iter().any(|t| t.contains("Allowed (session)")));
        // Second tap: already resolved → expiry card.
        let resp = card_action_response(
            &resolved_with_users(&["*"]),
            &card_activity("approve_session", session, "user-9"),
        );
        let texts: Vec<&str> = resp["value"]["body"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["text"].as_str().unwrap())
            .collect();
        assert!(texts.iter().any(|t| t.contains("already resolved")));
    }
}

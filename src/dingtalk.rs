//! DingTalk platform adapter — port of hermes `plugins/platforms/dingtalk`
//! @ v2026.8.3 (adapter.py).
//!
//! Uses DingTalk **Stream Mode** without the Python SDK: the gateway
//! connection is opened via `POST /v1.0/gateway/connections/open`
//! (client id + secret + a CALLBACK subscription on
//! `/v1.0/im/bot/messages/get`), then a WebSocket to the returned
//! endpoint carries JSON frames that are ACKed hermes-style. Replies go
//! out through the per-chat `sessionWebhook` as markdown.
//!
//! Intake parity: `allowed_users` (staff_id/sender_id, `*` wildcard),
//! hard `allowed_chats` group whitelist, `require_mention` (hermes
//! default **false** for DingTalk) with `isInAtList` detection, regex
//! `mention_patterns` wake words, and `free_response_chats`. Inbound
//! media arrives as `downloadCode` references resolved through
//! `/v1.0/robot/messageFiles/download` with an OAuth2 access token and
//! cached into the media cache.
//!
//! Emoji reactions are ported (hermes `_send_emotion` /
//! `_fire_done_reaction`): a 🤔Thinking text-emoji lands on the inbound
//! message fire-and-forget via `POST /v1.0/robot/emotion/reply`, then
//! swaps to 🥳Done (`/recall` + `/reply`) once after the final markdown
//! reply — idempotent per chat. Known differences: AI streaming cards
//! (`card_1_0` SDK) and message editing are not ported — replies are
//! plain webhook markdown.

use crate::messaging::{Dispatcher, MediaAttachment, MessageEvent};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

const API_BASE: &str = "https://api.dingtalk.com";
const BOT_TOPIC: &str = "/v1.0/im/bot/messages/get";
/// hermes `MAX_MESSAGE_LENGTH` for DingTalk markdown.
const MAX_MESSAGE_LENGTH: usize = 20_000;
const RECONNECT_BACKOFF: &[u64] = &[2, 5, 10, 30, 60];
const SESSION_WEBHOOKS_MAX: usize = 500;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const WEBHOOK_SEND_TIMEOUT: Duration = Duration::from_secs(15);
const DEDUP_WINDOW_SECS: u64 = 300;
const DEDUP_MAX_SIZE: usize = 1000;
/// Access tokens are valid ~7200 s; refresh with a safety margin.
const TOKEN_REFRESH_SECS: u64 = 6600;
/// hermes `_send_emotion` text-emoji metadata (`emotion_type` 2 = text
/// emoji).
const EMOTION_ID: &str = "2659900";
const EMOTION_BACKGROUND_ID: &str = "im_bg_1";
/// Lifecycle emojis (hermes stream handler / `_fire_done_reaction`).
const THINKING_EMOJI: &str = "🤔Thinking";
const DONE_EMOJI: &str = "🥳Done";

/// `[messaging.dingtalk]` — DingTalk Stream Mode adapter (hermes
/// `platforms.dingtalk` plugin config + `DINGTALK_*` env vars).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DingTalkConfig {
    pub enabled: bool,
    /// App key (fallback `DINGTALK_CLIENT_ID`).
    pub client_id: String,
    /// App secret (fallback `DINGTALK_CLIENT_SECRET`).
    pub client_secret: String,
    /// Require @mention / wake word in group chats. Hermes defaults this
    /// to FALSE for DingTalk.
    pub require_mention: bool,
    /// Conversation ids that skip the mention gate.
    pub free_response_chats: Vec<String>,
    /// When non-empty, group messages outside this set are ignored.
    pub allowed_chats: Vec<String>,
    /// staff_id/sender_id allowlist (`*` = anyone; empty = anyone —
    /// hermes `_is_user_allowed`).
    pub allowed_users: Vec<String>,
    /// Regex wake words that trigger the bot in groups.
    pub mention_patterns: Vec<String>,
    /// AI Card template id (`card_template_id`, fallback
    /// `DINGTALK_CARD_TEMPLATE_ID`). When set, replies ride a streaming
    /// AI Card (hermes `_create_and_stream_card`) instead of the session
    /// webhook markdown.
    pub card_template_id: String,
    /// Robot code for card delivery (fallback `DINGTALK_ROBOT_CODE`,
    /// then `client_id` — hermes `robot_code or client_id`).
    pub robot_code: String,
}

impl Default for DingTalkConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            client_id: String::new(),
            client_secret: String::new(),
            require_mention: false,
            free_response_chats: Vec::new(),
            allowed_chats: Vec::new(),
            allowed_users: Vec::new(),
            mention_patterns: Vec::new(),
            card_template_id: String::new(),
            robot_code: String::new(),
        }
    }
}

fn env_trim(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn env_bool(name: &str, default: bool) -> bool {
    match env_trim(name) {
        Some(v) => matches!(v.to_lowercase().as_str(), "true" | "1" | "yes" | "on"),
        None => default,
    }
}

fn env_csv(name: &str) -> Option<Vec<String>> {
    env_trim(name).map(|raw| {
        raw.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    })
}

/// DingTalk API base — normally `https://api.dingtalk.com`; the
/// `DINGTALK_API_BASE` override exists for tests and corporate proxies
/// (mirrors the slack/telegram/discord pattern).
fn dingtalk_api_base() -> String {
    env_trim("DINGTALK_API_BASE").unwrap_or_else(|| API_BASE.to_string())
}

#[derive(Debug, Clone)]
pub struct ResolvedDingTalk {
    pub client_id: String,
    pub client_secret: String,
    pub require_mention: bool,
    pub free_response_chats: Vec<String>,
    pub allowed_chats: Vec<String>,
    pub allowed_users: Vec<String>,
    pub mention_patterns: Vec<String>,
    pub card_template_id: String,
    pub robot_code: String,
}

impl DingTalkConfig {
    pub fn resolve(&self) -> ResolvedDingTalk {
        let mention_patterns = match env_trim("DINGTALK_MENTION_PATTERNS") {
            Some(raw) => {
                // hermes: JSON array first, then newline/comma split.
                if let Ok(list) = serde_json::from_str::<Vec<String>>(&raw) {
                    list
                } else {
                    raw.split(|c| c == '\n' || c == ',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect()
                }
            }
            None => self.mention_patterns.clone(),
        };
        ResolvedDingTalk {
            client_id: env_trim("DINGTALK_CLIENT_ID")
                .unwrap_or_else(|| self.client_id.trim().to_string()),
            client_secret: env_trim("DINGTALK_CLIENT_SECRET")
                .unwrap_or_else(|| self.client_secret.trim().to_string()),
            require_mention: env_bool("DINGTALK_REQUIRE_MENTION", self.require_mention),
            free_response_chats: env_csv("DINGTALK_FREE_RESPONSE_CHATS")
                .unwrap_or_else(|| self.free_response_chats.clone()),
            allowed_chats: env_csv("DINGTALK_ALLOWED_CHATS")
                .unwrap_or_else(|| self.allowed_chats.clone()),
            allowed_users: env_csv("DINGTALK_ALLOWED_USERS")
                .unwrap_or_else(|| self.allowed_users.clone()),
            mention_patterns,
            card_template_id: env_trim("DINGTALK_CARD_TEMPLATE_ID")
                .unwrap_or_else(|| self.card_template_id.trim().to_string()),
            robot_code: env_trim("DINGTALK_ROBOT_CODE")
                .unwrap_or_else(|| self.robot_code.trim().to_string()),
        }
    }
}

struct Runtime {
    cfg: ResolvedDingTalk,
    client: reqwest::Client,
    /// chat_id → (session_webhook, expired_ms).
    session_webhooks: Mutex<HashMap<String, (String, u64)>>,
    dedup: Mutex<HashMap<String, u64>>,
    access_token: Mutex<(String, std::time::Instant)>,
    mention_regexes: Vec<regex::Regex>,
    /// chat_id → (open_msg_id, open_conversation_id); hermes
    /// `_message_contexts`.
    message_contexts: Mutex<HashMap<String, (String, String)>>,
    /// Chats whose 🤔Thinking → 🥳Done swap already fired; hermes
    /// `_done_emoji_fired`.
    done_emoji_fired: Mutex<HashSet<String>>,
}

impl Runtime {
    fn is_user_allowed(&self, sender_id: &str, sender_staff_id: &str) -> bool {
        if self.cfg.allowed_users.is_empty() || self.cfg.allowed_users.iter().any(|u| u == "*") {
            return true;
        }
        let lower: Vec<String> = self
            .cfg
            .allowed_users
            .iter()
            .map(|u| u.to_lowercase())
            .collect();
        [sender_id, sender_staff_id]
            .iter()
            .any(|id| !id.is_empty() && lower.contains(&id.to_lowercase()))
    }

    fn matches_mention_patterns(&self, text: &str) -> bool {
        self.mention_regexes.iter().any(|re| re.is_match(text))
    }

    /// hermes `_should_process_message` group gate.
    fn should_process(&self, text: &str, is_group: bool, chat_id: &str, in_at_list: bool) -> bool {
        if !is_group {
            return true;
        }
        if !self.cfg.allowed_chats.is_empty()
            && !self.cfg.allowed_chats.contains(&chat_id.to_string())
        {
            return false;
        }
        if self.cfg.free_response_chats.contains(&chat_id.to_string()) {
            return true;
        }
        if !self.cfg.require_mention {
            return true;
        }
        in_at_list || self.matches_mention_patterns(text)
    }

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

    /// Cache the inbound session webhook (hermes `_session_webhooks`,
    /// LRU-ish cap + 5-minute expiry margin).
    async fn store_webhook(&self, chat_id: &str, webhook: &str, expired_ms: u64) {
        if !is_dingtalk_webhook_url(webhook) {
            return;
        }
        let mut map = self.session_webhooks.lock().await;
        if map.len() >= SESSION_WEBHOOKS_MAX {
            let oldest = map.keys().next().cloned();
            if let Some(key) = oldest {
                map.remove(&key);
            }
        }
        map.insert(chat_id.to_string(), (webhook.to_string(), expired_ms));
    }

    fn valid_webhook<'a>(
        map: &'a HashMap<String, (String, u64)>,
        chat_id: &str,
    ) -> Option<&'a String> {
        let (webhook, expired_ms) = map.get(chat_id)?;
        if *expired_ms > 0 {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let margin_ms: u64 = 5 * 60 * 1000;
            if now_ms + margin_ms >= *expired_ms {
                return None;
            }
        }
        Some(webhook)
    }

    /// OAuth2 access token for media download (cached).
    async fn get_access_token(&self) -> Result<String, String> {
        let (token, fetched_at) = self.access_token.lock().await.clone();
        if !token.is_empty() && fetched_at.elapsed() < Duration::from_secs(TOKEN_REFRESH_SECS) {
            return Ok(token);
        }
        let resp = self
            .client
            .post(format!("{}/v1.0/oauth2/accessToken", dingtalk_api_base()))
            .json(&json!({"appKey": self.cfg.client_id, "appSecret": self.cfg.client_secret}))
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|e| format!("accessToken: {e}"))?;
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        if status >= 400 {
            return Err(format!("accessToken → {status}: {body}"));
        }
        let value: Value =
            serde_json::from_str(&body).map_err(|e| format!("accessToken JSON: {e}"))?;
        let token = value
            .get("accessToken")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "accessToken missing in response".to_string())?
            .to_string();
        *self.access_token.lock().await = (token.clone(), std::time::Instant::now());
        Ok(token)
    }

    /// hermes `_send_emotion`: add or recall a text-emoji reaction on a
    /// message. Best-effort — callers log and move on.
    async fn send_emotion(
        &self,
        robot_code: &str,
        open_msg_id: &str,
        open_conversation_id: &str,
        emoji_name: &str,
        recall: bool,
    ) -> Result<(), String> {
        if open_msg_id.is_empty() || open_conversation_id.is_empty() {
            return Err("missing openMsgId/openConversationId".into());
        }
        let token = self.get_access_token().await?;
        let resp = self
            .client
            .post(format!("{API_BASE}{}", emotion_endpoint(recall)))
            .header("x-acs-dingtalk-access-token", &token)
            .json(&emotion_payload(
                robot_code,
                open_msg_id,
                open_conversation_id,
                emoji_name,
            ))
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|e| format!("emotion {}: {e}", if recall { "recall" } else { "reply" }))?;
        let status = resp.status().as_u16();
        if status >= 400 {
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("emotion → {status}: {body}"));
        }
        Ok(())
    }

    /// hermes `_message_contexts[chat_id] = message` +
    /// `_done_emoji_fired.discard(chat_id)` — stash the inbound ids for
    /// the post-reply Done swap and reset the per-chat fired marker.
    async fn record_message_context(&self, chat_id: &str, msg_id: &str, conversation_id: &str) {
        self.done_emoji_fired.lock().await.remove(chat_id);
        self.message_contexts.lock().await.insert(
            chat_id.to_string(),
            (msg_id.to_string(), conversation_id.to_string()),
        );
    }

    /// hermes `_fire_done_reaction` state half: mark the chat fired and
    /// return the reaction target exactly once per inbound message.
    async fn take_done_reaction(&self, chat_id: &str) -> Option<(String, String)> {
        {
            let mut fired = self.done_emoji_fired.lock().await;
            if fired.contains(chat_id) {
                return None;
            }
            fired.insert(chat_id.to_string());
        }
        let (msg_id, conversation_id) = self.message_contexts.lock().await.get(chat_id)?.clone();
        if msg_id.is_empty() || conversation_id.is_empty() {
            return None;
        }
        Some((msg_id, conversation_id))
    }

    /// Resolve a `downloadCode` to a URL and cache the bytes.
    async fn download_media_code(
        &self,
        download_code: &str,
        robot_code: &str,
        filename_hint: &str,
    ) -> Option<MediaAttachment> {
        let token = self.get_access_token().await.ok()?;
        let resp = self
            .client
            .post(format!("{API_BASE}/v1.0/robot/messageFiles/download"))
            .header("x-acs-dingtalk-access-token", &token)
            .json(&json!({"downloadCode": download_code, "robotCode": robot_code}))
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            eprintln!("[dingtalk] media resolve failed: HTTP {}", resp.status());
            return None;
        }
        let value: Value = resp.json().await.ok()?;
        let url = value.get("downloadUrl").and_then(|v| v.as_str())?;
        let file_resp = self
            .client
            .get(url)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .ok()?;
        if !file_resp.status().is_success() {
            return None;
        }
        let bytes = file_resp.bytes().await.ok()?.to_vec();
        let mime = mime_from_filename(filename_hint);
        let path = crate::media_cache::cache_media_bytes(
            &crate::config::ulnclaw_home(),
            &bytes,
            &mime,
            filename_hint,
        )
        .ok()?;
        Some(MediaAttachment {
            path,
            mime,
            bytes: bytes.len() as u64,
            original_name: filename_hint.to_string(),
        })
    }

    /// Send markdown via session webhook.
    async fn send_markdown(&self, chat_id: &str, content: &str) -> Result<(), String> {
        let webhook = {
            let map = self.session_webhooks.lock().await;
            Self::valid_webhook(&map, chat_id).cloned()
        };
        let Some(webhook) = webhook else {
            return Err("no valid session_webhook (reply must follow an incoming message)".into());
        };
        let normalized = normalize_markdown(content);
        for chunk in crate::messaging::chunk_text(&normalized, MAX_MESSAGE_LENGTH) {
            let payload = json!({
                "msgtype": "markdown",
                "markdown": {"title": "Ulnclaw", "text": chunk},
            });
            let resp = self
                .client
                .post(&webhook)
                .json(&payload)
                .timeout(WEBHOOK_SEND_TIMEOUT)
                .send()
                .await
                .map_err(|e| format!("webhook send: {e}"))?;
            if !resp.status().is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(format!("webhook HTTP error: {body}"));
            }
        }
        Ok(())
    }

    /// Robot code for card delivery — configured value wins, falling back
    /// to client_id (hermes `self._robot_code = robot_code or client_id`).
    fn robot_code(&self) -> String {
        if self.cfg.robot_code.is_empty() {
            self.cfg.client_id.clone()
        } else {
            self.cfg.robot_code.clone()
        }
    }

    /// hermes `_create_and_stream_card` step 1 — create the card instance
    /// (STREAM callback type, forwardable group/robot space models).
    async fn create_card(&self, token: &str, out_track_id: &str) -> Result<(), String> {
        let body = json!({
            "cardTemplateId": self.cfg.card_template_id,
            "outTrackId": out_track_id,
            "cardData": {"cardParamMap": {"content": ""}},
            "callbackType": "STREAM",
            "imGroupOpenSpaceModel": {"supportForward": true},
            "imRobotOpenSpaceModel": {"supportForward": true},
        });
        let resp = self
            .client
            .post(format!("{}/v1.0/card/instances", dingtalk_api_base()))
            .header("x-acs-dingtalk-access-token", token)
            .json(&body)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|e| format!("createCard: {e}"))?;
        let status = resp.status().as_u16();
        if status >= 400 {
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("createCard → {status}: {text}"));
        }
        Ok(())
    }

    /// hermes step 2 — deliver into the conversation: group space
    /// `dtv1.card//IM_GROUP.<conversationId>` with the robot code, DM
    /// space `dtv1.card//IM_ROBOT.<senderStaffId>` (hermes skips DM cards
    /// without a staff id).
    async fn deliver_card(
        &self,
        token: &str,
        out_track_id: &str,
        is_group: bool,
        conversation_id: &str,
        sender_staff_id: &str,
    ) -> Result<(), String> {
        let mut body = json!({
            "outTrackId": out_track_id,
            "userIdType": 1,
        });
        if is_group {
            body["openSpaceId"] = json!(format!("dtv1.card//IM_GROUP.{conversation_id}"));
            body["imGroupOpenDeliverModel"] = json!({"robotCode": self.robot_code()});
        } else {
            if sender_staff_id.is_empty() {
                return Err("AI Card skipped: missing sender_staff_id for DM".into());
            }
            body["openSpaceId"] = json!(format!("dtv1.card//IM_ROBOT.{sender_staff_id}"));
            body["imRobotOpenDeliverModel"] = json!({"spaceType": "IM_ROBOT"});
        }
        let resp = self
            .client
            .post(format!(
                "{}/v1.0/card/instances/deliver",
                dingtalk_api_base()
            ))
            .header("x-acs-dingtalk-access-token", token)
            .json(&body)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|e| format!("deliverCard: {e}"))?;
        let status = resp.status().as_u16();
        if status >= 400 {
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("deliverCard → {status}: {text}"));
        }
        Ok(())
    }

    /// hermes `_stream_card_content` — full-content streaming update;
    /// `finalize` closes the streaming indicator (hermes
    /// `REQUIRES_EDIT_FINALIZE`).
    async fn stream_card_content(
        &self,
        token: &str,
        out_track_id: &str,
        content: &str,
        finalize: bool,
    ) -> Result<(), String> {
        let truncated: String = content.chars().take(MAX_MESSAGE_LENGTH).collect();
        let body = json!({
            "outTrackId": out_track_id,
            "guid": uuid::Uuid::new_v4().to_string(),
            "key": "content",
            "content": truncated,
            "isFull": true,
            "isFinalize": finalize,
            "isError": false,
        });
        let resp = self
            .client
            .put(format!("{}/v1.0/card/streaming", dingtalk_api_base()))
            .header("x-acs-dingtalk-access-token", token)
            .json(&body)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|e| format!("streamingUpdate: {e}"))?;
        let status = resp.status().as_u16();
        if status >= 400 {
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("streamingUpdate → {status}: {text}"));
        }
        Ok(())
    }

    /// hermes `_create_and_stream_card`: create → deliver → stream with
    /// `finalize=true` (one-shot; ulnclaw sends complete replies, so the
    /// card never lingers in streaming state and the sibling-cleanup
    /// ledger has nothing to track). Returns the out_track_id.
    async fn send_ai_card(
        &self,
        is_group: bool,
        conversation_id: &str,
        sender_staff_id: &str,
        content: &str,
    ) -> Result<String, String> {
        if self.cfg.card_template_id.is_empty() {
            return Err("no card_template_id configured".into());
        }
        let token = self.get_access_token().await?;
        let out_track_id = format!(
            "ulnclaw_{}",
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        );
        self.create_card(&token, &out_track_id).await?;
        self.deliver_card(
            &token,
            &out_track_id,
            is_group,
            conversation_id,
            sender_staff_id,
        )
        .await?;
        self.stream_card_content(&token, &out_track_id, content, true)
            .await?;
        Ok(out_track_id)
    }
}

/// hermes `_DINGTALK_WEBHOOK_RE`.
pub fn is_dingtalk_webhook_url(url: &str) -> bool {
    url.starts_with("https://api.dingtalk.com/") || url.starts_with("https://oapi.dingtalk.com/")
}

fn mime_from_filename(filename: &str) -> String {
    let mime = crate::media_cache::mime_for_ext(std::path::Path::new(filename));
    if mime == "application/octet-stream" {
        return mime;
    }
    mime
}

/// hermes `_normalize_markdown`: blank line before numbered lists, dedent
/// fenced code blocks.
pub fn normalize_markdown(text: &str) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let numbered = regex::Regex::new(r"^\d+\.\s").unwrap();
    let mut out: Vec<String> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if numbered.is_match(line.trim()) && i > 0 {
            let prev = lines[i - 1];
            if !prev.trim().is_empty() && !numbered.is_match(prev.trim()) {
                out.push(String::new());
            }
        }
        let trimmed = line.trim_start();
        let pushed = if trimmed.starts_with("```") && trimmed.len() != line.len() {
            trimmed.to_string()
        } else {
            line.to_string()
        };
        out.push(pushed);
    }
    out.join("\n")
}

/// hermes `_send_emotion` endpoint selection (SDK `robot_1_0`
/// `robotReplyEmotion` / `robotRecallEmotion` paths).
pub fn emotion_endpoint(recall: bool) -> &'static str {
    if recall {
        "/v1.0/robot/emotion/recall"
    } else {
        "/v1.0/robot/emotion/reply"
    }
}

/// hermes `_send_emotion` kwargs → JSON body (`emotion_type` 2 = text
/// emoji; `text_emotion` mirrors the SDK's
/// `RobotReplyEmotionRequestTextEmotion`).
pub fn emotion_payload(
    robot_code: &str,
    open_msg_id: &str,
    open_conversation_id: &str,
    emoji_name: &str,
) -> Value {
    json!({
        "robotCode": robot_code,
        "openMsgId": open_msg_id,
        "openConversationId": open_conversation_id,
        "emotionType": 2,
        "emotionName": emoji_name,
        "textEmotion": {
            "emotionId": EMOTION_ID,
            "emotionName": emoji_name,
            "text": emoji_name,
            "backgroundId": EMOTION_BACKGROUND_ID,
        },
    })
}

/// Entry point spawned by `run_messaging`.
pub async fn run(
    cfg: DingTalkConfig,
    dispatcher: Arc<Dispatcher>,
    pairing: Option<Arc<crate::pairing::PairingStore>>,
) {
    let resolved = cfg.resolve();
    if resolved.client_id.is_empty() || resolved.client_secret.is_empty() {
        eprintln!(
            "[dingtalk] disabled: set [messaging.dingtalk] client_id/client_secret or DINGTALK_CLIENT_ID/DINGTALK_CLIENT_SECRET"
        );
        return;
    }
    let mention_regexes: Vec<regex::Regex> = resolved
        .mention_patterns
        .iter()
        .filter_map(|p| match regex::Regex::new(p) {
            Ok(re) => Some(re),
            Err(e) => {
                eprintln!("[dingtalk] invalid mention pattern '{p}': {e}");
                None
            }
        })
        .collect();
    let runtime = Arc::new(Runtime {
        cfg: resolved,
        client: reqwest::Client::new(),
        session_webhooks: Mutex::new(HashMap::new()),
        dedup: Mutex::new(HashMap::new()),
        access_token: Mutex::new((String::new(), std::time::Instant::now())),
        mention_regexes,
        message_contexts: Mutex::new(HashMap::new()),
        done_emoji_fired: Mutex::new(HashSet::new()),
    });
    crate::messaging::register_platform_sender(
        "dingtalk",
        Arc::new(DingTalkSender {
            runtime: runtime.clone(),
        }),
    );

    let mut backoff_idx: usize = 0;
    loop {
        match stream_session(&runtime, &dispatcher, &pairing).await {
            Ok(()) => backoff_idx = 0,
            Err(e) => eprintln!("[dingtalk] stream error: {e}"),
        }
        let delay = RECONNECT_BACKOFF[backoff_idx.min(RECONNECT_BACKOFF.len() - 1)];
        eprintln!("[dingtalk] reconnecting in {delay}s");
        tokio::time::sleep(Duration::from_secs(delay)).await;
        backoff_idx += 1;
    }
}

/// Open the gateway connection and run one WebSocket session.
async fn stream_session(
    runtime: &Arc<Runtime>,
    dispatcher: &Arc<Dispatcher>,
    pairing: &Option<Arc<crate::pairing::PairingStore>>,
) -> Result<(), String> {
    use futures::StreamExt;

    let resp = runtime
        .client
        .post(format!("{API_BASE}/v1.0/gateway/connections/open"))
        .json(&json!({
            "clientId": runtime.cfg.client_id,
            "clientSecret": runtime.cfg.client_secret,
            "subscriptions": [{"type": "CALLBACK", "topic": BOT_TOPIC}],
            "ua": format!("ulnclaw/{}", env!("CARGO_PKG_VERSION")),
        }))
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("connections/open: {e}"))?;
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    if status >= 400 {
        return Err(format!("connections/open → {status}: {body}"));
    }
    let open: Value = serde_json::from_str(&body).map_err(|e| format!("open JSON: {e}"))?;
    let endpoint = open
        .get("endpoint")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "connections/open: no endpoint".to_string())?;
    let ticket = open.get("ticket").and_then(|v| v.as_str()).unwrap_or("");
    let ws_url = format!("{endpoint}?ticket={ticket}");

    let (ws, _) = tokio_tungstenite::connect_async(&ws_url)
        .await
        .map_err(|e| format!("ws connect: {e}"))?;
    eprintln!("[dingtalk] stream connected");
    let (mut sink, mut stream) = ws.split();

    while let Some(message) = stream.next().await {
        let message = match message {
            Ok(m) => m,
            Err(e) => return Err(format!("ws: {e}")),
        };
        let text = match message {
            tokio_tungstenite::tungstenite::Message::Text(t) => t,
            tokio_tungstenite::tungstenite::Message::Close(_) => {
                return Err("ws closed by server".into())
            }
            _ => continue,
        };
        let Ok(frame) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let frame_type = frame.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let headers = frame.get("headers").cloned().unwrap_or(json!({}));
        let message_id = headers
            .get("messageId")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let topic = headers.get("topic").and_then(|v| v.as_str()).unwrap_or("");

        match frame_type {
            "SYSTEM" => {
                // ping/disconnect frames: ack and keep going.
                send_ack(&mut sink, &message_id, "").await;
                if topic == "disconnect" {
                    return Err("server requested disconnect".into());
                }
            }
            "CALLBACK" => {
                let data_str = frame
                    .get("data")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                // ACK first so the SDK-style heartbeat never blocks
                // (hermes `_IncomingHandler.process` dispatches in a
                // background task).
                send_ack(&mut sink, &message_id, "").await;
                if topic == BOT_TOPIC && !data_str.is_empty() {
                    let payload: Value = match serde_json::from_str(&data_str) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    handle_bot_message(runtime, dispatcher, pairing, &payload).await;
                }
            }
            _ => {}
        }
    }
    Err("ws stream ended".into())
}

async fn send_ack(
    sink: &mut futures::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        tokio_tungstenite::tungstenite::Message,
    >,
    message_id: &str,
    data: &str,
) {
    use futures::SinkExt;
    let ack = json!({
        "code": 200,
        "headers": {"messageId": message_id, "contentType": "application/json"},
        "message": "OK",
        "data": data,
    });
    let _ = sink
        .send(tokio_tungstenite::tungstenite::Message::Text(
            ack.to_string(),
        ))
        .await;
}

/// hermes `_on_message` — gating, webhook caching, media resolution,
/// dispatch.
async fn handle_bot_message(
    runtime: &Arc<Runtime>,
    dispatcher: &Arc<Dispatcher>,
    pairing: &Option<Arc<crate::pairing::PairingStore>>,
    payload: &Value,
) {
    let msg_id = payload
        .get("msgId")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
    let conversation_id = payload
        .get("conversationId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let robot_code = payload
        .get("robotCode")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // hermes fires the 🤔Thinking text-emoji fire-and-forget on the raw
    // CALLBACK frame — before dedup and gates (`_send_emotion(...,
    // recall=False)` in the stream handler). Mirror that ordering; the
    // reaction is idempotent on DingTalk's side.
    if !msg_id.is_empty() && !conversation_id.is_empty() {
        let rt = runtime.clone();
        let code = if robot_code.is_empty() {
            runtime.cfg.client_id.clone()
        } else {
            robot_code.clone()
        };
        let mid = msg_id.clone();
        let cid = conversation_id.clone();
        tokio::spawn(async move {
            if let Err(e) = rt
                .send_emotion(&code, &mid, &cid, THINKING_EMOJI, false)
                .await
            {
                eprintln!("[dingtalk] Thinking reaction failed: {e}");
            }
        });
    }

    if runtime.is_duplicate(&msg_id).await {
        return;
    }

    let conversation_type = payload
        .get("conversationType")
        .and_then(|v| v.as_str())
        .unwrap_or("1");
    let is_group = conversation_type == "2";
    let sender_id = payload
        .get("senderId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let sender_nick = payload
        .get("senderNick")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| sender_id.clone());
    let sender_staff_id = payload
        .get("senderStaffId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let chat_id = if conversation_id.is_empty() {
        sender_id.clone()
    } else {
        conversation_id.clone()
    };
    if chat_id.is_empty() {
        return;
    }

    if !runtime.is_user_allowed(&sender_id, &sender_staff_id) {
        eprintln!(
            "[dingtalk] dropping message from non-allowlisted user {sender_staff_id}/{sender_id}"
        );
        return;
    }

    let text = extract_text(payload);
    let in_at_list = payload
        .get("isInAtList")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !runtime.should_process(&text, is_group, &chat_id, in_at_list) {
        return;
    }

    // DM pairing gate (hermes leaves DMs unrestricted apart from
    // allowed_users; ulnclaw adds the pairing union like the other
    // adapters).
    if !is_group {
        let allowed = runtime.cfg.allowed_users.iter().any(|u| u == "*")
            || runtime.is_user_allowed(&sender_id, &sender_staff_id);
        if !allowed {
            if let Some(store) = pairing {
                if !store.is_approved("dingtalk", &sender_id) {
                    if let Some(code_msg) = crate::messaging::pairing_offer_public(
                        store.as_ref(),
                        "dingtalk",
                        &sender_id,
                        &sender_nick,
                    ) {
                        let _ = send_dingtalk_text(runtime, &chat_id, payload, &code_msg).await;
                    }
                    return;
                }
            }
        }
    }

    // Stash the inbound ids for the post-reply 🥳Done swap (hermes
    // `_message_contexts[chat_id] = message` +
    // `_done_emoji_fired.discard(chat_id)`).
    runtime
        .record_message_context(&chat_id, &msg_id, &conversation_id)
        .await;

    let session_webhook = payload
        .get("sessionWebhook")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let expired = payload
        .get("sessionWebhookExpiredMilli")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    runtime
        .store_webhook(&chat_id, session_webhook, expired)
        .await;

    // Resolve media download codes.
    let attachments: Vec<MediaAttachment> = collect_media_refs(runtime, payload, &robot_code)
        .await
        .into_iter()
        .flatten()
        .collect();

    if text.trim().is_empty() && attachments.is_empty() {
        return;
    }

    let mut event = MessageEvent {
        platform: "dingtalk".into(),
        chat_id: chat_id.clone(),
        sender_id: sender_id.clone(),
        sender_name: sender_nick,
        text,
        message_id: msg_id,
        attachments,
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
    let (reply_text, _media_paths) = crate::messaging::extract_media_tags(&full);
    // DingTalk session webhooks only carry text/markdown — media paths
    // degrade to their path lines (hermes sends image URLs in markdown;
    // local files have no public URL to link).
    if !reply_text.trim().is_empty() {
        // With a card template configured the reply rides an AI Card
        // (hermes `_create_and_stream_card`); any failure falls back to
        // the session-webhook markdown.
        // P705: ledger-protected reply delivery (card first, webhook
        // fallback).
        let sent = dispatcher
            .try_send_with_ledger("dingtalk", &chat_id, &reply_text, || async {
                let mut result: Result<(), String> = Err("no delivery path attempted".to_string());
                if !runtime.cfg.card_template_id.is_empty() {
                    match runtime
                        .send_ai_card(is_group, &conversation_id, &sender_staff_id, &reply_text)
                        .await
                    {
                        Ok(track_id) => {
                            eprintln!("[dingtalk] AI card created+finalized: {track_id}");
                            result = Ok(());
                        }
                        Err(e) => {
                            eprintln!("[dingtalk] AI card failed, falling back to webhook: {e}");
                            result = Err(e.to_string());
                        }
                    }
                }
                if result.is_err() {
                    match runtime.send_markdown(&chat_id, &reply_text).await {
                        Ok(()) => result = Ok(()),
                        Err(e) => {
                            eprintln!("[dingtalk] reply failed: {e}");
                            result = Err(e.to_string());
                        }
                    }
                }
                result
            })
            .await;
        if sent {
            if let Some((target_msg, target_conv)) = runtime.take_done_reaction(&chat_id).await {
                // hermes `_fire_done_reaction`: recall 🤔Thinking, then add
                // 🥳Done — once per inbound message.
                let rt = runtime.clone();
                let code = if robot_code.is_empty() {
                    runtime.cfg.client_id.clone()
                } else {
                    robot_code.clone()
                };
                tokio::spawn(async move {
                    if let Err(e) = rt
                        .send_emotion(&code, &target_msg, &target_conv, THINKING_EMOJI, true)
                        .await
                    {
                        eprintln!("[dingtalk] Thinking recall failed: {e}");
                    }
                    if let Err(e) = rt
                        .send_emotion(&code, &target_msg, &target_conv, DONE_EMOJI, false)
                        .await
                    {
                        eprintln!("[dingtalk] Done reaction failed: {e}");
                    }
                });
            }
        }
    }
}

/// Outbound text for pairing offers before any webhook may be cached:
/// uses the inbound message's own sessionWebhook directly.
async fn send_dingtalk_text(
    runtime: &Arc<Runtime>,
    _chat_id: &str,
    payload: &Value,
    text: &str,
) -> Result<(), String> {
    let webhook = payload
        .get("sessionWebhook")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if !is_dingtalk_webhook_url(webhook) {
        return Err("no session webhook".into());
    }
    let normalized = normalize_markdown(text);
    let payload = json!({
        "msgtype": "markdown",
        "markdown": {"title": "Ulnclaw", "text": normalized},
    });
    let resp = runtime
        .client
        .post(webhook)
        .json(&payload)
        .timeout(WEBHOOK_SEND_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("webhook send: {e}"))?;
    if resp.status().is_success() {
        Ok(())
    } else {
        Err(format!("webhook HTTP {}", resp.status()))
    }
}

/// hermes `_extract_text`: text.content → richText → audio recognition →
/// file name → doc card.
pub fn extract_text(payload: &Value) -> String {
    if let Some(content) = payload.pointer("/text/content").and_then(|v| v.as_str()) {
        let trimmed = content.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    if let Some(rich_list) = payload
        .pointer("/richTextContent/richTextList")
        .or_else(|| payload.pointer("/richText"))
        .and_then(|v| v.as_array())
    {
        let parts: Vec<String> = rich_list
            .iter()
            .filter_map(|item| {
                item.get("text")
                    .or_else(|| item.get("content"))
                    .and_then(|v| v.as_str())
            })
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.to_string())
            .collect();
        if !parts.is_empty() {
            return parts.join(" ");
        }
    }
    let msg_type = payload
        .get("msgtype")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    match msg_type {
        "audio" => {
            if let Some(recognition) = payload
                .pointer("/extensions/content/recognition")
                .and_then(|v| v.as_str())
            {
                if !recognition.trim().is_empty() {
                    return recognition.trim().to_string();
                }
            }
        }
        "file" => {
            if let Some(fname) = payload
                .pointer("/extensions/content/fileName")
                .and_then(|v| v.as_str())
            {
                if !fname.is_empty() {
                    return format!("[文件] {fname}");
                }
            }
        }
        "card" => {
            let title = payload
                .pointer("/extensions/card/title")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let mut doc_url = String::new();
            if let Some(raw_content) = payload.pointer("/extensions/card/content") {
                if let Some(obj) = raw_content.as_object() {
                    doc_url = obj
                        .get("url")
                        .or_else(|| obj.get("docUrl"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                } else if let Some(s) = raw_content.as_str() {
                    let trimmed = s.trim();
                    if !trimmed.is_empty() {
                        doc_url = serde_json::from_str::<Value>(trimmed)
                            .ok()
                            .and_then(|parsed| {
                                parsed
                                    .get("url")
                                    .or_else(|| parsed.get("docUrl"))
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string())
                            })
                            .unwrap_or_else(|| trimmed.to_string());
                    }
                }
            }
            let mut parts = Vec::new();
            if !title.is_empty() {
                parts.push(format!("[文档] {title}"));
            }
            if !doc_url.is_empty() {
                parts.push(doc_url);
            }
            if !parts.is_empty() {
                return parts.join(" ");
            }
        }
        _ => {}
    }
    String::new()
}

/// Collect `(downloadCode, filename)` media refs (hermes `_extract_media`
/// core) and resolve them through the robot file-download API.
async fn collect_media_refs(
    runtime: &Arc<Runtime>,
    payload: &Value,
    robot_code: &str,
) -> Vec<Option<MediaAttachment>> {
    let mut refs: Vec<(String, String)> = Vec::new();
    let msg_type = payload
        .get("msgtype")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if let Some(rich_list) = payload
        .pointer("/richTextContent/richTextList")
        .or_else(|| payload.pointer("/richText"))
        .and_then(|v| v.as_array())
    {
        for item in rich_list {
            if let Some(code) = item
                .get("downloadCode")
                .or_else(|| item.get("download_code"))
                .and_then(|v| v.as_str())
            {
                if !code.is_empty() {
                    let ext = item.get("type").and_then(|v| v.as_str()).unwrap_or("file");
                    refs.push((code.to_string(), format!("rich_{ext}")));
                }
            }
        }
    }
    if matches!(msg_type, "file" | "image") {
        if let Some(code) = payload
            .pointer("/extensions/content/downloadCode")
            .and_then(|v| v.as_str())
        {
            if !code.is_empty() {
                let fname = payload
                    .pointer("/extensions/content/fileName")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| {
                        format!(
                            "attachment.{}",
                            if msg_type == "image" { "jpg" } else { "bin" }
                        )
                    });
                refs.push((code.to_string(), fname));
            }
        }
    }

    let mut results = Vec::new();
    for (code, fname) in refs {
        results.push(runtime.download_media_code(&code, robot_code, &fname).await);
    }
    results
}

struct DingTalkSender {
    runtime: Arc<Runtime>,
}

#[async_trait::async_trait]
impl crate::messaging::PlatformSender for DingTalkSender {
    async fn send_text(&self, chat_id: &str, text: &str) {
        if let Err(e) = self.runtime.send_markdown(chat_id, text).await {
            eprintln!("[dingtalk] send_text to {chat_id} failed: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_runtime() -> Runtime {
        Runtime {
            cfg: ResolvedDingTalk {
                client_id: String::new(),
                client_secret: String::new(),
                require_mention: false,
                free_response_chats: Vec::new(),
                allowed_chats: Vec::new(),
                allowed_users: Vec::new(),
                mention_patterns: Vec::new(),
                card_template_id: String::new(),
                robot_code: String::new(),
            },
            client: reqwest::Client::new(),
            session_webhooks: Mutex::new(HashMap::new()),
            dedup: Mutex::new(HashMap::new()),
            access_token: Mutex::new((String::new(), std::time::Instant::now())),
            mention_regexes: Vec::new(),
            message_contexts: Mutex::new(HashMap::new()),
            done_emoji_fired: Mutex::new(HashSet::new()),
        }
    }

    #[test]
    fn webhook_url_validation() {
        assert!(is_dingtalk_webhook_url(
            "https://oapi.dingtalk.com/robot/sendBySession?session=x"
        ));
        assert!(is_dingtalk_webhook_url("https://api.dingtalk.com/v1.0/x"));
        assert!(!is_dingtalk_webhook_url(
            "https://evil.com/oapi.dingtalk.com"
        ));
        assert!(!is_dingtalk_webhook_url("http://oapi.dingtalk.com/x"));
    }

    #[test]
    fn normalize_markdown_numbered_lists() {
        let input = "Intro\n1. first\n2. second";
        let out = normalize_markdown(input);
        assert!(out.contains("Intro\n\n1. first"));
    }

    #[test]
    fn normalize_markdown_dedents_fences() {
        let input = "text\n    ```rust\n    code\n    ```";
        let out = normalize_markdown(input);
        assert!(out.contains("\n```rust"));
    }

    #[test]
    fn extract_text_plain() {
        let payload = json!({"text": {"content": "  hello bot  "}, "msgtype": "text"});
        assert_eq!(extract_text(&payload), "hello bot");
    }

    #[test]
    fn extract_text_audio_recognition() {
        let payload = json!({
            "msgtype": "audio",
            "extensions": {"content": {"recognition": "语音内容"}},
        });
        assert_eq!(extract_text(&payload), "语音内容");
    }

    #[test]
    fn extract_text_file_name() {
        let payload = json!({
            "msgtype": "file",
            "extensions": {"content": {"fileName": "report.pdf"}},
        });
        assert_eq!(extract_text(&payload), "[文件] report.pdf");
    }

    #[test]
    fn extract_text_card_doc() {
        let payload = json!({
            "msgtype": "card",
            "extensions": {"card": {"title": "设计文档", "content": "{\"url\": \"https://docs.example.com/d/1\"}"}},
        });
        let text = extract_text(&payload);
        assert!(text.contains("[文档] 设计文档"));
        assert!(text.contains("https://docs.example.com/d/1"));
    }

    #[test]
    fn group_gating_rules() {
        let cfg = ResolvedDingTalk {
            client_id: String::new(),
            client_secret: String::new(),
            require_mention: true,
            free_response_chats: vec!["free-chat".into()],
            allowed_chats: vec!["allowed-chat".into(), "free-chat".into()],
            allowed_users: Vec::new(),
            mention_patterns: vec!["^小马".into()],
            card_template_id: String::new(),
            robot_code: String::new(),
        };
        let runtime = Runtime {
            cfg,
            client: reqwest::Client::new(),
            session_webhooks: Mutex::new(HashMap::new()),
            dedup: Mutex::new(HashMap::new()),
            access_token: Mutex::new((String::new(), std::time::Instant::now())),
            mention_regexes: vec![regex::Regex::new("^小马").unwrap()],
            message_contexts: Mutex::new(HashMap::new()),
            done_emoji_fired: Mutex::new(HashSet::new()),
        };
        // allowed_chats hard gate
        assert!(!runtime.should_process("hi", true, "other-chat", true));
        // mention passes
        assert!(runtime.should_process("hi", true, "allowed-chat", true));
        // no mention, not free → blocked
        assert!(!runtime.should_process("hi", true, "allowed-chat", false));
        // wake word passes
        assert!(runtime.should_process("小马你好", true, "allowed-chat", false));
        // free chat passes without mention
        assert!(runtime.should_process("hi", true, "free-chat", false));
        // DMs always pass
        assert!(runtime.should_process("hi", false, "any", false));
    }

    #[test]
    fn user_allowlist_matching() {
        let runtime = Runtime {
            cfg: ResolvedDingTalk {
                client_id: String::new(),
                client_secret: String::new(),
                require_mention: false,
                free_response_chats: Vec::new(),
                allowed_chats: Vec::new(),
                allowed_users: vec!["Manager123".into()],
                mention_patterns: Vec::new(),
                card_template_id: String::new(),
                robot_code: String::new(),
            },
            client: reqwest::Client::new(),
            session_webhooks: Mutex::new(HashMap::new()),
            dedup: Mutex::new(HashMap::new()),
            access_token: Mutex::new((String::new(), std::time::Instant::now())),
            mention_regexes: Vec::new(),
            message_contexts: Mutex::new(HashMap::new()),
            done_emoji_fired: Mutex::new(HashSet::new()),
        };
        assert!(runtime.is_user_allowed("x", "manager123"));
        assert!(runtime.is_user_allowed("MANAGER123", ""));
        assert!(!runtime.is_user_allowed("someone", "else"));
    }

    #[test]
    fn webhook_expiry_margin() {
        let mut map = HashMap::new();
        let future_ms = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64)
            + 60 * 60 * 1000;
        map.insert(
            "chat1".to_string(),
            ("https://oapi.dingtalk.com/x".to_string(), future_ms),
        );
        assert!(Runtime::valid_webhook(&map, "chat1").is_some());
        // Expires within the 5-minute margin → rejected.
        let soon_ms = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64)
            + 60 * 1000;
        map.insert(
            "chat2".to_string(),
            ("https://oapi.dingtalk.com/y".to_string(), soon_ms),
        );
        assert!(Runtime::valid_webhook(&map, "chat2").is_none());
        // No expiry recorded → always valid.
        map.insert(
            "chat3".to_string(),
            ("https://oapi.dingtalk.com/z".to_string(), 0),
        );
        assert!(Runtime::valid_webhook(&map, "chat3").is_some());
    }

    #[tokio::test]
    async fn dedup_behavior() {
        let runtime = Runtime {
            cfg: ResolvedDingTalk {
                client_id: String::new(),
                client_secret: String::new(),
                require_mention: false,
                free_response_chats: Vec::new(),
                allowed_chats: Vec::new(),
                allowed_users: Vec::new(),
                mention_patterns: Vec::new(),
                card_template_id: String::new(),
                robot_code: String::new(),
            },
            client: reqwest::Client::new(),
            session_webhooks: Mutex::new(HashMap::new()),
            dedup: Mutex::new(HashMap::new()),
            access_token: Mutex::new((String::new(), std::time::Instant::now())),
            mention_regexes: Vec::new(),
            message_contexts: Mutex::new(HashMap::new()),
            done_emoji_fired: Mutex::new(HashSet::new()),
        };
        assert!(!runtime.is_duplicate("m1").await);
        assert!(runtime.is_duplicate("m1").await);
    }

    #[test]
    fn emotion_endpoint_selection() {
        assert_eq!(emotion_endpoint(false), "/v1.0/robot/emotion/reply");
        assert_eq!(emotion_endpoint(true), "/v1.0/robot/emotion/recall");
    }

    #[test]
    fn emotion_payload_matches_sdk_schema() {
        let payload = emotion_payload("rc", "msg1", "conv1", THINKING_EMOJI);
        assert_eq!(payload["robotCode"], "rc");
        assert_eq!(payload["openMsgId"], "msg1");
        assert_eq!(payload["openConversationId"], "conv1");
        assert_eq!(payload["emotionType"], 2);
        assert_eq!(payload["emotionName"], THINKING_EMOJI);
        let text = &payload["textEmotion"];
        assert_eq!(text["emotionId"], "2659900");
        assert_eq!(text["emotionName"], THINKING_EMOJI);
        assert_eq!(text["text"], THINKING_EMOJI);
        assert_eq!(text["backgroundId"], "im_bg_1");
    }

    #[tokio::test]
    async fn done_reaction_swap_is_idempotent_per_chat() {
        let runtime = test_runtime();
        runtime.record_message_context("chat1", "m1", "c1").await;
        assert_eq!(
            runtime.take_done_reaction("chat1").await,
            Some(("m1".to_string(), "c1".to_string()))
        );
        // Second call for the same inbound is suppressed (hermes
        // `_done_emoji_fired`).
        assert_eq!(runtime.take_done_reaction("chat1").await, None);
        // A new inbound message resets the cycle.
        runtime.record_message_context("chat1", "m2", "c1").await;
        assert_eq!(
            runtime.take_done_reaction("chat1").await,
            Some(("m2".to_string(), "c1".to_string()))
        );
    }

    #[tokio::test]
    async fn done_reaction_requires_context() {
        let runtime = test_runtime();
        assert_eq!(runtime.take_done_reaction("ghost").await, None);
        runtime.record_message_context("empty", "", "").await;
        assert_eq!(runtime.take_done_reaction("empty").await, None);
    }

    // -- AI Card (card_1_0) parity -----------------------------------------

    /// axum mock of the DingTalk card endpoints — logs (method, path,
    /// body) per call so the create → deliver → stream sequence is
    /// assertable.
    async fn spawn_card_api(log: Arc<std::sync::Mutex<Vec<(String, String, Value)>>>) -> String {
        use axum::extract::State;
        use axum::routing::{post, put};
        type Log = Arc<std::sync::Mutex<Vec<(String, String, Value)>>>;
        let app = axum::Router::new()
            .route(
                "/v1.0/oauth2/accessToken",
                post(
                    move |State(log): State<Log>, axum::Json(body): axum::Json<Value>| async move {
                        log.lock().unwrap().push((
                            "POST".into(),
                            "/v1.0/oauth2/accessToken".into(),
                            body,
                        ));
                        axum::Json(json!({"accessToken": "TOK", "expireIn": 7200}))
                    },
                ),
            )
            .route(
                "/v1.0/card/instances",
                post(
                    move |State(log): State<Log>, axum::Json(body): axum::Json<Value>| async move {
                        log.lock().unwrap().push((
                            "POST".into(),
                            "/v1.0/card/instances".into(),
                            body,
                        ));
                        axum::Json(json!({"result": {"outTrackId": "x"}}))
                    },
                ),
            )
            .route(
                "/v1.0/card/instances/deliver",
                post(
                    move |State(log): State<Log>, axum::Json(body): axum::Json<Value>| async move {
                        log.lock().unwrap().push((
                            "POST".into(),
                            "/v1.0/card/instances/deliver".into(),
                            body,
                        ));
                        axum::Json(json!({"result": true}))
                    },
                ),
            )
            .route(
                "/v1.0/card/streaming",
                put(
                    move |State(log): State<Log>, axum::Json(body): axum::Json<Value>| async move {
                        log.lock().unwrap().push((
                            "PUT".into(),
                            "/v1.0/card/streaming".into(),
                            body,
                        ));
                        axum::Json(json!({"result": true}))
                    },
                ),
            )
            .with_state(log);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn ai_card_group_create_deliver_stream() {
        let _env_guard = crate::models_dev::test_env_lock();
        let log = Arc::new(std::sync::Mutex::new(Vec::<(String, String, Value)>::new()));
        let base = spawn_card_api(log.clone()).await;
        std::env::set_var("DINGTALK_API_BASE", &base);
        let mut runtime = test_runtime();
        runtime.cfg.client_id = "cid".into();
        runtime.cfg.client_secret = "csecret".into();
        runtime.cfg.card_template_id = "tpl-123".into();
        runtime.cfg.robot_code = "robot-9".into();
        let track = runtime
            .send_ai_card(true, "conv-1", "staff-1", "Hello **card**")
            .await
            .expect("card send should succeed");
        std::env::remove_var("DINGTALK_API_BASE");
        assert!(track.starts_with("ulnclaw_"));
        let reqs = log.lock().unwrap();
        let paths: Vec<String> = reqs.iter().map(|(_, p, _)| p.clone()).collect();
        assert_eq!(
            paths,
            vec![
                "/v1.0/oauth2/accessToken",
                "/v1.0/card/instances",
                "/v1.0/card/instances/deliver",
                "/v1.0/card/streaming",
            ]
        );
        // create: STREAM callback + template + forwardable spaces.
        let create = &reqs[1].2;
        assert_eq!(create["cardTemplateId"], "tpl-123");
        assert_eq!(create["callbackType"], "STREAM");
        assert_eq!(create["cardData"]["cardParamMap"]["content"], "");
        assert_eq!(create["imGroupOpenSpaceModel"]["supportForward"], true);
        // deliver: group space + configured robot_code.
        let deliver = &reqs[2].2;
        assert_eq!(deliver["openSpaceId"], "dtv1.card//IM_GROUP.conv-1");
        assert_eq!(deliver["imGroupOpenDeliverModel"]["robotCode"], "robot-9");
        assert_eq!(deliver["userIdType"], 1);
        // stream: full content, finalized, keyed "content".
        let stream = &reqs[3].2;
        assert_eq!(stream["key"], "content");
        assert_eq!(stream["content"], "Hello **card**");
        assert_eq!(stream["isFull"], true);
        assert_eq!(stream["isFinalize"], true);
        assert_eq!(stream["isError"], false);
    }

    #[tokio::test]
    async fn ai_card_dm_uses_robot_space_and_robot_code_defaults_to_client_id() {
        let _env_guard = crate::models_dev::test_env_lock();
        let log = Arc::new(std::sync::Mutex::new(Vec::<(String, String, Value)>::new()));
        let base = spawn_card_api(log.clone()).await;
        std::env::set_var("DINGTALK_API_BASE", &base);
        let mut runtime = test_runtime();
        runtime.cfg.client_id = "cid".into();
        runtime.cfg.client_secret = "csecret".into();
        runtime.cfg.card_template_id = "tpl-123".into();
        // robot_code left empty → falls back to client_id (hermes).
        let track = runtime
            .send_ai_card(false, "conv-1", "staff-77", "dm reply")
            .await
            .expect("card send should succeed");
        std::env::remove_var("DINGTALK_API_BASE");
        assert!(track.starts_with("ulnclaw_"));
        let reqs = log.lock().unwrap();
        let deliver = &reqs[2].2;
        assert_eq!(deliver["openSpaceId"], "dtv1.card//IM_ROBOT.staff-77");
        assert_eq!(deliver["imRobotOpenDeliverModel"]["spaceType"], "IM_ROBOT");
    }

    #[tokio::test]
    async fn ai_card_dm_without_staff_id_is_skipped() {
        let _env_guard = crate::models_dev::test_env_lock();
        let log = Arc::new(std::sync::Mutex::new(Vec::<(String, String, Value)>::new()));
        let base = spawn_card_api(log.clone()).await;
        std::env::set_var("DINGTALK_API_BASE", &base);
        let mut runtime = test_runtime();
        runtime.cfg.client_id = "cid".into();
        runtime.cfg.client_secret = "csecret".into();
        runtime.cfg.card_template_id = "tpl-123".into();
        let err = runtime
            .send_ai_card(false, "conv-1", "", "dm reply")
            .await
            .unwrap_err();
        std::env::remove_var("DINGTALK_API_BASE");
        assert!(err.contains("sender_staff_id"), "got: {err}");
        // create fired but deliver was skipped (hermes ordering).
        let reqs = log.lock().unwrap();
        let paths: Vec<String> = reqs.iter().map(|(_, p, _)| p.clone()).collect();
        assert!(paths.contains(&"/v1.0/card/instances".to_string()));
        assert!(!paths.contains(&"/v1.0/card/instances/deliver".to_string()));
    }

    #[test]
    fn ai_card_requires_template() {
        let runtime = test_runtime(); // no card_template_id
        assert!(runtime.cfg.card_template_id.is_empty());
    }

    #[test]
    fn card_config_env_overrides() {
        let _env_guard = crate::models_dev::test_env_lock();
        std::env::set_var("DINGTALK_CARD_TEMPLATE_ID", "tpl-env");
        std::env::set_var("DINGTALK_ROBOT_CODE", "robot-env");
        std::env::set_var("DINGTALK_API_BASE", "http://example.test");
        let resolved = DingTalkConfig::default().resolve();
        std::env::remove_var("DINGTALK_CARD_TEMPLATE_ID");
        std::env::remove_var("DINGTALK_ROBOT_CODE");
        std::env::remove_var("DINGTALK_API_BASE");
        assert_eq!(resolved.card_template_id, "tpl-env");
        assert_eq!(resolved.robot_code, "robot-env");
        assert_eq!(dingtalk_api_base(), API_BASE);
    }
}

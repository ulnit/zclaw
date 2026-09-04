//! Messaging platform gateways — port of hermes `gateway/platforms/`.
//!
//! Hermes runs chat platforms (Telegram/Discord/Slack/…) inside the
//! gateway process: each adapter normalizes incoming messages into a
//! `MessageEvent`, the core runs an agent turn per chat, and the reply
//! goes back through the platform. ulnclaw ports the architecture plus
//! three self-contained adapters:
//!
//! * **Telegram** — Bot API long-polling (`getUpdates` / `sendMessage`)
//! * **Discord**  — Gateway v10 websocket (IDENTIFY/heartbeat/
//!   MESSAGE_CREATE) + REST message send
//! * **Slack**    — Socket Mode websocket (events_api envelopes) +
//!   `chat.postMessage`
//!
//! Access control mirrors hermes pairing semantics: every platform is
//! allowlist-gated (`allowed_chat_ids` / `allowed_channel_ids`); an empty
//! allowlist refuses all traffic and logs the ids to add (fail closed).

use crate::agent::Agent;
use crate::error::{AgentError, Result};
use crate::session::sqlite::SqliteSessionStore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

/// `[messaging]` config block.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MessagingConfig {
    #[serde(default)]
    pub telegram: TelegramConfig,
    #[serde(default)]
    pub discord: DiscordConfig,
    #[serde(default)]
    pub slack: SlackConfig,
    /// Signal via signal-cli HTTP daemon (hermes `platforms.signal`).
    #[serde(default)]
    pub signal: crate::signal::SignalConfig,
    /// Offer pairing codes to unknown senders (hermes unauthorized-DM
    /// `pair` behavior). Approved codes join the allowlist as a union.
    #[serde(default = "default_pairing")]
    pub pairing: bool,
    /// Announce gateway restarts to each platform's most recent
    /// channel on startup (hermes `gateway_restart_notification`
    /// parity) — a lifecycle ping that the bot is back.
    #[serde(default = "default_gateway_restart_notification")]
    pub gateway_restart_notification: bool,
    /// Inject image attachments natively into the user turn as
    /// multimodal content parts (P226, hermes media-injection parity).
    /// When false — or for non-image media — attachments stay path
    /// references the agent inspects with vision_analyze/read_file.
    #[serde(default = "default_multimodal_injection")]
    pub multimodal_injection: bool,
    /// Per-platform slash command access control (hermes
    /// `slash_access`, P718): of the users allowed to talk, which can
    /// run which slash commands. Keyed by platform id; platforms
    /// without an entry keep the legacy ungated behavior.
    #[serde(default)]
    pub slash_access:
        std::collections::HashMap<String, crate::slash_access::SlashAccessScopeConfig>,
    /// WhatsApp Cloud webhook platform (mounted on the gateway router).
    #[serde(default)]
    pub whatsapp_cloud: crate::webhook_platforms::WhatsAppCloudConfig,
    /// Microsoft Graph change-notification ingress (gateway router).
    #[serde(default)]
    pub msgraph: crate::webhook_platforms::MsGraphConfig,
    /// Generic inbound-webhook platform: signed routes from external
    /// services (hermes `platforms.webhook`).
    #[serde(default)]
    pub webhook: crate::webhook_platforms::WebhookConfig,
    /// BlueBubbles iMessage bridge (hermes `platforms.bluebubbles`),
    /// mounted on the gateway router.
    #[serde(default)]
    pub bluebubbles: crate::webhook_platforms::BlueBubblesConfig,
    /// Weixin personal account via the iLink Bot API (hermes
    /// `platforms.weixin`).
    #[serde(default)]
    pub weixin: crate::weixin::WeixinConfig,
    /// Official QQ Bot API v2 adapter (hermes `platforms.qq`).
    #[serde(default)]
    pub qq: crate::qqbot::QQBotConfig,
    /// Yuanbao WS-gateway adapter (hermes `platforms.yuanbao`).
    #[serde(default)]
    pub yuanbao: crate::yuanbao::YuanbaoConfig,
    /// Email via IMAP/SMTP (hermes `platforms.email` plugin).
    #[serde(default)]
    pub email: crate::email_platform::EmailConfig,
    /// Mattermost via REST v4 + WebSocket (hermes `platforms.mattermost`
    /// plugin).
    #[serde(default)]
    pub mattermost: crate::mattermost::MattermostConfig,
    /// Matrix via the Client-Server API (hermes `platforms.matrix`
    /// plugin, sans E2EE).
    #[serde(default)]
    pub matrix: crate::matrix::MatrixConfig,
    /// DingTalk via Stream Mode (hermes `platforms.dingtalk` plugin).
    #[serde(default)]
    pub dingtalk: crate::dingtalk::DingTalkConfig,
    /// WeCom AI Bot via WebSocket gateway (hermes `platforms.wecom`
    /// plugin).
    #[serde(default)]
    pub wecom: crate::wecom::WeComConfig,
    /// Feishu/Lark via gateway webhook (hermes `platforms.feishu`
    /// plugin, webhook transport).
    #[serde(default)]
    pub feishu: crate::feishu::FeishuConfig,
    /// Home Assistant state-change events via the WS API (hermes
    /// `platforms.homeassistant` plugin).
    #[serde(default)]
    pub homeassistant: crate::homeassistant::HomeassistantConfig,
    /// Twilio SMS via REST + gateway webhook (hermes `platforms.sms`
    /// plugin).
    #[serde(default)]
    pub sms: crate::sms::SmsConfig,
    /// WhatsApp via an external Baileys HTTP bridge (hermes
    /// `platforms.whatsapp` plugin, bridge-client transport).
    #[serde(default)]
    pub whatsapp: crate::whatsapp::WhatsappConfig,
    /// IRC via a zero-dependency TLS client (hermes `platforms.irc`
    /// plugin).
    #[serde(default)]
    pub irc: crate::irc::IrcConfig,
    /// ntfy topics via HTTP streaming (hermes `platforms.ntfy` plugin).
    #[serde(default)]
    pub ntfy: crate::ntfy::NtfyConfig,
    /// SimpleX via the simplex-chat daemon WS API (hermes
    /// `platforms.simplex` plugin).
    #[serde(default)]
    pub simplex: crate::simplex::SimplexConfig,
    /// Microsoft Teams via the raw Bot Framework protocol (hermes
    /// `platforms.teams` plugin), mounted on the gateway router.
    #[serde(default)]
    pub teams: crate::teams::TeamsConfig,
    /// LINE Messaging API via gateway webhook (hermes `platforms.line`
    /// plugin).
    #[serde(default)]
    pub line: crate::line::LineConfig,
    /// Google Chat via HTTP-callback events + Chat REST API (hermes
    /// `platforms.google_chat` plugin), mounted on the gateway router.
    #[serde(default)]
    pub google_chat: crate::google_chat::GoogleChatConfig,
    /// iMessage via the `buzz` CLI (hermes `platforms.buzz` plugin,
    /// polling transport).
    #[serde(default)]
    pub buzz: crate::buzz::BuzzConfig,
    /// iMessage via the Photon sidecar HTTP API (hermes
    /// `platforms.photon` plugin, sidecar-client transport).
    #[serde(default)]
    pub photon: crate::photon::PhotonConfig,
    /// Raft activity wake events via gateway webhook (hermes
    /// `platforms.raft` plugin, wake-endpoint half).
    #[serde(default)]
    pub raft: crate::raft::RaftConfig,
    /// A2A v1.0 agent server surface on the gateway (hermes
    /// `platforms.a2a` plugin).
    #[serde(default)]
    pub a2a: crate::a2a::A2aConfig,
}

fn default_multimodal_injection() -> bool {
    true
}

fn default_pairing() -> bool {
    true
}

fn default_gateway_restart_notification() -> bool {
    true
}

impl MessagingConfig {
    /// True when any platform section is enabled — the gate for
    /// spawning `run_messaging` (router-mounted platforms like
    /// whatsapp_cloud/msgraph/webhook ride gateway routes but still
    /// register senders via their startup paths).
    pub fn any_platform_enabled(&self) -> bool {
        self.telegram.enabled
            || self.discord.enabled
            || self.slack.enabled
            || self.signal.enabled
            || self.whatsapp_cloud.enabled
            || self.msgraph.enabled
            || self.webhook.enabled
            || self.bluebubbles.enabled
            || self.weixin.enabled
            || self.qq.enabled
            || self.yuanbao.enabled
            || self.email.enabled
            || self.mattermost.enabled
            || self.matrix.enabled
            || self.dingtalk.enabled
            || self.wecom.enabled
            || self.feishu.enabled
            || self.homeassistant.enabled
            || self.sms.enabled
            || self.whatsapp.enabled
            || self.irc.enabled
            || self.ntfy.enabled
            || self.simplex.enabled
            || self.teams.enabled
            || self.line.enabled
            || self.google_chat.enabled
            || self.buzz.enabled
            || self.photon.enabled
            || self.raft.enabled
            || self.a2a.enabled
    }

    /// Names of every enabled platform section, in config order (setup
    /// wizard summary + monitoring heartbeat input).
    pub fn enabled_platform_names(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        let entries: [(&str, bool); 26] = [
            ("telegram", self.telegram.enabled),
            ("discord", self.discord.enabled),
            ("slack", self.slack.enabled),
            ("signal", self.signal.enabled),
            ("whatsapp_cloud", self.whatsapp_cloud.enabled),
            ("msgraph", self.msgraph.enabled),
            ("webhook", self.webhook.enabled),
            ("bluebubbles", self.bluebubbles.enabled),
            ("weixin", self.weixin.enabled),
            ("qq", self.qq.enabled),
            ("yuanbao", self.yuanbao.enabled),
            ("email", self.email.enabled),
            ("mattermost", self.mattermost.enabled),
            ("matrix", self.matrix.enabled),
            ("dingtalk", self.dingtalk.enabled),
            ("wecom", self.wecom.enabled),
            ("feishu", self.feishu.enabled),
            ("homeassistant", self.homeassistant.enabled),
            ("sms", self.sms.enabled),
            ("whatsapp", self.whatsapp.enabled),
            ("irc", self.irc.enabled),
            ("ntfy", self.ntfy.enabled),
            ("simplex", self.simplex.enabled),
            ("teams", self.teams.enabled),
            ("line", self.line.enabled),
            ("google_chat", self.google_chat.enabled),
        ];
        for (name, enabled) in entries {
            if enabled {
                out.push(name);
            }
        }
        if self.buzz.enabled {
            out.push("buzz");
        }
        if self.photon.enabled {
            out.push("photon");
        }
        if self.raft.enabled {
            out.push("raft");
        }
        if self.a2a.enabled {
            out.push("a2a");
        }
        out
    }
}

/// `[messaging.telegram]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TelegramConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Bot token from @BotFather (falls back to `TELEGRAM_BOT_TOKEN`).
    #[serde(default)]
    pub bot_token: String,
    /// Chat ids allowed to talk to the bot (hermes pairing). Empty =
    /// refuse all and log the ids that try.
    #[serde(default)]
    pub allowed_chat_ids: Vec<String>,
}

/// `[messaging.discord]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DiscordConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Bot token (falls back to `DISCORD_BOT_TOKEN`).
    #[serde(default)]
    pub bot_token: String,
    /// Channel ids allowed to talk to the bot (empty = refuse all).
    #[serde(default)]
    pub allowed_channel_ids: Vec<String>,
    /// Seconds before clarify button prompts visually expire (hermes
    /// `approvals.discord_prompt_timeout`; 0 = 300 s default, clamped to
    /// 30–900 s like hermes).
    #[serde(default)]
    pub prompt_timeout_secs: u64,
}

/// `[messaging.slack]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SlackConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Bot token `xoxb-…` (falls back to `SLACK_BOT_TOKEN`).
    #[serde(default)]
    pub bot_token: String,
    /// App-level token `xapp-…` for Socket Mode (falls back to
    /// `SLACK_APP_TOKEN`).
    #[serde(default)]
    pub app_token: String,
    /// Channel ids allowed to talk to the bot (empty = refuse all).
    #[serde(default)]
    pub allowed_channel_ids: Vec<String>,
    /// Custom assistant-thread typing status (hermes `typing_status_text`).
    /// When unset the gateway shows "is thinking..." and switches to an
    /// elapsed-time "still working… (Xm YYs)" heartbeat after 30 s.
    #[serde(default)]
    pub typing_status_text: Option<String>,
}

/// A downloaded attachment cached in `<home>/media-cache/` (hermes media
/// pipeline input).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MediaAttachment {
    pub path: std::path::PathBuf,
    pub mime: String,
    pub bytes: u64,
    pub original_name: String,
}

/// Normalized incoming message (hermes `MessageEvent`, core fields).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MessageEvent {
    pub platform: String,
    pub chat_id: String,
    pub sender_id: String,
    pub sender_name: String,
    pub text: String,
    pub message_id: String,
    /// Cached media attachments (empty when the message had none or the
    /// download failed — failures degrade to a text note, never fatal).
    pub attachments: Vec<MediaAttachment>,
}

/// Process-wide chat-session remap (hermes switch_session semantics):
/// platform chat session key → session id to run under. Shared between
/// the messaging Dispatcher (`/resume`) and the gateway (`/handoff`) —
/// both surfaces can bind a chat to another session.
fn session_remappings() -> &'static std::sync::Mutex<HashMap<String, String>> {
    static REMAP: std::sync::OnceLock<std::sync::Mutex<HashMap<String, String>>> =
        std::sync::OnceLock::new();
    REMAP.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// Bind a platform chat session key to a session id (hermes
/// switch_session; used by `/resume` and `/handoff`).
pub fn set_session_remap(session_key: &str, session_id: &str) {
    session_remappings()
        .lock()
        .unwrap()
        .insert(session_key.to_string(), session_id.to_string());
}

/// Session id the chat key currently runs under — the remapped session
/// when one is bound, else the deterministic chat key itself.
pub fn effective_session_id_for(session_key: &str) -> String {
    session_remappings()
        .lock()
        .unwrap()
        .get(session_key)
        .cloned()
        .unwrap_or_else(|| session_key.to_string())
}

/// Per-chat rolling history (mirrors the REPL continuity model) —
/// process-global so gateway `/handoff` can evict a chat's cache when
/// rebinding it to another session.
fn chat_histories() -> &'static Mutex<HashMap<String, Vec<crate::provider::Message>>> {
    static HISTORIES: std::sync::OnceLock<Mutex<HashMap<String, Vec<crate::provider::Message>>>> =
        std::sync::OnceLock::new();
    HISTORIES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Evict a chat's cached history so the next turn rebuilds from the
/// (possibly remapped) session transcript (hermes agent-cache eviction).
pub async fn drop_history_cache(session_key: &str) {
    chat_histories().lock().await.remove(session_key);
}

/// Test hook: clear the process-wide remap table.
#[cfg(test)]
pub(crate) fn clear_session_remappings_for_tests() {
    session_remappings().lock().unwrap().clear();
}

/// Test hook: the remap table is process-global, so tests that clear
/// or rewrite entries race when run in parallel — serialize them.
#[cfg(test)]
pub(crate) fn remap_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Per-chat conversation runner: one session per platform+chat
/// (hermes gateway session routing).
/// P710: RAII release of a turn lease — every run_turn exit path
/// (including early returns and error unwinds) frees the lease, and
/// release is ownership-checked + idempotent (hermes turn_lease).
struct TurnLeaseGuard {
    registry: Arc<crate::turn_lease::SessionTurnLeaseRegistry>,
    token: Option<Arc<crate::turn_lease::TurnLeaseToken>>,
}

impl Drop for TurnLeaseGuard {
    fn drop(&mut self) {
        if let Some(token) = self.token.take() {
            self.registry.release(&token);
        }
    }
}

/// P714: stamp mid-turn agent progress into the shared activity
/// registry (hermes `get_activity_summary()` as the stall watcher's
/// single progress source). Messaging agents are dedicated per
/// adapter, so installing callbacks never displaces TUI wiring; the
/// stamps read the current chat from the dispatch task-local.
fn install_activity_callbacks(agent: &Agent) {
    agent.try_set_callbacks(crate::agent::AgentCallbacks {
        on_activity: Some(Box::new(|description: &str| {
            if let Some(ctx) = current_messaging_ctx() {
                crate::session_activity::touch(&ctx.session_key, description, "agent.progress");
            }
        })),
        ..Default::default()
    });
}

pub struct Dispatcher {
    agent: Arc<Agent>,
    store: Arc<SqliteSessionStore>,
    /// Per-chat in-flight guard: one turn at a time per chat.
    busy: Arc<Mutex<HashMap<String, bool>>>,
    /// Per-chat FIFO of messages that arrived while a turn was busy
    /// (hermes busy-policy queue parity) — drained after each turn.
    queued: Arc<Mutex<HashMap<String, std::collections::VecDeque<MessageEvent>>>>,
    /// P709: inbound profile routing (hermes `_profile_name_for_source`
    /// + multiplexer): when set, events matching a configured route are
    /// delegated to that profile's dispatcher before any local state
    /// (busy/queue/sessions) is touched.
    profile_routing: Option<Arc<ProfileRoutingHub>>,
    /// P710: per-session turn leases (hermes turn_lease, #64934):
    /// serializes turns that share a session_id across routing keys.
    turn_leases: Arc<crate::turn_lease::SessionTurnLeaseRegistry>,
    /// Monotonic turn generation for lease ownership diagnostics.
    turn_generation: std::sync::atomic::AtomicU64,
}

/// P709: factory that builds a profile's messaging dispatcher on
/// first routed use (hermes multiplexer profile-runtime laziness).
/// The returned dispatcher owns the profile's agent + session store
/// (`<home>/profiles/<name>`), mirroring the `/p/<profile>` HTTP
/// stack isolation. A trait (not a type alias) so the
/// factory→Dispatcher→hub cycle stays behind concrete types.
pub trait ProfileDispatcherFactory: Send + Sync {
    fn build(
        &self,
        profile: String,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = std::result::Result<Arc<Dispatcher>, String>> + Send>,
    >;
}

/// Closure adapter so callers can supply a plain async closure.
impl<F, Fut> ProfileDispatcherFactory for F
where
    F: Fn(String) -> Fut + Send + Sync,
    Fut:
        std::future::Future<Output = std::result::Result<Arc<Dispatcher>, String>> + Send + 'static,
{
    fn build(
        &self,
        profile: String,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = std::result::Result<Arc<Dispatcher>, String>> + Send>,
    > {
        Box::pin(self(profile))
    }
}

/// P709: inbound profile-routing hub (hermes `gateway.profile_routes`
/// + `_profile_name_for_source` parity): matches events against the
/// configured routes (most specific first) and lazily caches one
/// dispatcher per routed profile.
pub struct ProfileRoutingHub {
    routes: Vec<crate::profile_routing::ProfileRoute>,
    factory: Arc<dyn ProfileDispatcherFactory>,
    cache: tokio::sync::Mutex<HashMap<String, Arc<Dispatcher>>>,
}

impl ProfileRoutingHub {
    pub fn new(
        routes: Vec<crate::profile_routing::ProfileRoute>,
        factory: Arc<dyn ProfileDispatcherFactory>,
    ) -> Arc<Self> {
        Arc::new(Self {
            routes,
            factory,
            cache: tokio::sync::Mutex::new(HashMap::new()),
        })
    }

    /// Profile the event routes to, if any route matches (hermes
    /// `match_profile_route` over the event's source fields).
    pub fn match_event(&self, platform: &str, chat_id: &str) -> Option<String> {
        crate::profile_routing::match_profile_route(
            &self.routes,
            platform,
            None,
            Some(chat_id),
            None,
            None,
        )
        .map(|route| route.profile.clone())
    }

    /// The profile's dispatcher, built on first use (build failures
    /// are logged and surface as None so the caller falls back to the
    /// default profile — routing must never drop a message).
    pub async fn dispatcher_for(&self, profile: &str) -> Option<Arc<Dispatcher>> {
        {
            let cache = self.cache.lock().await;
            if let Some(dispatcher) = cache.get(profile) {
                return Some(dispatcher.clone());
            }
        }
        let built = match self.factory.build(profile.to_string()).await {
            Ok(dispatcher) => dispatcher,
            Err(e) => {
                tracing::warn!("[messaging] profile '{profile}' dispatcher build failed: {e}");
                return None;
            }
        };
        let mut cache = self.cache.lock().await;
        Some(cache.entry(profile.to_string()).or_insert(built).clone())
    }
}

/// Result of one dispatched platform message (hermes run-turn output):
/// the agent reply plus any 🎙️ transcript echoes to deliver first.
#[derive(Debug, Clone, Default)]
pub struct DispatchOutcome {
    pub reply: String,
    pub transcript_echoes: Vec<String>,
}

// ---------------------------------------------------------------------------
// Messaging turn context + platform senders (clarify gateway integration)
// ---------------------------------------------------------------------------

/// The platform chat a messaging turn belongs to. Carried as a tokio
/// task-local so the messaging-aware clarify callback can render prompts
/// back to the right chat without threading platform state through the
/// tool layer.
#[derive(Debug, Clone)]
pub struct PlatformChatRef {
    pub platform: String,
    pub chat_id: String,
    pub session_key: String,
}

tokio::task_local! {
    static MESSAGING_CTX: PlatformChatRef;
}

/// The current turn's platform chat, when running inside a messaging
/// dispatch task.
pub fn current_messaging_ctx() -> Option<PlatformChatRef> {
    MESSAGING_CTX.try_with(|c| c.clone()).ok()
}

/// Outbound channel for a platform adapter, registered when the platform
/// loop starts. Used by the clarify gateway to render prompts mid-turn.
#[async_trait::async_trait]
pub trait PlatformSender: Send + Sync {
    async fn send_text(&self, chat_id: &str, text: &str);
    /// Render a clarify prompt natively (WhatsApp buttons/list). Returns
    /// false when the platform has no native interactive support — the
    /// caller falls back to numbered text.
    async fn send_clarify(
        &self,
        _chat_id: &str,
        _clarify_id: &str,
        _question: &str,
        _choices: &[String],
    ) -> bool {
        false
    }
    /// Render an exec-approval prompt natively (Teams adaptive-card
    /// buttons). Returns false when the platform has no native
    /// interactive support — the caller falls back to `/approve` text.
    #[allow(clippy::too_many_arguments)]
    async fn send_exec_approval(
        &self,
        _chat_id: &str,
        _command: &str,
        _session_key: &str,
        _description: &str,
        _allow_permanent: bool,
        _allow_session: bool,
        _smart_denied: bool,
    ) -> bool {
        false
    }
    /// Render a slash-command confirmation natively (hermes
    /// `send_slash_confirm`: Approve Once / Always Approve / Cancel
    /// buttons). Button callbacks must route to
    /// `crate::slash_confirm::resolve(session_key, confirm_id, choice)`.
    /// Returns false when the platform has no native interactive support —
    /// the caller falls back to `/approve` text.
    async fn send_slash_confirm(
        &self,
        _chat_id: &str,
        _title: &str,
        _message: &str,
        _session_key: &str,
        _confirm_id: &str,
    ) -> bool {
        false
    }
    /// Attach (or with `remove` retract) an emoji reaction — hermes
    /// `send_message(action='react'/'unreact')` adapter hooks
    /// (`add_reaction` / `remove_reaction`). `None` = platform has no
    /// reaction support; `Some(ok)` = API outcome.
    async fn send_reaction(
        &self,
        _chat_id: &str,
        _emoji: &str,
        _message_id: &str,
        _remove: bool,
    ) -> Option<bool> {
        None
    }
    /// Deliver media files natively with an optional accompanying text
    /// (hermes per-platform media delivery in the send_message path).
    /// Returns false when the platform has no native media path — the
    /// caller falls back to a text description.
    async fn send_media(&self, _chat_id: &str, _text: &str, _paths: &[std::path::PathBuf]) -> bool {
        false
    }
}

fn platform_senders() -> &'static std::sync::Mutex<HashMap<String, Arc<dyn PlatformSender>>> {
    static SENDERS: std::sync::OnceLock<
        std::sync::Mutex<HashMap<String, Arc<dyn PlatformSender>>>,
    > = std::sync::OnceLock::new();
    SENDERS.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

pub fn register_platform_sender(platform: &str, sender: Arc<dyn PlatformSender>) {
    platform_senders()
        .lock()
        .unwrap()
        .insert(platform.to_string(), sender);
    // Sender registration is the loop's "I'm live" signal.
    set_platform_lifecycle(platform, "running");
}

/// Process-wide platform-loop lifecycle states (`starting` → `running`
/// → `exited`) — crash visibility for the `/platforms` digest (lean
/// groundwork for hermes `/platform` retry-queue controls).
fn platform_lifecycles() -> &'static std::sync::Mutex<HashMap<String, String>> {
    static LIFECYCLES: std::sync::OnceLock<std::sync::Mutex<HashMap<String, String>>> =
        std::sync::OnceLock::new();
    LIFECYCLES.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// Record a platform loop state (`starting` / `running` / `exited`).
pub fn set_platform_lifecycle(platform: &str, state: &str) {
    platform_lifecycles()
        .lock()
        .unwrap()
        .insert(platform.to_string(), state.to_string());
}

/// Last recorded loop state for a platform, if any.
pub fn platform_lifecycle(platform: &str) -> Option<String> {
    platform_lifecycles().lock().unwrap().get(platform).cloned()
}

/// Test hook: clear the lifecycle table.
#[cfg(test)]
fn clear_platform_lifecycles_for_tests() {
    platform_lifecycles().lock().unwrap().clear();
}

/// Retry policy for crashed platform loops (lean hermes
/// reconnect-watcher parity): bounded attempts with a fixed backoff.
const MAX_PLATFORM_RETRIES: usize = 3;
const PLATFORM_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(5);

/// Pause flags for platform retries (hermes `/platform pause|resume`).
fn platform_pauses() -> &'static std::sync::Mutex<HashMap<String, bool>> {
    static PAUSES: std::sync::OnceLock<std::sync::Mutex<HashMap<String, bool>>> =
        std::sync::OnceLock::new();
    PAUSES.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// Retry-attempt counters (the `/platform list` digest shows them).
fn platform_retry_counts() -> &'static std::sync::Mutex<HashMap<String, usize>> {
    static COUNTS: std::sync::OnceLock<std::sync::Mutex<HashMap<String, usize>>> =
        std::sync::OnceLock::new();
    COUNTS.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// Pause retrying for a platform (takes effect when the loop next
/// exits; a loop already waiting in the retry poll wakes up).
pub fn pause_platform_retries(platform: &str) {
    platform_pauses()
        .lock()
        .unwrap()
        .insert(platform.to_string(), true);
}

/// Clear a pause flag so the retry poll proceeds.
pub fn resume_platform_retries(platform: &str) {
    platform_pauses().lock().unwrap().remove(platform);
}

pub fn platform_retries_paused(platform: &str) -> bool {
    platform_pauses()
        .lock()
        .unwrap()
        .get(platform)
        .copied()
        .unwrap_or(false)
}

pub fn platform_retry_count(platform: &str) -> usize {
    platform_retry_counts()
        .lock()
        .unwrap()
        .get(platform)
        .copied()
        .unwrap_or(0)
}

/// Spawn one platform loop with lifecycle + retry bookkeeping:
/// `starting` → `running` (sender registration) → on exit, retry up to
/// [`MAX_PLATFORM_RETRIES`] times (state `retrying`, honoring pause
/// flags) before settling on `exited` (hermes reconnect-watcher lean
/// parity). The factory rebuilds the loop future on every attempt.
fn spawn_platform_task<F, Fut>(
    tasks: &mut Vec<tokio::task::JoinHandle<()>>,
    platform: &'static str,
    factory: F,
) where
    F: Fn() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    set_platform_lifecycle(platform, "starting");
    tasks.push(tokio::spawn(async move {
        let mut attempt = 0usize;
        loop {
            factory().await;
            // Paused before the retry decision: park until resumed.
            if platform_retries_paused(platform) {
                set_platform_lifecycle(platform, "paused");
                while platform_retries_paused(platform) {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
            }
            if attempt >= MAX_PLATFORM_RETRIES {
                break;
            }
            attempt += 1;
            platform_retry_counts()
                .lock()
                .unwrap()
                .insert(platform.to_string(), attempt);
            set_platform_lifecycle(platform, "retrying");
            tokio::time::sleep(PLATFORM_RETRY_DELAY).await;
            if platform_retries_paused(platform) {
                set_platform_lifecycle(platform, "paused");
                while platform_retries_paused(platform) {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
            }
            set_platform_lifecycle(platform, "starting");
        }
        set_platform_lifecycle(platform, "exited");
    }));
}

pub fn platform_sender(platform: &str) -> Option<Arc<dyn PlatformSender>> {
    platform_senders().lock().unwrap().get(platform).cloned()
}

/// Test hook: remove a registered sender (the registry is process-wide
/// and tests share it).
#[cfg(test)]
pub fn unregister_platform_sender_for_tests(platform: &str) {
    platform_senders().lock().unwrap().remove(platform);
}

/// Whether any platform adapter has registered a live sender (hermes
/// `is_gateway_running` gate for the send_message tool).
pub fn has_platform_senders() -> bool {
    !platform_senders().lock().unwrap().is_empty()
}

/// Names of the platforms with live senders (send_message diagnostics).
pub fn platform_sender_names() -> Vec<String> {
    let mut names: Vec<String> = platform_senders().lock().unwrap().keys().cloned().collect();
    names.sort();
    names
}

/// P714: one parked inbound message visible to the stall watcher
/// (hermes `_pending_messages` observation surface).
#[derive(Debug, Clone, serde::Serialize)]
pub struct PendingInboundRow {
    pub session_key: String,
    pub platform: String,
    pub chat_id: String,
    pub queued_at: f64,
}

/// P735: process-wide registry of live dispatchers so the gateway
/// shutdown path can flush every parked queue (hermes shutdown_flush).
/// Weak refs: test-created dispatchers drop out automatically.
fn dispatcher_registry() -> &'static std::sync::Mutex<Vec<std::sync::Weak<Dispatcher>>> {
    static REGISTRY: std::sync::OnceLock<std::sync::Mutex<Vec<std::sync::Weak<Dispatcher>>>> =
        std::sync::OnceLock::new();
    REGISTRY.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

/// P735: drain every live dispatcher's parked queue, returning the
/// events with their session keys (hermes `_pending_messages` snapshot
/// before shutdown clear).
pub async fn take_all_parked() -> Vec<(String, MessageEvent)> {
    let weaks: Vec<std::sync::Weak<Dispatcher>> = {
        let mut registry = dispatcher_registry().lock().unwrap();
        registry.retain(|w| w.upgrade().is_some());
        registry.clone()
    };
    let mut all = Vec::new();
    for weak in weaks {
        if let Some(dispatcher) = weak.upgrade() {
            all.extend(dispatcher.take_parked().await);
        }
    }
    all
}

fn pending_inbound_directory() -> &'static std::sync::Mutex<HashMap<String, PendingInboundRow>> {
    static DIRECTORY: std::sync::OnceLock<std::sync::Mutex<HashMap<String, PendingInboundRow>>> =
        std::sync::OnceLock::new();
    DIRECTORY.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// Publish a parked follow-up to the pending-inbound directory (hermes
/// `_pending_messages` visibility for the session stall watcher).
pub fn register_pending_inbound(session_key: &str, platform: &str, chat_id: &str) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    pending_inbound_directory().lock().unwrap().insert(
        session_key.to_string(),
        PendingInboundRow {
            session_key: session_key.to_string(),
            platform: platform.to_string(),
            chat_id: chat_id.to_string(),
            queued_at: now,
        },
    );
}

/// Drop a session's pending-inbound row when its queue drains.
pub fn unregister_pending_inbound(session_key: &str) {
    pending_inbound_directory()
        .lock()
        .unwrap()
        .remove(session_key);
}

/// Every session with parked inbound messages (stall watcher scan).
pub fn pending_inbound_snapshot() -> Vec<PendingInboundRow> {
    pending_inbound_directory()
        .lock()
        .unwrap()
        .values()
        .cloned()
        .collect()
}

/// Fresh re-check of one session's pending state right before a stall
/// notice goes out (hermes #76354 review S2).
pub fn pending_inbound_contains(session_key: &str) -> bool {
    pending_inbound_directory()
        .lock()
        .unwrap()
        .contains_key(session_key)
}

/// Test-only full clear (the directory is process-wide).
#[doc(hidden)]
pub fn clear_pending_inbound_for_tests() {
    pending_inbound_directory().lock().unwrap().clear();
}

/// Announce a gateway restart to each live platform's most recently
/// active channel (hermes `gateway_restart_notification` parity).
/// Runs after a short settle delay so platform senders finish
/// registering and the channel directory reflects the previous run.
pub async fn announce_gateway_restart(settle: std::time::Duration) {
    tokio::time::sleep(settle).await;
    for platform in platform_sender_names() {
        let Some(sender) = platform_sender(&platform) else {
            continue;
        };
        let Some((_channel_platform, entry)) =
            crate::channel_directory::list_channels(Some(&platform))
                .into_iter()
                .max_by_key(|(_, entry)| entry.updated_at)
        else {
            continue;
        };
        sender.send_text(&entry.id, "♻ Gateway restarted").await;
    }
}

/// One messaging-platform catalog row (lean hermes
/// `_messaging_platform_catalog` parity): static metadata for the
/// dashboard Channels surface. `env_keys` lists the env vars the
/// adapter actually honors (telegram/discord/slack fallbacks); the
/// remaining platforms are config.toml-driven, so their cards carry no
/// env rows. `required_any` encodes the configured rule: every group
/// needs at least one non-empty field in `[messaging.<id>]`.
#[derive(Debug, Clone, Copy)]
pub struct PlatformCatalogEntry {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub env_keys: &'static [(&'static str, bool)],
    pub required_any: &'static [&'static [&'static str]],
}

/// The messaging platform catalog (hermes ChannelsPage inventory).
/// Ordering mirrors `connected_messaging_platforms` callers' display
/// order (the `/api/channels` list).
pub fn platform_catalog() -> Vec<PlatformCatalogEntry> {
    vec![
        PlatformCatalogEntry {
            id: "telegram",
            name: "Telegram",
            description: "Bot API long-polling adapter",
            env_keys: &[("TELEGRAM_BOT_TOKEN", true)],
            required_any: &[&["bot_token"]],
        },
        PlatformCatalogEntry {
            id: "discord",
            name: "Discord",
            description: "Gateway v10 websocket + REST adapter",
            env_keys: &[("DISCORD_BOT_TOKEN", true)],
            required_any: &[&["bot_token"]],
        },
        PlatformCatalogEntry {
            id: "slack",
            name: "Slack",
            description: "Socket Mode websocket + chat.postMessage adapter",
            env_keys: &[("SLACK_BOT_TOKEN", true), ("SLACK_APP_TOKEN", true)],
            required_any: &[&["bot_token"], &["app_token"]],
        },
        PlatformCatalogEntry {
            id: "signal",
            name: "Signal",
            description: "signal-cli HTTP daemon adapter",
            env_keys: &[],
            required_any: &[&["http_url"], &["account"]],
        },
        PlatformCatalogEntry {
            id: "weixin",
            name: "Weixin",
            description: "Weixin personal account via the iLink Bot API",
            env_keys: &[],
            required_any: &[&["token"], &["base_url"]],
        },
        PlatformCatalogEntry {
            id: "qq",
            name: "QQ",
            description: "Official QQ Bot API v2 adapter",
            env_keys: &[],
            required_any: &[&["app_id"], &["client_secret"]],
        },
        PlatformCatalogEntry {
            id: "yuanbao",
            name: "Yuanbao",
            description: "Yuanbao WS-gateway adapter",
            env_keys: &[],
            required_any: &[&["app_id"], &["app_secret"]],
        },
        PlatformCatalogEntry {
            id: "email",
            name: "Email",
            description: "Email via IMAP/SMTP",
            env_keys: &[],
            required_any: &[&["password"]],
        },
        PlatformCatalogEntry {
            id: "mattermost",
            name: "Mattermost",
            description: "Mattermost REST v4 + WebSocket adapter",
            env_keys: &[],
            required_any: &[&["url"], &["token"]],
        },
        PlatformCatalogEntry {
            id: "matrix",
            name: "Matrix",
            description: "Matrix Client-Server API adapter (sans E2EE)",
            env_keys: &[],
            required_any: &[&["homeserver"], &["access_token", "password"]],
        },
        PlatformCatalogEntry {
            id: "dingtalk",
            name: "DingTalk",
            description: "DingTalk Stream Mode adapter",
            env_keys: &[],
            required_any: &[&["client_id"], &["client_secret"]],
        },
        PlatformCatalogEntry {
            id: "wecom",
            name: "WeCom",
            description: "WeCom AI Bot WebSocket gateway adapter",
            env_keys: &[],
            required_any: &[&["bot_id"], &["secret"]],
        },
        PlatformCatalogEntry {
            id: "feishu",
            name: "Feishu/Lark",
            description: "Feishu/Lark gateway webhook adapter",
            env_keys: &[],
            required_any: &[&["app_id"], &["app_secret"]],
        },
        PlatformCatalogEntry {
            id: "homeassistant",
            name: "Home Assistant",
            description: "Home Assistant WS API state-change events",
            env_keys: &[],
            required_any: &[&["url"], &["token"]],
        },
        PlatformCatalogEntry {
            id: "sms",
            name: "SMS (Twilio)",
            description: "Twilio SMS via REST + gateway webhook",
            env_keys: &[],
            required_any: &[&["account_sid"], &["auth_token"]],
        },
        PlatformCatalogEntry {
            id: "whatsapp",
            name: "WhatsApp",
            description: "WhatsApp via an external Baileys HTTP bridge",
            env_keys: &[],
            required_any: &[&["bridge_url"]],
        },
        PlatformCatalogEntry {
            id: "irc",
            name: "IRC",
            description: "IRC via a zero-dependency TLS client",
            env_keys: &[],
            required_any: &[&["server"], &["nickname"]],
        },
        PlatformCatalogEntry {
            id: "ntfy",
            name: "ntfy",
            description: "ntfy topics via HTTP streaming",
            env_keys: &[],
            required_any: &[&["server"], &["topic"]],
        },
        PlatformCatalogEntry {
            id: "simplex",
            name: "SimpleX",
            description: "SimpleX via the simplex-chat daemon WS API",
            env_keys: &[],
            required_any: &[&["ws_url"]],
        },
        PlatformCatalogEntry {
            id: "teams",
            name: "Microsoft Teams",
            description: "Microsoft Teams via the raw Bot Framework protocol",
            env_keys: &[],
            required_any: &[&["client_id"], &["client_secret"]],
        },
        PlatformCatalogEntry {
            id: "line",
            name: "LINE",
            description: "LINE Messaging API adapter",
            env_keys: &[],
            required_any: &[&["channel_access_token"], &["channel_secret"]],
        },
        PlatformCatalogEntry {
            id: "google_chat",
            name: "Google Chat",
            description: "Google Chat service-account HTTP events adapter",
            env_keys: &[],
            required_any: &[&["service_account_file"]],
        },
        PlatformCatalogEntry {
            id: "buzz",
            name: "Buzz",
            description: "Buzz CLI bridge adapter",
            env_keys: &[],
            required_any: &[&["cli_path"], &["self_pubkey"]],
        },
        PlatformCatalogEntry {
            id: "photon",
            name: "Photon",
            description: "Photon sidecar bridge adapter",
            env_keys: &[],
            required_any: &[&["sidecar_url"]],
        },
        PlatformCatalogEntry {
            id: "raft",
            name: "Raft",
            description: "Raft bridge runtime adapter",
            env_keys: &[],
            required_any: &[&["bridge_token"]],
        },
        PlatformCatalogEntry {
            id: "a2a",
            name: "A2A",
            description: "Agent-to-agent protocol adapter",
            env_keys: &[],
            required_any: &[&["public_url"]],
        },
    ]
}

/// Look up one catalog entry by platform id.
pub fn platform_lookup(id: &str) -> Option<PlatformCatalogEntry> {
    platform_catalog().into_iter().find(|entry| entry.id == id)
}

/// Whether `[messaging.<id>]` carries the credentials the adapter
/// needs: every `required_any` group must have at least one non-empty
/// string field. Telegram/Discord/Slack also honor their env fallbacks.
pub fn platform_configured(section: &serde_json::Value, entry: &PlatformCatalogEntry) -> bool {
    let env_satisfies = |group: &[&str]| -> bool {
        entry.env_keys.iter().any(|(key, _)| {
            group.iter().any(|field| {
                key.ends_with(&format!("_{}", field.to_uppercase()))
                    && std::env::var(key)
                        .map(|value| !value.trim().is_empty())
                        .unwrap_or(false)
            })
        })
    };
    entry.required_any.iter().all(|group| {
        group.iter().any(|field| {
            section
                .get(field)
                .and_then(|v| v.as_str())
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false)
        }) || env_satisfies(group)
    })
}

/// Per-platform posture rows for the chat digest (P669 — hermes
/// `/platforms` parity): `(id, name, state)` where state is
/// `connected` / `not_configured` / `disabled`.
pub fn platform_state_rows() -> Vec<(&'static str, &'static str, String)> {
    let config = crate::config::UlncLawConfig::load(None).unwrap_or_default();
    let messaging_value = serde_json::to_value(&config.messaging).unwrap_or(Value::Null);
    platform_catalog()
        .into_iter()
        .map(|entry| {
            let section = messaging_value
                .get(entry.id)
                .cloned()
                .unwrap_or(Value::Null);
            let enabled = section
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let configured = platform_configured(&section, &entry);
            let state = if !enabled {
                "disabled"
            } else if !configured {
                "not_configured"
            } else if platform_sender(entry.id).is_some() {
                "connected"
            } else {
                // Enabled + configured but no live sender: surface the
                // loop lifecycle when this process tracks one (a crashed
                // loop reads "exited"); otherwise assume connected
                // (REPL /platforms runs without the gateway loops).
                match platform_lifecycle(entry.id).as_deref() {
                    Some("exited") => "exited",
                    Some("starting") => "starting",
                    Some("retrying") => "retrying",
                    Some("paused") => "paused",
                    _ => "connected",
                }
            };
            (entry.id, entry.name, state.to_string())
        })
        .collect()
}

/// Text digest of messaging-platform posture (P669 — the `/platforms`
/// slash, shared by REPL and gateway). Pure over the rows from
/// [`platform_state_rows`].
pub fn format_platforms_digest(rows: &[(&str, &str, String)]) -> String {
    if rows.is_empty() {
        return "(o_o) no messaging platforms in the catalog.\n".to_string();
    }
    let connected = rows.iter().filter(|r| r.2 == "connected").count();
    let mut out = String::new();
    out.push_str(&format!(
        "messaging platforms: {} of {} connected\n",
        connected,
        rows.len()
    ));
    for (id, name, state) in rows {
        let glyph = match state.as_str() {
            "connected" => "\u{2713}",
            "not_configured" => "\u{26a0}",
            "exited" => "\u{2717}",
            "retrying" => "\u{21bb}",
            "paused" => "\u{23f8}",
            _ => "\u{25cb}",
        };
        out.push_str(&format!("  {glyph} {:<14} {:<14} {id}\n", name, state));
    }
    out
}

/// hermes `/platform list` digest: connected platforms plus the
/// retry/pause queue (retrying attempts, paused, exited loops).
pub fn platform_ops_digest() -> String {
    let rows = platform_state_rows();
    let connected: Vec<&str> = rows
        .iter()
        .filter(|r| r.2 == "connected")
        .map(|r| r.0)
        .collect();
    let mut lines = vec!["**Gateway platforms**".to_string()];
    lines.push(if connected.is_empty() {
        "Connected: (none)".to_string()
    } else {
        format!("Connected: {}", connected.join(", "))
    });
    let mut issues = Vec::new();
    for (id, _name, state) in &rows {
        match state.as_str() {
            "paused" => issues.push(format!(
                "  \u{b7} {id} \u{2014} PAUSED. Resume with `/platform resume {id}`."
            )),
            "retrying" => issues.push(format!(
                "  \u{b7} {id} \u{2014} retrying (attempt {})",
                platform_retry_count(id)
            )),
            "exited" => issues.push(format!("  \u{b7} {id} \u{2014} exited (retries exhausted)")),
            "starting" => issues.push(format!("  \u{b7} {id} \u{2014} starting")),
            _ => {}
        }
    }
    if issues.is_empty() {
        lines.push("Failed/paused: (none)".to_string());
    } else {
        lines.extend(issues);
    }
    lines.join("\n")
}

/// `/platform list|pause|resume [name]` runner (hermes `/platform`
/// parity, shared by REPL and gateway).
pub fn run_platform_slash(rest: &str) -> String {
    let mut parts = rest.split_whitespace();
    let action = parts.next().unwrap_or("list").to_lowercase();
    let target = parts.next().map(|t| t.to_lowercase());
    match action.as_str() {
        "list" => platform_ops_digest(),
        "pause" => {
            let Some(target) = target else {
                return "Usage: /platform pause <name>".to_string();
            };
            if !platform_catalog().iter().any(|entry| entry.id == target) {
                return format!("Unknown platform: {target}");
            }
            pause_platform_retries(&target);
            format!(
                "\u{2713} {target} paused. Resume with `/platform resume {target}`."
            )
        }
        "resume" => {
            let Some(target) = target else {
                return "Usage: /platform resume <name>".to_string();
            };
            if !platform_catalog().iter().any(|entry| entry.id == target) {
                return format!("Unknown platform: {target}");
            }
            let was_paused = platform_retries_paused(&target);
            resume_platform_retries(&target);
            if was_paused {
                format!("\u{2713} {target} resumed \u{2014} restarting.")
            } else {
                format!("\u{2713} {target} resumed \u{2014} nothing was paused.")
            }
        }
        _ => "Usage: /platform <list|pause|resume> [name]\n  /platform list \u{2014} show platform status\n  /platform pause <name> \u{2014} stop retrying a failing platform\n  /platform resume <name> \u{2014} re-queue a paused platform"
            .to_string(),
    }
}

/// Parse a reply to a pending slash-confirm prompt (hermes gateway/run.py
/// intercept keyword table). Slash-command forms and plain text both work;
/// `!`-prefixed replies (Slack-style) are accepted verbatim.
pub fn parse_slash_confirm_reply(text: &str) -> Option<crate::slash_confirm::ConfirmChoice> {
    use crate::slash_confirm::ConfirmChoice;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(command) = trimmed.strip_prefix('/') {
        let first = command
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_lowercase();
        match first.as_str() {
            "approve" | "yes" | "ok" | "confirm" => return Some(ConfirmChoice::Once),
            "always" | "remember" => return Some(ConfirmChoice::Always),
            "cancel" | "no" | "deny" | "nevermind" => return Some(ConfirmChoice::Cancel),
            _ => {}
        }
    }
    let norm = trimmed.trim_start_matches(['!', '/']).trim().to_lowercase();
    match norm.as_str() {
        "approve" | "approve once" | "once" => Some(ConfirmChoice::Once),
        "always" | "always approve" => Some(ConfirmChoice::Always),
        "cancel" | "nevermind" | "no" => Some(ConfirmChoice::Cancel),
        _ => None,
    }
}

/// Plain-text slash-confirm prompt (hermes `_request_slash_confirm`
/// message shape): used on platforms without native buttons.
pub fn format_slash_confirm_prompt(command: &str, detail: &str) -> String {
    format!(
        "⚠️ **Confirm /{command}**\n\n{detail}\n\nChoose:\n         • **Approve Once** — proceed this time only\n         • **Always Approve** — proceed and silence this prompt permanently\n         • **Cancel** — keep things as they are\n\n         _Text fallback: reply `/approve`, `/always`, or `/cancel`._"
    )
}

fn next_slash_confirm_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::SeqCst).to_string()
}

/// Ask the user to confirm an expensive slash command (hermes
/// `_request_slash_confirm`). Registers the pending confirm FIRST (a
/// super-fast button click cannot race the send), then tries the
/// platform's native buttons; returns `None` when buttons rendered (the
/// buttons are the ack) or the text prompt to send as the direct reply.
pub async fn request_slash_confirm(
    platform: &str,
    chat_id: &str,
    session_key: &str,
    command: &str,
    title: &str,
    message: &str,
    handler: crate::slash_confirm::ConfirmHandler,
) -> Option<String> {
    let confirm_id = next_slash_confirm_id();
    crate::slash_confirm::register(session_key, &confirm_id, command, handler);
    if let Some(sender) = platform_sender(platform) {
        match sender
            .send_slash_confirm(chat_id, title, message, session_key, &confirm_id)
            .await
        {
            true => return None,
            false => {}
        }
    }
    Some(message.to_string())
}

/// Truncate on a char boundary for approval previews (hermes
/// `command[:200] + "..."`).
fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let cut: String = text.chars().take(max_chars).collect();
    format!("{cut}...")
}

/// Plain-text exec-approval prompt (hermes
/// `_format_exec_approval_fallback`): used on platforms without native
/// approval buttons and as the fallback when a card send fails.
pub fn format_approval_text(
    command: &str,
    description: &str,
    allow_session: bool,
    allow_permanent: bool,
    smart_denied: bool,
) -> String {
    let cmd_preview = truncate_chars(command, 200);
    let heading = if smart_denied {
        "⚠️ **Smart DENY — owner override for one operation:**"
    } else {
        "⚠️ **Dangerous command requires approval:**"
    };
    let mut choices: Vec<String> =
        vec!["Reply `/approve` to execute this one operation".to_string()];
    if !smart_denied && allow_session {
        choices.push("`/approve session` to approve this pattern for the session".to_string());
        if allow_permanent {
            choices.push("`/approve always` to approve permanently".to_string());
        }
    }
    choices.push("`/deny` to cancel".to_string());
    let last = choices.pop().expect("choices is never empty");
    format!(
        "{heading}\n```\n{cmd_preview}\n```\nReason: {description}\n\n{}, or {last}.",
        choices.join(", ")
    )
}

/// Parsed `/approve` / `/deny` chat command (hermes
/// `_handle_approve_command` / `_handle_deny_command`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApprovalCommand {
    pub approve: bool,
    pub all: bool,
    /// `once` | `session` | `always` for approve; `deny` for deny.
    pub choice: &'static str,
}

/// Parse a `/approve` / `/deny` message. Accepts the slash form on
/// every platform (hermes slash-command semantics).
pub fn parse_approval_command(text: &str) -> Option<ApprovalCommand> {
    let mut parts = text.trim().split_whitespace();
    let head = parts.next()?.to_lowercase();
    let rest: Vec<String> = parts.map(|p| p.to_lowercase()).collect();
    match head.as_str() {
        "/approve" => {
            let all = rest.iter().any(|p| p == "all");
            let choice = if rest.iter().any(|p| p == "always") {
                crate::approval_gateway::CHOICE_ALWAYS
            } else if rest.iter().any(|p| p == "session") {
                crate::approval_gateway::CHOICE_SESSION
            } else {
                crate::approval_gateway::CHOICE_ONCE
            };
            Some(ApprovalCommand {
                approve: true,
                all,
                choice,
            })
        }
        "/deny" => Some(ApprovalCommand {
            approve: false,
            all: rest.iter().any(|p| p == "all"),
            choice: crate::approval_gateway::CHOICE_DENY,
        }),
        _ => None,
    }
}

/// Apply a parsed approval command to the session's pending registry;
/// returns the user-facing confirmation (hermes approve/deny replies).
/// Returns `None` when nothing is pending and the message should be
/// treated as normal chat text.
pub fn apply_approval_command(session_key: &str, cmd: ApprovalCommand) -> String {
    if !crate::approval_gateway::has_blocking(session_key) {
        return "No pending approvals.".to_string();
    }
    let count = if cmd.all {
        crate::approval_gateway::resolve_all(session_key, cmd.choice)
    } else if crate::approval_gateway::resolve(session_key, cmd.choice) {
        1
    } else {
        0
    };
    if count == 0 {
        return "No pending approvals.".to_string();
    }
    let plural = if count == 1 { "command" } else { "commands" };
    let label = if cmd.approve {
        match cmd.choice {
            crate::approval_gateway::CHOICE_SESSION => "✅ Allowed (session)",
            crate::approval_gateway::CHOICE_ALWAYS => "✅ Always allowed",
            _ => "✅ Allowed (once)",
        }
    } else {
        "❌ Denied"
    };
    if count == 1 {
        label.to_string()
    } else {
        format!("{label} — {count} {plural}")
    }
}

/// Messaging-aware approve callback (hermes `tools/approval.py`
/// gateway-notify integration): inside a messaging turn the dangerous
/// command prompt renders on the platform (adaptive-card buttons where
/// supported, `/approve` text elsewhere) and blocks the agent until
/// the user taps or replies. Outside a messaging context the wrapped
/// callback (gateway run-based flow) decides.
pub fn messaging_aware_approve_fn(
    inner: crate::tools::context::ApproveFn,
) -> crate::tools::context::ApproveFn {
    Arc::new(move |reason, command| {
        let inner = inner.clone();
        Box::pin(async move {
            let Some(ctx) = current_messaging_ctx() else {
                return inner(reason, command).await;
            };
            // Redact credentials before the command reaches the chat
            // platform (hermes `_redact_approval_command`).
            let cmd = crate::redact::redact_sensitive_text(&command, Default::default());
            let handle = crate::approval_gateway::register(
                &ctx.session_key,
                &cmd,
                &reason,
                false,
                true,
                true,
            );
            let rendered = if let Some(sender) = platform_sender(&ctx.platform) {
                let native = sender
                    .send_exec_approval(
                        &ctx.chat_id,
                        &cmd,
                        &ctx.session_key,
                        &reason,
                        true,
                        true,
                        false,
                    )
                    .await;
                if !native {
                    sender
                        .send_text(
                            &ctx.chat_id,
                            &format_approval_text(&cmd, &reason, true, true, false),
                        )
                        .await;
                }
                true
            } else {
                false
            };
            if !rendered {
                // No outbound channel: fail closed.
                crate::approval_gateway::resolve(
                    &ctx.session_key,
                    crate::approval_gateway::CHOICE_DENY,
                );
                return false;
            }
            matches!(handle.rx.await, Ok(choice) if choice != crate::approval_gateway::CHOICE_DENY)
        })
    })
}

/// Numbered-text clarify rendering for platforms without native
/// interactive messages.
pub fn format_clarify_text(question: &str, choices: &[String], multi_select: bool) -> String {
    let mut out = format!("❓ {}", question.trim());
    if !choices.is_empty() {
        out.push_str("\n\n");
        for (i, choice) in choices.iter().enumerate() {
            out.push_str(&format!("{}. {}\n", i + 1, choice.trim()));
        }
        out.push_str(if multi_select {
            "\nReply with the numbers of all choices that apply."
        } else {
            "\nReply with the number of your choice (or type your own answer)."
        });
    }
    out
}

/// Messaging-aware clarify callback for gateway agents (hermes
/// `tools/clarify_gateway.py` integration): renders the prompt on the
/// current platform (native buttons on WhatsApp, numbered text
/// elsewhere) and blocks the tool until the user taps or replies. Runs
/// without a messaging context (plain `/api/chat`, cron, one-shot)
/// report the standard non-interactive error.
pub fn messaging_clarify_fn() -> crate::tools::context::ClarifyFn {
    Arc::new(|question, choices, multi| {
        Box::pin(async move {
            let Some(ctx) = current_messaging_ctx() else {
                return Err(AgentError::tool(
                    "No user is available to answer (non-interactive session). Proceed with \
                     your best judgment and state your assumptions in the final answer.",
                ));
            };
            let handle =
                crate::clarify_gateway::register(&ctx.session_key, &question, &choices, multi);
            let Some(sender) = platform_sender(&ctx.platform) else {
                crate::clarify_gateway::resolve(&handle.clarify_id, "");
                return Err(AgentError::tool(format!(
                    "clarify: platform '{}' has no registered sender",
                    ctx.platform
                )));
            };
            let mut rendered_natively = false;
            if !choices.is_empty() {
                rendered_natively = sender
                    .send_clarify(&ctx.chat_id, &handle.clarify_id, &question, &choices)
                    .await;
            }
            if !rendered_natively {
                let text = if choices.is_empty() {
                    format!("❓ {}", question.trim())
                } else {
                    format_clarify_text(&question, &choices, multi)
                };
                sender.send_text(&ctx.chat_id, &text).await;
            }
            match handle.rx.await {
                Ok(answer) if !answer.is_empty() => Ok(answer),
                Ok(_) => Err(AgentError::tool("clarify: no answer received")),
                Err(_) => Err(AgentError::tool(
                    "clarify was cancelled (gateway restart or session end)",
                )),
            }
        })
    })
}

impl Dispatcher {
    pub fn new(agent: Arc<Agent>, store: Arc<SqliteSessionStore>) -> Arc<Self> {
        install_activity_callbacks(&agent);
        let dispatcher = Arc::new(Self {
            agent,
            store,
            busy: Arc::new(Mutex::new(HashMap::new())),
            queued: Arc::new(Mutex::new(HashMap::new())),
            profile_routing: None,
            turn_leases: Arc::new(crate::turn_lease::SessionTurnLeaseRegistry::new(
                crate::turn_lease::DEFAULT_MAX_LEASES,
            )),
            turn_generation: std::sync::atomic::AtomicU64::new(0),
        });
        // P735: expose to the shutdown-flush sweep.
        dispatcher_registry()
            .lock()
            .unwrap()
            .push(Arc::downgrade(&dispatcher));
        dispatcher
    }

    /// P709: build the default dispatcher with an inbound
    /// profile-routing hub (hermes `gateway.profile_routes` under
    /// multiplexing). Profile dispatchers built by the hub use plain
    /// [`Dispatcher::new`], so routing never recurses.
    pub fn new_with_routing(
        agent: Arc<Agent>,
        store: Arc<SqliteSessionStore>,
        hub: Arc<ProfileRoutingHub>,
    ) -> Arc<Self> {
        install_activity_callbacks(&agent);
        let dispatcher = Arc::new(Self {
            agent,
            store,
            busy: Arc::new(Mutex::new(HashMap::new())),
            queued: Arc::new(Mutex::new(HashMap::new())),
            profile_routing: Some(hub),
            turn_leases: Arc::new(crate::turn_lease::SessionTurnLeaseRegistry::new(
                crate::turn_lease::DEFAULT_MAX_LEASES,
            )),
            turn_generation: std::sync::atomic::AtomicU64::new(0),
        });
        // P735: expose to the shutdown-flush sweep.
        dispatcher_registry()
            .lock()
            .unwrap()
            .push(Arc::downgrade(&dispatcher));
        dispatcher
    }

    /// P735: drain this dispatcher's parked queues (the busy-policy
    /// FIFOs), unregistering their pending-inbound rows. Called by the
    /// gateway shutdown sweep so no parked follow-up is lost (hermes
    /// shutdown_flush `_pending_messages` parity).
    pub async fn take_parked(&self) -> Vec<(String, MessageEvent)> {
        let mut taken = Vec::new();
        let mut queued = self.queued.lock().await;
        for (key, queue) in queued.drain() {
            for event in queue {
                taken.push((key.clone(), event));
            }
            unregister_pending_inbound(&key);
        }
        taken
    }

    fn session_key(event: &MessageEvent) -> String {
        format!("platform-{}-{}", event.platform, event.chat_id)
    }

    /// P718: hermes `_check_slash_access` — resolve the platform +
    /// scope policy and deny non-admins commands outside their
    /// allowlist. Unknown `/…` text is never gated (users legitimately
    /// send paths); skill/bundle slashes stay ungated agent turns.
    fn slash_gate_denial(&self, event: &MessageEvent) -> Option<String> {
        let cmd_word = event.text.trim().split_whitespace().next()?;
        let canonical = crate::slash_access::normalize_command(cmd_word);
        if !crate::slash_access::is_known_command(&canonical) {
            return None;
        }
        let chat_type = crate::channel_directory::chat_type_for(&event.platform, &event.chat_id);
        let scope = crate::slash_access::scope_for_chat_type(chat_type.as_deref());
        let policy = crate::slash_access::policy_for(
            self.agent
                .context()
                .config
                .messaging
                .slash_access
                .get(&event.platform),
            scope,
        );
        if policy.can_run(Some(&event.sender_id), &canonical) {
            return None;
        }
        tracing::info!(
            "slash command /{canonical} denied for {}:{} (not admin, not in user_allowed_commands)",
            event.platform,
            event.sender_id,
        );
        Some(crate::slash_access::denial_message(&canonical, &policy))
    }

    /// Run one agent turn for the event's chat; returns the reply text
    /// plus transcript echoes (hermes `_echo_pending_stt_transcripts_once`).
    pub async fn handle_event(self: &Arc<Self>, event: MessageEvent) -> Result<DispatchOutcome> {
        // P709: inbound profile routing (hermes
        // `_profile_name_for_source`): an event matching a configured
        // route is delegated wholesale to that profile's dispatcher —
        // session, busy/queue state and approvals all live under the
        // profile's own agent/store. A failed profile build falls
        // back to the default profile; routing never drops a message.
        if let Some(hub) = self.profile_routing.as_ref() {
            if let Some(profile) = hub.match_event(&event.platform, &event.chat_id) {
                if let Some(profile_dispatcher) = hub.dispatcher_for(&profile).await {
                    // Boxed: handle_event recurses through routing (a
                    // recursive async fn needs indirection).
                    return Box::pin(profile_dispatcher.handle_event(event)).await;
                }
                tracing::warn!(
                    "[messaging] profile '{profile}' unavailable for {}:{} — using default profile",
                    event.platform,
                    event.chat_id
                );
            }
        }
        let key = Self::session_key(&event);
        // Channel directory observation (hermes gateway directory build):
        // every inbound event keeps the send_message target list fresh.
        crate::channel_directory::record_channel(
            &event.platform,
            &event.chat_id,
            &event.sender_name,
            "",
            &event.message_id,
        );
        // P718: per-platform slash access control (hermes
        // slash_access) — of the users allowed to talk, which can run
        // which commands. Applied before every command intercept so
        // gating can't be bypassed; plain chat and unknown /text are
        // unaffected.
        if let Some(denied) = self.slash_gate_denial(&event) {
            return Ok(DispatchOutcome {
                reply: denied,
                transcript_echoes: Vec::new(),
            });
        }
        // P737: command hooks (hermes `command:<canonical>`
        // emit_collect with the decision protocol): fired after access
        // control, before core handling; deny/handled/rewrite intercept
        // dispatch exactly like hermes.
        let mut event = event;
        if let Some(cmd_word) = event.text.trim().split_whitespace().next() {
            let canonical = crate::slash_access::normalize_command(cmd_word);
            if crate::slash_access::is_known_command(&canonical) {
                let raw_args = event
                    .text
                    .trim()
                    .splitn(2, ' ')
                    .nth(1)
                    .unwrap_or("")
                    .trim()
                    .to_string();
                let hook_ctx = serde_json::json!({
                    "platform": event.platform,
                    "user_id": event.sender_id,
                    "command": canonical,
                    "raw_command": cmd_word,
                    "args": raw_args,
                    "raw_args": raw_args,
                });
                let hook_results =
                    crate::event_hooks::emit_collect(&format!("command:{canonical}"), hook_ctx)
                        .await;
                match crate::event_hooks::interpret_command_hook_results(&hook_results) {
                    crate::event_hooks::CommandHookDecision::Deny(message) => {
                        let reply = if message.is_empty() {
                            format!("Command `/{canonical}` was blocked by a hook.")
                        } else {
                            message
                        };
                        return Ok(DispatchOutcome {
                            reply,
                            transcript_echoes: Vec::new(),
                        });
                    }
                    crate::event_hooks::CommandHookDecision::Handled(message) => {
                        return Ok(DispatchOutcome {
                            reply: message,
                            transcript_echoes: Vec::new(),
                        });
                    }
                    crate::event_hooks::CommandHookDecision::Rewrite { command, args } => {
                        event.text = format!("/{command} {args}");
                        event.text = event.text.trim().to_string();
                    }
                    crate::event_hooks::CommandHookDecision::Allow => {}
                }
            }
        }
        // Identity floor command (hermes /whoami) — always allowed.
        if event.text.trim() == "/whoami" {
            let name = if event.sender_name.is_empty() {
                "(unknown)"
            } else {
                &event.sender_name
            };
            return Ok(DispatchOutcome {
                reply: format!(
                    "you are {name} ({}) on {} in chat {}",
                    event.sender_id, event.platform, event.chat_id
                ),
                transcript_echoes: Vec::new(),
            });
        }
        // Exec-approval intercept (hermes `/approve` + `/deny` slash
        // commands): resolves a pending blocking approval while the
        // blocked turn holds the busy flag, so it must run before the
        // busy check below. Tool approvals take precedence over
        // slash-confirms (hermes `has_blocking_approval` gate).
        if crate::approval_gateway::has_blocking(&key) {
            if let Some(cmd) = parse_approval_command(&event.text) {
                return Ok(DispatchOutcome {
                    reply: apply_approval_command(&key, cmd),
                    transcript_echoes: Vec::new(),
                });
            }
        }
        // Slash-confirm text-fallback intercept (hermes gateway/run.py):
        // a pending /reload-mcp confirm catches /approve-style replies;
        // anything else falls through to normal dispatch (a stale pending
        // confirm never blocks other commands).
        if let Some(pending) = crate::slash_confirm::get_pending(&key) {
            if let Some(choice) = parse_slash_confirm_reply(&event.text) {
                let resolved = crate::slash_confirm::resolve(
                    &key,
                    &pending.confirm_id,
                    choice,
                    std::time::Duration::from_secs(crate::slash_confirm::DEFAULT_TIMEOUT_SECONDS),
                )
                .await;
                return Ok(DispatchOutcome {
                    reply: resolved.unwrap_or_default(),
                    transcript_echoes: Vec::new(),
                });
            }
            crate::slash_confirm::clear_if_stale(
                &key,
                std::time::Duration::from_secs(crate::slash_confirm::DEFAULT_TIMEOUT_SECONDS),
            );
        }
        // Gateway slash commands (hermes gateway/slash_commands.py).
        if event.text.trim() == "/reload-mcp" {
            return self.handle_reload_mcp_command(&key, &event).await;
        }
        // /resume [name|N|id] — list or switch this chat's session
        // (hermes gateway /resume; needs the dispatcher remap state).
        if event.text.trim().split_whitespace().next() == Some("/resume") {
            return self.handle_resume_command(&key, &event).await;
        }
        // Direct slash commands (hermes gateway/slash_commands.py direct
        // set): answer without an LLM turn; skill/bundle slashes expand
        // into scaffolded agent turns. Runs before the busy check — these
        // never consume a turn slot.
        let mut event = event;
        if event.text.trim().starts_with('/') {
            let home = self.agent.context().home.clone();
            match crate::platform_slash::resolve(&self.agent, &self.store, &home, &key, &event.text)
                .await
            {
                Some(crate::platform_slash::PlatformSlashOutcome::Direct(reply)) => {
                    return Ok(DispatchOutcome {
                        reply,
                        transcript_echoes: Vec::new(),
                    });
                }
                Some(crate::platform_slash::PlatformSlashOutcome::AgentTurn(message)) => {
                    event.text = message;
                }
                None => {}
            }
        }
        // Stray approval commands with nothing pending keep their
        // historical reply instead of reaching the model.
        if let Some(cmd) = parse_approval_command(&event.text) {
            return Ok(DispatchOutcome {
                reply: apply_approval_command(&key, cmd),
                transcript_echoes: Vec::new(),
            });
        }
        // Clarify text intercept (hermes `_maybe_intercept_clarify_text`):
        // when a prompt awaits a typed answer, the next message resolves it
        // instead of starting a fresh turn (the blocked clarify tool call
        // in the in-flight turn receives the answer and continues).
        if !event.text.trim().is_empty() {
            if let Some(pending) = crate::clarify_gateway::pending_for_session(&key) {
                if pending.awaiting_text
                    && crate::clarify_gateway::resolve(&pending.clarify_id, &event.text)
                {
                    return Ok(DispatchOutcome::default());
                }
            }
        }
        {
            let mut busy = self.busy.lock().await;
            if *busy.get(&key).unwrap_or(&false) {
                // hermes busy-policy queue parity: park the message;
                // it runs right after the current turn settles.
                let depth = {
                    let mut queued = self.queued.lock().await;
                    let queue = queued.entry(key.clone()).or_default();
                    queue.push_back(event.clone());
                    queue.len()
                };
                // P714: publish the parked follow-up to the pending-inbound
                // directory (hermes `_pending_messages` visibility) so the
                // session stall watcher can see it.
                register_pending_inbound(&key, &event.platform, &event.chat_id);
                // P721: queue-depth detail is gated on the per-platform
                // `busy_ack_detail` display setting (hermes busy-ack
                // iteration-counter parity); quiet platforms get a plain ack.
                let busy_ack_detail = crate::config::UlncLawConfig::load(None)
                    .map(|config| {
                        crate::display_config::resolve_flag(
                            &config.display,
                            Some(&event.platform),
                            crate::display_config::DisplaySetting::BusyAckDetail,
                        )
                    })
                    .unwrap_or(true);
                let reply = if busy_ack_detail {
                    format!(
                        "(queued \u{2014} message {depth} in queue; runs after the current turn)"
                    )
                } else {
                    "(queued \u{2014} runs after the current turn)".to_string()
                };
                return Ok(DispatchOutcome {
                    reply,
                    transcript_echoes: Vec::new(),
                });
            }
            busy.insert(key.clone(), true);
        }
        let chat_ref = PlatformChatRef {
            platform: event.platform.clone(),
            chat_id: event.chat_id.clone(),
            session_key: key.clone(),
        };
        let result = MESSAGING_CTX
            .scope(chat_ref, self.run_turn(&key, &event))
            .await;
        // Keep the busy flag while draining queued follow-ups so new
        // inbound messages queue instead of interleaving.
        self.drain_queued(&key).await;
        self.busy.lock().await.insert(key, false);
        result
    }

    /// Run queued follow-up messages one by one (hermes busy-policy
    /// queue parity); replies go out through the platform sender.
    async fn drain_queued(&self, key: &str) {
        loop {
            let next = self
                .queued
                .lock()
                .await
                .get_mut(key)
                .and_then(std::collections::VecDeque::pop_front);
            let Some(event) = next else {
                // Queue drained: drop the empty entry and the P714
                // pending-inbound directory row.
                self.queued.lock().await.remove(key);
                unregister_pending_inbound(key);
                return;
            };
            let chat_ref = PlatformChatRef {
                platform: event.platform.clone(),
                chat_id: event.chat_id.clone(),
                session_key: key.to_string(),
            };
            let outcome = MESSAGING_CTX
                .scope(chat_ref, self.run_turn(key, &event))
                .await;
            let sender = platform_sender(&event.platform);
            match outcome {
                Ok(result) => {
                    if let Some(sender) = sender {
                        for echo in &result.transcript_echoes {
                            sender.send_text(&event.chat_id, echo).await;
                        }
                        if !result.reply.is_empty() {
                            // P700: durable delivery obligation around the
                            // send (hermes delivery_ledger) — a crash
                            // between finalize and send is redelivered on
                            // the next boot.
                            let obligation = crate::delivery_ledger::record_obligation(
                                &self.store,
                                key,
                                &event.platform,
                                &event.chat_id,
                                None,
                                &result.reply,
                            );
                            if let Some(id) = &obligation {
                                crate::delivery_ledger::mark_attempting(&self.store, id);
                            }
                            sender.send_text(&event.chat_id, &result.reply).await;
                            if let Some(id) = &obligation {
                                crate::delivery_ledger::mark_delivered(&self.store, id);
                            }
                        }
                    }
                }
                Err(e) => {
                    if let Some(sender) = sender {
                        sender
                            .send_text(&event.chat_id, &format!("(queued turn failed: {e})"))
                            .await;
                    }
                }
            }
        }
    }

    /// P703: ledger-protected delivery of a turn reply through an
    /// adapter's own send function (hermes delivery_ledger checkpoint
    /// parity for the direct-send adapters). The obligation records the
    /// text before the send and marks delivered after; a crash in
    /// between leaves a row the next boot's sweep redelivers.
    pub async fn send_with_ledger<F, Fut>(&self, platform: &str, chat_id: &str, text: &str, send: F)
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let session_key = format!("platform-{platform}-{chat_id}");
        let obligation = crate::delivery_ledger::record_obligation(
            &self.store,
            &session_key,
            platform,
            chat_id,
            None,
            text,
        );
        if let Some(id) = &obligation {
            crate::delivery_ledger::mark_attempting(&self.store, id);
        }
        send().await;
        if let Some(id) = &obligation {
            crate::delivery_ledger::mark_delivered(&self.store, id);
        }
    }

    /// P704: ledger-protected delivery for adapters whose send reports
    /// success/failure — the obligation is marked `delivered` on `Ok`
    /// and `failed` (definitive rejection; retried on the next boot)
    /// on `Err`. P707: the error text also feeds the dead-target
    /// registry (whole-chat deaths short-circuit future sends) and a
    /// successful send clears any stale dead flag for the target.
    pub async fn try_send_with_ledger<F, Fut>(
        &self,
        platform: &str,
        chat_id: &str,
        text: &str,
        send: F,
    ) -> bool
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = std::result::Result<(), String>>,
    {
        let session_key = format!("platform-{platform}-{chat_id}");
        let obligation = crate::delivery_ledger::record_obligation(
            &self.store,
            &session_key,
            platform,
            chat_id,
            None,
            text,
        );
        if let Some(id) = &obligation {
            crate::delivery_ledger::mark_attempting(&self.store, id);
        }
        let result = send().await;
        match &result {
            Ok(()) => {
                if let Some(id) = &obligation {
                    crate::delivery_ledger::mark_delivered(&self.store, id);
                }
                crate::dead_targets::revive(platform, chat_id);
            }
            Err(err) => {
                if let Some(id) = &obligation {
                    crate::delivery_ledger::mark_failed(&self.store, id, err);
                }
                crate::dead_targets::mark_dead_from_error(platform, chat_id, err);
            }
        }
        result.is_ok()
    }

    /// Test hook: current queue depth for a chat key.
    #[cfg(test)]
    async fn queued_depth(&self, key: &str) -> usize {
        self.queued
            .lock()
            .await
            .get(key)
            .map(std::collections::VecDeque::len)
            .unwrap_or(0)
    }

    /// `/reload-mcp` from a messaging platform (hermes
    /// `_confirm_and_reload_mcp` gateway path): gate on
    /// `approvals.mcp_reload_confirm`, deliver the prompt via native
    /// buttons where the platform supports them or `/approve` text
    /// otherwise, then rebuild the MCP tool surface on confirmation.
    async fn handle_reload_mcp_command(
        self: &Arc<Self>,
        key: &str,
        event: &MessageEvent,
    ) -> Result<DispatchOutcome> {
        let confirm_required = self
            .agent
            .tool_context()
            .config
            .approvals
            .mcp_reload_confirm;
        if !confirm_required {
            return Ok(DispatchOutcome {
                reply: self.run_reload_mcp(key).await,
                transcript_echoes: Vec::new(),
            });
        }
        let detail = "Reloading MCP servers rebuilds the tool surface and invalidates the                       provider prompt cache; the next message re-sends full input tokens                       (expensive on long-context / high-reasoning models).";
        let message = format_slash_confirm_prompt("reload-mcp", detail);
        let dispatcher = self.clone();
        let session = key.to_string();
        let handler: crate::slash_confirm::ConfirmHandler = Box::new(move |choice| {
            let dispatcher = dispatcher.clone();
            let session = session.clone();
            Box::pin(async move {
                if choice == crate::slash_confirm::ConfirmChoice::Cancel {
                    return Some("🟡 /reload-mcp cancelled. MCP tools unchanged.".to_string());
                }
                if choice == crate::slash_confirm::ConfirmChoice::Always {
                    if let Err(e) = crate::config_cmd::set_config_value(
                        "approvals.mcp_reload_confirm",
                        "false",
                        false,
                    ) {
                        eprintln!("[messaging] couldn't persist reload-mcp opt-out: {e}");
                    }
                }
                Some(dispatcher.run_reload_mcp(&session).await)
            })
        });
        let ack = request_slash_confirm(
            &event.platform,
            &event.chat_id,
            key,
            "reload-mcp",
            "Reload MCP servers?",
            &message,
            handler,
        )
        .await;
        // Buttons rendered → no redundant text ack (hermes returns None).
        Ok(DispatchOutcome {
            reply: ack.unwrap_or_default(),
            transcript_echoes: Vec::new(),
        })
    }

    /// Run the actual MCP reload and inject the change note at the END of
    /// the session history (hermes: the model sees the new tool surface
    /// next turn while the prompt-cache prefix survives).
    async fn run_reload_mcp(&self, key: &str) -> String {
        let fresh_config = match crate::config::UlncLawConfig::load(None) {
            Ok(config) => config,
            Err(e) => return format!("❌ /reload-mcp failed to re-read config: {e}"),
        };
        let report = self.agent.reload_mcp(&fresh_config).await;
        let formatted = crate::mcp::format_reload_report(&report);
        let mut change_parts: Vec<String> = Vec::new();
        if !report.added.is_empty() {
            change_parts.push(format!("Added servers: {}", report.added.join(", ")));
        }
        if !report.removed.is_empty() {
            change_parts.push(format!("Removed servers: {}", report.removed.join(", ")));
        }
        if !report.reconnected.is_empty() {
            change_parts.push(format!(
                "Reconnected servers: {}",
                report.reconnected.join(", ")
            ));
        }
        if change_parts.is_empty() {
            change_parts.push("server list unchanged".to_string());
        }
        let note = crate::provider::Message {
            role: crate::provider::Role::User,
            content: Some(format!(
                "[system note] MCP tools were just reloaded ({}). {} tool(s) now available.",
                change_parts.join("; "),
                report.tool_count
            )),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        };
        chat_histories()
            .lock()
            .await
            .entry(key.to_string())
            .or_default()
            .push(note);
        formatted
    }

    /// hermes `/resume [name|N|session-id]` parity: without args list
    /// recent titled sessions; otherwise switch this chat's session key
    /// to the resolved session (numbered pick, id/prefix, or title).
    async fn handle_resume_command(
        &self,
        key: &str,
        event: &MessageEvent,
    ) -> Result<DispatchOutcome> {
        let args = event
            .text
            .trim()
            .strip_prefix("/resume")
            .unwrap_or("")
            .trim();
        let name = strip_resume_brackets(args);
        let titled: Vec<crate::session::sqlite::SessionRow> = self
            .store
            .list_session_rows(50)
            .unwrap_or_default()
            .into_iter()
            .filter(|row| row.title.as_deref().map(|t| !t.is_empty()).unwrap_or(false))
            .take(10)
            .collect();
        if name.is_empty() {
            if titled.is_empty() {
                return Ok(DispatchOutcome {
                    reply: "No named sessions found.
Use `/title My Session` to name your                             current session, then `/resume My Session` to return to it later."
                        .to_string(),
                    transcript_echoes: Vec::new(),
                });
            }
            let mut lines = vec!["\u{1F4CB} **Named Sessions**".to_string()];
            for (idx, row) in titled.iter().enumerate() {
                lines.push(format!(
                    "  {}. {} ({} msgs)",
                    idx + 1,
                    row.title.as_deref().unwrap_or("(untitled)"),
                    row.message_count
                ));
            }
            lines.push(
                "
Usage: `/resume <session name>` or `/resume <number>` (e.g. `/resume 1`                  for the most recent)"
                    .to_string(),
            );
            return Ok(DispatchOutcome {
                reply: lines.join(
                    "
",
                ),
                transcript_echoes: Vec::new(),
            });
        }
        // Resolve a numbered choice, session id/prefix, or title.
        let mut target_id: Option<String> = None;
        if let Ok(index) = name.parse::<usize>() {
            if index < 1 || index > titled.len() {
                return Ok(DispatchOutcome {
                    reply: format!(
                        "Resume index {index} is out of range.
Use `/resume` with no                          arguments to see available sessions."
                    ),
                    transcript_echoes: Vec::new(),
                });
            }
            target_id = Some(titled[index - 1].id.clone());
        } else if let Some(id) = self.store.resolve_session_id(name).ok().flatten() {
            target_id = Some(id);
        } else if let Some(id) = self.store.resolve_session_by_title(name).ok().flatten() {
            target_id = Some(id);
        }
        let Some(target_id) = target_id else {
            return Ok(DispatchOutcome {
                reply: format!(
                    "No session found matching '**{name}**'.
Use `/resume` with no                      arguments to see available sessions."
                ),
                transcript_echoes: Vec::new(),
            });
        };
        if effective_session_id_for(key) == target_id {
            let title = self
                .store
                .get_session_row(&target_id)
                .ok()
                .flatten()
                .and_then(|row| row.title)
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| name.to_string());
            return Ok(DispatchOutcome {
                reply: format!("\u{1F4CC} Already on session **{title}**."),
                transcript_echoes: Vec::new(),
            });
        }
        // Switch: remap the chat key and drop the cached history so the
        // next turn rebuilds from the resumed session's transcript
        // (hermes switch_session + agent-cache eviction).
        set_session_remap(key, &target_id);
        drop_history_cache(key).await;
        let row = self.store.get_session_row(&target_id).ok().flatten();
        let title = row
            .as_ref()
            .and_then(|r| r.title.clone())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| name.to_string());
        let msg_count = row.as_ref().map(|r| r.message_count).unwrap_or(0);
        let reply = if msg_count == 0 {
            format!("\u{21BB} Resumed session **{title}**. Conversation restored.")
        } else {
            format!(
                "\u{21BB} Resumed session **{title}** ({msg_count} messages).                  Conversation restored."
            )
        };
        Ok(DispatchOutcome {
            reply,
            transcript_echoes: Vec::new(),
        })
    }

    async fn run_turn(&self, key: &str, event: &MessageEvent) -> Result<DispatchOutcome> {
        use crate::provider::Role;
        // P714: activity stamp for the session stall watcher (hermes
        // `_touch_activity`) — turn start is progress.
        crate::session_activity::touch(key, "turn started", "gateway.turn");
        // Resume remap (hermes /resume + /handoff): a chat may run
        // under another session id; default stays the deterministic key.
        let session_id = effective_session_id_for(key);
        // P710: per-session turn lease (hermes turn_lease, #64934):
        // the durable transcript is owned by session_id while the busy
        // guards are keyed by routing key — and remapping makes that
        // mapping many-to-one. Serialize the whole [load history → run
        // → flush] region per session_id; fail-open on a stuck holder.
        let lease_token = self
            .turn_leases
            .acquire(
                &session_id,
                key,
                self.turn_generation
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst),
                None,
            )
            .await;
        let _lease_guard = TurnLeaseGuard {
            registry: self.turn_leases.clone(),
            token: lease_token,
        };
        // Ensure the session row exists under a deterministic id.
        if self
            .store
            .resolve_session_id(&session_id)
            .ok()
            .flatten()
            .is_none()
        {
            let created = self
                .store
                .create_named_session(
                    &session_id,
                    &format!("platform:{}", event.platform),
                    Some(&self.agent.context().config.model.model),
                    None,
                )
                .is_ok();
            if created {
                // P737: session hooks — first message of a new session
                // (hermes session:start).
                crate::event_hooks::emit(
                    "session:start",
                    serde_json::json!({
                        "platform": event.platform,
                        "chat_id": event.chat_id,
                        "session_id": session_id,
                    }),
                );
            }
        }
        let mut histories = chat_histories().lock().await;
        let history = histories.entry(key.to_string()).or_default();
        if history.is_empty() {
            if let Ok(messages) = self.store.load_messages_with_timestamps(&session_id) {
                // P711: with `[gateway] message_timestamps` on, each
                // replayed user turn is rendered with exactly one
                // timestamp prefix from its stored send time (hermes
                // inject_timestamps replay path).
                let render_ts = self.agent.context().config.gateway.message_timestamps;
                *history = messages
                    .into_iter()
                    .filter(|(_, message)| message.role != Role::System)
                    .map(|(ts, mut message)| {
                        if render_ts && matches!(message.role, Role::User) {
                            if let Some(content) = message.content.as_deref() {
                                message.content = Some(
                                    crate::message_timestamps::render_user_content_with_timestamp(
                                        content,
                                        Some(ts),
                                    ),
                                );
                            }
                        }
                        message
                    })
                    .collect();
            }
        }
        // Voice-note STT enrichment (hermes
        // `_transcribe_pending_audio_event_once`): audio attachments are
        // transcribed ahead of the turn; transcripts are echoed back to
        // the chat and the enriched text replaces the raw caption.
        let config = &self.agent.context().config;
        let audio_paths: Vec<std::path::PathBuf> = event
            .attachments
            .iter()
            .filter(|a| crate::stt::attachment_is_stt_input(&a.mime))
            .map(|a| a.path.clone())
            .collect();
        let mut user_text = event.text.clone();
        let mut transcript_echoes: Vec<String> = Vec::new();
        let mut transcribed: Vec<std::path::PathBuf> = Vec::new();
        if !audio_paths.is_empty() {
            let (enriched, transcripts, done) = crate::stt::enrich_message_with_transcription(
                &config.stt,
                &user_text,
                &audio_paths,
            )
            .await;
            user_text = enriched;
            transcribed = done;
            if config.stt.echo_transcripts {
                transcript_echoes = transcripts
                    .iter()
                    .map(|t| crate::stt::echo_line(t))
                    .collect();
            }
        }
        // P711: strip any stale gateway timestamp prefixes from the
        // inbound text unconditionally — the persisted transcript
        // stays clean regardless of the render toggle (hermes run.py
        // inbound path runs the strip before any toggle check).
        user_text = crate::message_timestamps::strip_leading_message_timestamps(&user_text).0;
        let mut prompt = if event.sender_name.is_empty() {
            user_text
        } else {
            format!("{}: {}", event.sender_name, user_text)
        };
        // P711: message timestamps (hermes message_timestamps) — the
        // inbound text was already stripped of stale prefixes above
        // (persisted transcripts stay clean regardless of the toggle);
        // with `[gateway] message_timestamps = true` the model context
        // gets exactly one rendered prefix for temporal awareness.
        if config.gateway.message_timestamps {
            let now_epoch = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);
            prompt = crate::message_timestamps::render_user_content_with_timestamp(
                &prompt,
                Some(now_epoch),
            );
        }
        // Cached attachments: images are injected natively into the user
        // turn as multimodal content (P226, hermes media-injection
        // parity); every other medium stays a path reference the agent
        // can inspect with vision_analyze / video_analyze / read_file
        // (hermes text-fallback semantics). Transcribed voice notes
        // already appear in the enriched text — skip them to avoid
        // duplicate noise.
        let remaining: Vec<&MediaAttachment> = event
            .attachments
            .iter()
            .filter(|a| !transcribed.contains(&a.path))
            .collect();
        let (injectable, mut referenced) = if config.messaging.multimodal_injection {
            split_injectable_images(&remaining)
        } else {
            (Vec::new(), remaining.clone())
        };
        let mut images: Vec<crate::provider::MessageImage> = Vec::new();
        for attachment in injectable {
            match data_url_from_cache(attachment) {
                Some(url) => images.push(crate::provider::MessageImage {
                    url,
                    media_type: Some(attachment.mime.clone()),
                }),
                // Unreadable cache entry → degrade to a path reference.
                None => referenced.push(attachment),
            }
        }
        prompt.push_str(&attachment_note_refs(&referenced));
        // P736: event hooks — agent:start (hermes hooks.py emit with
        // the documented context fields; message truncated to 500).
        crate::event_hooks::emit(
            "agent:start",
            serde_json::json!({
                "platform": event.platform,
                "user_id": event.sender_id,
                "chat_id": event.chat_id,
                "thread_id": "",
                "chat_type": crate::channel_directory::chat_type_for(&event.platform, &event.chat_id).unwrap_or_default(),
                "session_id": session_id,
                "message": crate::event_hooks::truncate_context_text(&event.text),
            }),
        );
        let result = self
            .agent
            .run_with_session_images(&prompt, images, Some(history.clone()), Some(&session_id))
            .await?;
        *history = result
            .conversation
            .into_iter()
            .filter(|m| m.role != Role::System)
            .collect();
        // P714: turn completion is progress (hermes `_touch_activity`).
        crate::session_activity::touch(key, "turn completed", "gateway.turn");
        // P724: intentional-silence suppression (hermes
        // response_filters.is_intentional_silence_agent_result) — a
        // successful turn whose reply is EXACTLY a silence marker is
        // withheld from the chat; prose merely mentioning a marker is
        // delivered normally. Empty replies are skipped by every
        // platform sender.
        let reply =
            if crate::response_filters::is_intentional_silence_agent_result(false, &result.content)
            {
                String::new()
            } else {
                result.content
            };
        // P736: event hooks — agent:end adds the truncated response.
        crate::event_hooks::emit(
            "agent:end",
            serde_json::json!({
                "platform": event.platform,
                "user_id": event.sender_id,
                "chat_id": event.chat_id,
                "thread_id": "",
                "chat_type": crate::channel_directory::chat_type_for(&event.platform, &event.chat_id).unwrap_or_default(),
                "session_id": session_id,
                "message": crate::event_hooks::truncate_context_text(&event.text),
                "response": crate::event_hooks::truncate_context_text(&reply),
            }),
        );
        Ok(DispatchOutcome {
            reply,
            transcript_echoes,
        })
    }
}

/// Strip one pair of outer brackets/quotes users type literally from
/// the usage hint (`/resume <abc123>`), mirroring hermes.
fn strip_resume_brackets(name: &str) -> &str {
    let bytes = name.as_bytes();
    if name.len() >= 2 {
        let (first, last) = (bytes[0], bytes[bytes.len() - 1]);
        if (first == b'<' && last == b'>')
            || (first == b'[' && last == b']')
            || (first == b'"' && last == b'"')
            || (first == b'\'' && last == b'\'')
        {
            return name[1..name.len() - 1].trim();
        }
    }
    name
}

/// Access gate (hermes pairing): allowlisted ids pass; everything else
/// is refused and reported so the user knows what to allowlist.
fn allowlisted(allowlist: &[String], id: &str) -> bool {
    allowlist.iter().any(|allowed| allowed == id)
}

/// Offer a pairing code to an unauthorized sender (hermes pairing flow).
/// Returns the reply text, or None to stay silent (rate-limited repeat).
fn pairing_offer(
    store: &crate::pairing::PairingStore,
    platform: &str,
    sender_id: &str,
    sender_name: &str,
) -> Option<String> {
    // Repeat requests within the rate window are silently ignored (hermes).
    if store.is_rate_limited(platform, sender_id) {
        return None;
    }
    match store.generate_code(platform, sender_id, sender_name) {
        Some(code) => Some(format!(
            "Hi~ I don't recognize you yet!\n\n\
             Here's your pairing code: `{code}`\n\n\
             Ask the bot owner to run:\n\
             `ulnclaw pairing approve {platform} {code}`"
        )),
        None => {
            // Queue full or locked out: send the throttle notice once, then
            // silence follow-ups via the rate limit (hermes behavior).
            store.record_rate_limit(platform, sender_id);
            Some("Too many pairing requests right now~ Please try again later!".to_string())
        }
    }
}

/// Attachment download cap (Discord's free-tier upload limit — hermes
/// applies a similar ceiling).
const MAX_MEDIA_BYTES: u64 = 25 * 1024 * 1024;

/// Cache downloaded bytes and wrap failures into a text note (hermes
/// degrades attachment problems to text, never fatal).
fn cache_attachment(data: Vec<u8>, mime: &str, original_name: &str) -> Option<MediaAttachment> {
    if data.is_empty() || data.len() as u64 > MAX_MEDIA_BYTES {
        return None;
    }
    let home = crate::config::ulnclaw_home();
    match crate::media_cache::cache_media_bytes(&home, &data, mime, original_name) {
        Ok(path) => Some(MediaAttachment {
            path,
            mime: if mime.is_empty() {
                "application/octet-stream".to_string()
            } else {
                crate::media_cache::normalize_mime(mime)
            },
            bytes: data.len() as u64,
            original_name: original_name.to_string(),
        }),
        Err(e) => {
            eprintln!("[messaging] media cache write failed: {e}");
            None
        }
    }
}

/// Download one attachment URL into the media cache. `auth` is an
/// optional Authorization header (Slack private file URLs need the bot
/// bearer). Failures degrade to None with a log line (hermes policy).
async fn download_to_cache(
    client: &reqwest::Client,
    url: &str,
    auth: Option<&str>,
    mime: &str,
    original_name: &str,
    declared_size: u64,
) -> Option<MediaAttachment> {
    if declared_size > MAX_MEDIA_BYTES {
        eprintln!(
            "[messaging] skipping attachment {original_name:?}: {declared_size} bytes exceeds the {MAX_MEDIA_BYTES} byte cap"
        );
        return None;
    }
    let mut request = client.get(url);
    if let Some(auth) = auth {
        request = request.header("Authorization", auth);
    }
    match request.send().await.and_then(|r| r.error_for_status()) {
        Ok(response) => match response.bytes().await {
            Ok(bytes) => cache_attachment(bytes.to_vec(), mime, original_name),
            Err(e) => {
                eprintln!("[messaging] attachment download failed: {e}");
                None
            }
        },
        Err(e) => {
            eprintln!("[messaging] attachment download failed: {e}");
            None
        }
    }
}

/// Per-image inline-injection cap — base64 inflates ~33%, and
/// providers reject giant payloads (hermes applies a similar ceiling).
const MAX_INLINE_IMAGE_BYTES: u64 = 8 * 1024 * 1024;

/// Split cached attachments into inline-injectable images (mime
/// `image/*` within the size cap) and everything else (P226 multimodal
/// injection). Non-images and oversized images stay path references.
fn split_injectable_images<'a>(
    attachments: &[&'a MediaAttachment],
) -> (Vec<&'a MediaAttachment>, Vec<&'a MediaAttachment>) {
    let mut images = Vec::new();
    let mut rest = Vec::new();
    for attachment in attachments {
        if attachment.mime.starts_with("image/") && attachment.bytes <= MAX_INLINE_IMAGE_BYTES {
            images.push(*attachment);
        } else {
            rest.push(*attachment);
        }
    }
    (images, rest)
}

/// Build a `data:` URL from a cached attachment (base64). `None` when
/// the file vanished or reads empty — the caller degrades the
/// attachment back to a path reference.
fn data_url_from_cache(attachment: &MediaAttachment) -> Option<String> {
    let data = std::fs::read(&attachment.path).ok()?;
    if data.is_empty() {
        return None;
    }
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(&data);
    Some(format!("data:{};base64,{}", attachment.mime, encoded))
}

/// Render attachments as a text note appended to the user message —
/// the agent can inspect images via `vision_analyze`, video via
/// `video_analyze`, and documents via `read_file` (hermes falls back to
/// exactly this text reference when native multimodal injection fails).
#[cfg(test)]
fn attachment_note(attachments: &[MediaAttachment]) -> String {
    let refs: Vec<&MediaAttachment> = attachments.iter().collect();
    attachment_note_refs(&refs)
}

fn attachment_note_refs(attachments: &[&MediaAttachment]) -> String {
    if attachments.is_empty() {
        return String::new();
    }
    let mut note = String::from("\n\n[Attached media]\n");
    for attachment in attachments {
        let name = if attachment.original_name.is_empty() {
            String::new()
        } else {
            format!(" \"{}\"", attachment.original_name)
        };
        note.push_str(&format!(
            "- {} ({}, {} bytes){}\n",
            attachment.path.display(),
            attachment.mime,
            attachment.bytes,
            name
        ));
    }
    note.push_str(
        "Inspect images with vision_analyze, video with video_analyze,          documents with read_file.",
    );
    note
}

/// Extract `MEDIA:<path>` delivery tags from a reply (hermes
/// `extract_media`): returns the cleaned text plus the file paths, in
/// order of appearance. Tags may stand alone on a line; surrounding
/// whitespace-only lines are dropped with them.
pub fn extract_media_tags(text: &str) -> (String, Vec<std::path::PathBuf>) {
    let mut cleaned: Vec<String> = Vec::new();
    let mut paths: Vec<std::path::PathBuf> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("MEDIA:") {
            let candidate = rest.trim().trim_matches('`');
            if !candidate.is_empty() {
                let expanded = if let Some(stripped) = candidate.strip_prefix("~/") {
                    std::env::var_os("HOME")
                        .or_else(|| std::env::var_os("USERPROFILE"))
                        .map(|home| {
                            std::path::PathBuf::from(home)
                                .join(stripped)
                                .display()
                                .to_string()
                        })
                        .unwrap_or_else(|| candidate.to_string())
                } else {
                    candidate.to_string()
                };
                if std::path::Path::new(&expanded).is_file() {
                    paths.push(std::path::PathBuf::from(expanded));
                    continue;
                }
            }
        }
        cleaned.push(line.to_string());
    }
    // Collapse runs of blank lines left behind by removed tags.
    let mut out = String::new();
    let mut prev_blank = false;
    for line in &cleaned {
        let blank = line.trim().is_empty();
        if blank && prev_blank {
            continue;
        }
        out.push_str(line);
        out.push('\n');
        prev_blank = blank;
    }
    (out.trim_end_matches('\n').to_string(), paths)
}

/// Pub wrapper for webhook platforms (whatsapp_cloud / msgraph).
pub fn pairing_offer_public(
    store: &crate::pairing::PairingStore,
    platform: &str,
    sender_id: &str,
    sender_name: &str,
) -> Option<String> {
    pairing_offer(store, platform, sender_id, sender_name)
}

/// Pub wrapper for webhook platforms (whatsapp_cloud / msgraph).
pub async fn pre_gateway_dispatch_gate_public(event: &mut MessageEvent) -> bool {
    pre_gateway_dispatch_gate(event).await
}

/// `pre_gateway_dispatch` plugin hook (hermes fires it BEFORE auth so
/// plugins can handle unauthorized senders without triggering pairing).
/// Returns false when a hook consumed the event (`{"action": "skip"}`);
/// a `{"action": "rewrite", "text": ...}` response replaces `event.text`.
async fn pre_gateway_dispatch_gate(event: &mut MessageEvent) -> bool {
    if !crate::plugins::has_hook("pre_gateway_dispatch") {
        return true;
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let payload = crate::plugins::hook_payload(
        "pre_gateway_dispatch",
        "",
        &cwd,
        vec![
            ("platform", json!(event.platform)),
            ("chat_id", json!(event.chat_id)),
            ("sender_id", json!(event.sender_id)),
            ("sender_name", json!(event.sender_name)),
            ("text", json!(event.text)),
            ("message_id", json!(event.message_id)),
        ],
        json!({}),
    );
    let responses = crate::plugins::invoke_hook("pre_gateway_dispatch", payload).await;
    match crate::plugins::dispatch_decision(&responses) {
        Some((action, detail)) if action == "skip" => {
            eprintln!(
                "[messaging] pre_gateway_dispatch skipped {} message from chat {}: {}",
                event.platform,
                event.chat_id,
                detail.unwrap_or_default()
            );
            false
        }
        Some((action, text)) if action == "rewrite" => {
            if let Some(text) = text {
                event.text = text;
            }
            true
        }
        _ => true,
    }
}

/// Start every enabled platform; resolves when they have all stopped.
pub async fn run_messaging(
    config: &crate::config::UlncLawConfig,
    agent: Arc<Agent>,
    store: Arc<SqliteSessionStore>,
    profile_routing: Option<Arc<ProfileRoutingHub>>,
) {
    let dispatcher = match profile_routing {
        Some(hub) => Dispatcher::new_with_routing(agent, store, hub),
        None => Dispatcher::new(agent, store),
    };
    // P735: shutdown-flush recovery (hermes `recover_pending_to_db`):
    // parked follow-ups flushed by the previous process exit are
    // re-dispatched through the normal path (profile routing included),
    // so a restart never loses queued user messages.
    let recovered = crate::shutdown_flush::recover_parked_events(&crate::config::ulnclaw_home());
    if !recovered.is_empty() {
        let recovery_dispatcher = dispatcher.clone();
        tokio::spawn(async move {
            for (session_key, event) in recovered {
                tracing::info!("[shutdown_flush] re-dispatching parked message for {session_key}");
                if let Err(e) = recovery_dispatcher.handle_event(event).await {
                    tracing::warn!(
                        "[shutdown_flush] recovery dispatch failed for {session_key}: {e}"
                    );
                }
            }
        });
    }
    let mut tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    let msg = &config.messaging;
    let pairing: Option<Arc<crate::pairing::PairingStore>> = if msg.pairing {
        Some(Arc::new(crate::pairing::PairingStore::open(
            &crate::config::ulnclaw_home(),
        )))
    } else {
        None
    };
    if msg.telegram.enabled {
        let cfg = msg.telegram.clone();
        let dispatcher = dispatcher.clone();
        let pairing = pairing.clone();
        spawn_platform_task(&mut tasks, "telegram", move || {
            let cfg = cfg.clone();

            let dispatcher = dispatcher.clone();

            let pairing = pairing.clone();

            async move { telegram::run(cfg, dispatcher, pairing).await }
        });
    }
    if msg.discord.enabled {
        let cfg = msg.discord.clone();
        let dispatcher = dispatcher.clone();
        let pairing = pairing.clone();
        spawn_platform_task(&mut tasks, "discord", move || {
            let cfg = cfg.clone();

            let dispatcher = dispatcher.clone();

            let pairing = pairing.clone();

            async move { discord::run(cfg, dispatcher, pairing).await }
        });
    }
    if msg.slack.enabled {
        let cfg = msg.slack.clone();
        let dispatcher = dispatcher.clone();
        let pairing = pairing.clone();
        spawn_platform_task(&mut tasks, "slack", move || {
            let cfg = cfg.clone();

            let dispatcher = dispatcher.clone();

            let pairing = pairing.clone();

            async move { slack::run(cfg, dispatcher, pairing).await }
        });
    }
    if msg.signal.enabled {
        let cfg = msg.signal.clone();
        let dispatcher = dispatcher.clone();
        let pairing = pairing.clone();
        spawn_platform_task(&mut tasks, "signal", move || {
            let cfg = cfg.clone();

            let dispatcher = dispatcher.clone();

            let pairing = pairing.clone();

            async move { crate::signal::run(cfg, dispatcher, pairing).await }
        });
    }
    if msg.weixin.enabled {
        let cfg = msg.weixin.clone();
        let dispatcher = dispatcher.clone();
        let pairing = pairing.clone();
        spawn_platform_task(&mut tasks, "weixin", move || {
            let cfg = cfg.clone();

            let dispatcher = dispatcher.clone();

            let pairing = pairing.clone();

            async move { crate::weixin::run(cfg, dispatcher, pairing).await }
        });
    }
    if msg.qq.enabled {
        let cfg = msg.qq.clone();
        let dispatcher = dispatcher.clone();
        let pairing = pairing.clone();
        spawn_platform_task(&mut tasks, "qq", move || {
            let cfg = cfg.clone();

            let dispatcher = dispatcher.clone();

            let pairing = pairing.clone();

            async move { crate::qqbot::run(cfg, dispatcher, pairing).await }
        });
    }
    if msg.yuanbao.enabled {
        let cfg = msg.yuanbao.clone();
        let dispatcher = dispatcher.clone();
        let pairing = pairing.clone();
        spawn_platform_task(&mut tasks, "yuanbao", move || {
            let cfg = cfg.clone();

            let dispatcher = dispatcher.clone();

            let pairing = pairing.clone();

            async move { crate::yuanbao::run(cfg, dispatcher, pairing).await }
        });
    }
    if msg.email.enabled {
        let cfg = msg.email.clone();
        let dispatcher = dispatcher.clone();
        let pairing = pairing.clone();
        spawn_platform_task(&mut tasks, "email", move || {
            let cfg = cfg.clone();

            let dispatcher = dispatcher.clone();

            let pairing = pairing.clone();

            async move { crate::email_platform::run(cfg, dispatcher, pairing).await }
        });
    }
    if msg.mattermost.enabled {
        let cfg = msg.mattermost.clone();
        let dispatcher = dispatcher.clone();
        let pairing = pairing.clone();
        spawn_platform_task(&mut tasks, "mattermost", move || {
            let cfg = cfg.clone();

            let dispatcher = dispatcher.clone();

            let pairing = pairing.clone();

            async move { crate::mattermost::run(cfg, dispatcher, pairing).await }
        });
    }
    if msg.matrix.enabled {
        let cfg = msg.matrix.clone();
        let dispatcher = dispatcher.clone();
        let pairing = pairing.clone();
        spawn_platform_task(&mut tasks, "matrix", move || {
            let cfg = cfg.clone();

            let dispatcher = dispatcher.clone();

            let pairing = pairing.clone();

            async move { crate::matrix::run(cfg, dispatcher, pairing).await }
        });
    }
    if msg.dingtalk.enabled {
        let cfg = msg.dingtalk.clone();
        let dispatcher = dispatcher.clone();
        let pairing = pairing.clone();
        spawn_platform_task(&mut tasks, "dingtalk", move || {
            let cfg = cfg.clone();

            let dispatcher = dispatcher.clone();

            let pairing = pairing.clone();

            async move { crate::dingtalk::run(cfg, dispatcher, pairing).await }
        });
    }
    if msg.wecom.enabled {
        let cfg = msg.wecom.clone();
        let dispatcher = dispatcher.clone();
        let pairing = pairing.clone();
        spawn_platform_task(&mut tasks, "wecom", move || {
            let cfg = cfg.clone();

            let dispatcher = dispatcher.clone();

            let pairing = pairing.clone();

            async move { crate::wecom::run(cfg, dispatcher, pairing).await }
        });
    }
    // Standalone `notify/notify` sender (hermes `_standalone_send`):
    // delivery works without the live WS adapter whenever credentials
    // are configured; a starting live adapter overwrites the slot.
    crate::homeassistant::maybe_register_standalone_sender(&msg.homeassistant);
    if msg.homeassistant.enabled {
        let cfg = msg.homeassistant.clone();
        let dispatcher = dispatcher.clone();
        let pairing = pairing.clone();
        spawn_platform_task(&mut tasks, "homeassistant", move || {
            let cfg = cfg.clone();

            let dispatcher = dispatcher.clone();

            let pairing = pairing.clone();

            async move { crate::homeassistant::run(cfg, dispatcher, pairing).await }
        });
    }
    if msg.whatsapp.enabled {
        let cfg = msg.whatsapp.clone();
        let dispatcher = dispatcher.clone();
        let pairing = pairing.clone();
        spawn_platform_task(&mut tasks, "whatsapp", move || {
            let cfg = cfg.clone();

            let dispatcher = dispatcher.clone();

            let pairing = pairing.clone();

            async move { crate::whatsapp::run(cfg, dispatcher, pairing).await }
        });
    }
    if msg.irc.enabled {
        let cfg = msg.irc.clone();
        let dispatcher = dispatcher.clone();
        let pairing = pairing.clone();
        spawn_platform_task(&mut tasks, "irc", move || {
            let cfg = cfg.clone();

            let dispatcher = dispatcher.clone();

            let pairing = pairing.clone();

            async move { crate::irc::run(cfg, dispatcher, pairing).await }
        });
    }
    if msg.ntfy.enabled {
        let cfg = msg.ntfy.clone();
        let dispatcher = dispatcher.clone();
        let pairing = pairing.clone();
        spawn_platform_task(&mut tasks, "ntfy", move || {
            let cfg = cfg.clone();

            let dispatcher = dispatcher.clone();

            let pairing = pairing.clone();

            async move { crate::ntfy::run(cfg, dispatcher, pairing).await }
        });
    }
    if msg.simplex.enabled {
        let cfg = msg.simplex.clone();
        let dispatcher = dispatcher.clone();
        let pairing = pairing.clone();
        spawn_platform_task(&mut tasks, "simplex", move || {
            let cfg = cfg.clone();

            let dispatcher = dispatcher.clone();

            let pairing = pairing.clone();

            async move { crate::simplex::run(cfg, dispatcher, pairing).await }
        });
    }
    if msg.teams.enabled {
        // Gateway-mounted webhook platform: register sender + runtime.
        crate::teams::register(&msg.teams);
    }
    if msg.line.enabled {
        crate::line::register(&msg.line);
    }
    if msg.google_chat.enabled {
        crate::google_chat::register(&msg.google_chat);
        if !msg
            .google_chat
            .resolve()
            .pubsub_subscription
            .trim()
            .is_empty()
        {
            let dispatcher = dispatcher.clone();
            let pairing = pairing.clone();
            spawn_platform_task(&mut tasks, "google_chat", move || {
                let dispatcher = dispatcher.clone();
                let pairing = pairing.clone();
                async move { crate::google_chat::run_pubsub(dispatcher, pairing).await }
            });
        }
    }
    if msg.buzz.enabled {
        let cfg = msg.buzz.clone();
        let dispatcher = dispatcher.clone();
        let pairing = pairing.clone();
        spawn_platform_task(&mut tasks, "buzz", move || {
            let cfg = cfg.clone();

            let dispatcher = dispatcher.clone();

            let pairing = pairing.clone();

            async move { crate::buzz::run(cfg, dispatcher, pairing).await }
        });
    }
    if msg.photon.enabled {
        let cfg = msg.photon.clone();
        let dispatcher = dispatcher.clone();
        let pairing = pairing.clone();
        spawn_platform_task(&mut tasks, "photon", move || {
            let cfg = cfg.clone();

            let dispatcher = dispatcher.clone();

            let pairing = pairing.clone();

            async move { crate::photon::run(cfg, dispatcher, pairing).await }
        });
    }
    if msg.feishu.enabled && crate::feishu::is_websocket_mode(&msg.feishu.resolve().connection_mode)
    {
        // WebSocket long connection (hermes default); webhook mode
        // rides the gateway /webhooks/feishu route instead.
        let cfg = msg.feishu.clone();
        let dispatcher = dispatcher.clone();
        let pairing = pairing.clone();
        spawn_platform_task(&mut tasks, "feishu", move || {
            let cfg = cfg.clone();

            let dispatcher = dispatcher.clone();

            let pairing = pairing.clone();

            async move { crate::feishu_ws::run(cfg, dispatcher, pairing).await }
        });
    }
    if msg.a2a.enabled {
        crate::a2a::register(&msg.a2a);
    }
    if tasks.is_empty() {
        eprintln!("[messaging] no platforms enabled ([messaging.telegram|discord|slack|signal|weixin|qq|yuanbao|email|mattermost|matrix|dingtalk|wecom|homeassistant|whatsapp|irc|ntfy|simplex|buzz|photon|feishu] enabled = true (sms/teams/line/google_chat/raft/a2a ride gateway webhook routes))");
        return;
    }
    for task in tasks {
        task.await.ok();
    }
}

fn resolve_token(configured: &str, env_var: &str) -> Option<String> {
    let trimmed = configured.trim();
    if !trimmed.is_empty() {
        return Some(trimmed.to_string());
    }
    std::env::var(env_var).ok().filter(|v| !v.trim().is_empty())
}

/// Public token resolution for cross-module delivery targets
/// (generic-webhook `deliver = "telegram"`; hermes webhook.py reuses the
/// platform credentials).
pub fn resolve_telegram_token_public(cfg: &TelegramConfig) -> Option<String> {
    resolve_token(&cfg.bot_token, "TELEGRAM_BOT_TOKEN")
}

/// Public send wrapper (see `resolve_telegram_token_public`).
pub async fn telegram_send_public(
    client: &reqwest::Client,
    token: &str,
    chat_id: &str,
    text: &str,
) {
    telegram::send_message(client, token, chat_id, text).await
}

/// Public token resolution for webhook `deliver = "discord"`.
pub fn resolve_discord_token_public(cfg: &DiscordConfig) -> Option<String> {
    resolve_token(&cfg.bot_token, "DISCORD_BOT_TOKEN")
}

/// Public send wrapper (see `resolve_discord_token_public`).
pub async fn discord_send_public(token: &str, channel_id: &str, text: &str) {
    discord::send_channel_message(token, channel_id, text).await
}

/// Public token resolution for webhook `deliver = "slack"`.
pub fn resolve_slack_bot_token_public(cfg: &SlackConfig) -> Option<String> {
    resolve_token(&cfg.bot_token, "SLACK_BOT_TOKEN")
}

/// Public send wrapper (see `resolve_slack_bot_token_public`).
pub async fn slack_send_public(token: &str, channel: &str, text: &str) {
    slack::post_message(token, channel, text).await
}

/// Public media wrapper (standalone send_message / MCP bridge): Telegram
/// photo/document delivery with hermes caption-split semantics.
pub async fn telegram_send_media_public(
    client: &reqwest::Client,
    token: &str,
    chat_id: &str,
    text: &str,
    paths: &[std::path::PathBuf],
) -> bool {
    if paths.is_empty() {
        return false;
    }
    let (caption, body) = telegram::media_caption_split(text, paths, 1024);
    if !body.trim().is_empty() {
        telegram::send_message(client, token, chat_id, &body).await;
    }
    for (idx, path) in paths.iter().enumerate() {
        let cap = if idx == 0 { caption.as_deref() } else { None };
        if crate::media_cache::mime_for_ext(path).starts_with("image/") {
            telegram::send_photo(client, token, chat_id, path, cap).await;
        } else {
            telegram::send_document(client, token, chat_id, path, cap).await;
        }
    }
    true
}

/// Public media wrapper (standalone send_message / MCP bridge): one
/// multipart Discord message with content + attachments.
pub async fn discord_send_media_public(
    token: &str,
    channel_id: &str,
    text: &str,
    paths: &[std::path::PathBuf],
) -> bool {
    discord::send_media_message(token, channel_id, text, paths).await
}

/// Public media wrapper (standalone send_message / MCP bridge): body
/// text via chat.postMessage, then one native upload per file.
pub async fn slack_send_media_public(
    token: &str,
    channel: &str,
    text: &str,
    paths: &[std::path::PathBuf],
) -> bool {
    if paths.is_empty() {
        return false;
    }
    if !text.trim().is_empty() {
        slack::post_message(token, channel, text.trim()).await;
    }
    for path in paths {
        slack::upload_file(token, channel, path).await;
    }
    true
}

// ---------------------------------------------------------------------------
// Telegram — Bot API long-polling
// ---------------------------------------------------------------------------

pub mod telegram {
    use super::*;

    const API: &str = "https://api.telegram.org";

    /// Telegram Bot API base URL — normally `https://api.telegram.org`;
    /// the `TELEGRAM_API_BASE` override exists for tests and corporate
    /// proxies (mirrors the slack `SLACK_API_BASE` pattern).
    pub(crate) fn telegram_api_base() -> String {
        std::env::var("TELEGRAM_API_BASE")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| API.to_string())
    }

    /// Monotonic approval-id counter (hermes `_approval_counter`).
    static APPROVAL_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    /// approval-id → session_key registry (hermes `_approval_state`):
    /// `callback_data` only carries the short id; a tap looks the session
    /// up here and pops it, so one prompt resolves exactly once.
    fn approval_state() -> &'static std::sync::Mutex<std::collections::HashMap<u64, String>> {
        static STATE: std::sync::OnceLock<
            std::sync::Mutex<std::collections::HashMap<u64, String>>,
        > = std::sync::OnceLock::new();
        STATE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
    }

    #[cfg(test)]
    fn reset_approval_state_for_tests() {
        approval_state().lock().unwrap().clear();
    }

    struct Sender {
        client: reqwest::Client,
        token: String,
    }

    #[async_trait::async_trait]
    impl PlatformSender for Sender {
        async fn send_text(&self, chat_id: &str, text: &str) {
            send_message(&self.client, &self.token, chat_id, text).await;
        }

        /// Inline-keyboard clarify prompt (hermes Telegram `send_clarify`).
        /// Returns false on API failure so the numbered-text fallback still
        /// goes out.
        async fn send_clarify(
            &self,
            chat_id: &str,
            clarify_id: &str,
            question: &str,
            choices: &[String],
        ) -> bool {
            send_clarify_message(
                &self.client,
                &self.token,
                chat_id,
                clarify_id,
                question,
                choices,
            )
            .await
        }

        /// Inline-keyboard exec-approval prompt (hermes Telegram
        /// `send_exec_approval`). Returns false on API failure so the
        /// `/approve` text fallback still goes out.
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
            send_approval_message(
                &self.client,
                &self.token,
                chat_id,
                session_key,
                command,
                description,
                allow_permanent,
                allow_session,
                smart_denied,
            )
            .await
        }

        /// Emoji reactions via the Bot API (hermes send_message
        /// action='react'/'unreact'; remove = empty reaction array).
        async fn send_reaction(
            &self,
            chat_id: &str,
            emoji: &str,
            message_id: &str,
            remove: bool,
        ) -> Option<bool> {
            let Ok(msg_id) = message_id.trim().parse::<i64>() else {
                return Some(false);
            };
            let reaction: Vec<Value> = if remove {
                Vec::new()
            } else {
                vec![json!({"type": "emoji", "emoji": emoji})]
            };
            let params = json!({
                "chat_id": chat_id,
                "message_id": msg_id,
                "reaction": reaction,
            });
            match api(&self.client, &self.token, "setMessageReaction", params).await {
                Ok(_) => Some(true),
                Err(e) => {
                    eprintln!("[telegram] setMessageReaction failed: {e}");
                    Some(false)
                }
            }
        }

        /// Native media delivery with hermes caption semantics
        /// (`_media_caption_split`, 1024-char Telegram cap).
        async fn send_media(
            &self,
            chat_id: &str,
            text: &str,
            paths: &[std::path::PathBuf],
        ) -> bool {
            if paths.is_empty() {
                return false;
            }
            let (caption, body) = media_caption_split(text, paths, 1024);
            if !body.trim().is_empty() {
                send_message(&self.client, &self.token, chat_id, &body).await;
            }
            for (idx, path) in paths.iter().enumerate() {
                let cap = if idx == 0 { caption.as_deref() } else { None };
                if crate::media_cache::mime_for_ext(path).starts_with("image/") {
                    send_photo(&self.client, &self.token, chat_id, path, cap).await;
                } else {
                    send_document(&self.client, &self.token, chat_id, path, cap).await;
                }
            }
            true
        }
    }

    pub async fn run(
        cfg: TelegramConfig,
        dispatcher: Arc<Dispatcher>,
        pairing: Option<Arc<crate::pairing::PairingStore>>,
    ) {
        let Some(token) = resolve_token(&cfg.bot_token, "TELEGRAM_BOT_TOKEN") else {
            eprintln!("[telegram] disabled: no bot_token configured (set messaging.telegram.bot_token or TELEGRAM_BOT_TOKEN)");
            return;
        };
        let client = reqwest::Client::new();
        register_platform_sender(
            "telegram",
            Arc::new(Sender {
                client: client.clone(),
                token: token.clone(),
            }),
        );
        match api(&client, &token, "getMe", json!({})).await {
            Ok(me) => {
                let username = me
                    .pointer("/result/username")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                eprintln!("[telegram] connected as @{username}");
            }
            Err(e) => {
                eprintln!("[telegram] getMe failed: {e}");
                return;
            }
        }
        let mut offset: Option<i64> = None;
        loop {
            let mut params =
                json!({"timeout": 25, "allowed_updates": ["message", "callback_query"]});
            if let Some(offset) = offset {
                params["offset"] = json!(offset + 1);
            }
            let updates = match api(&client, &token, "getUpdates", params).await {
                Ok(value) => value
                    .pointer("/result")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default(),
                Err(e) => {
                    eprintln!("[telegram] getUpdates failed: {e} — retrying in 5s");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    continue;
                }
            };
            for update in updates {
                let update_id = update
                    .get("update_id")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                offset = Some(offset.unwrap_or(0).max(update_id));
                // Clarify button taps arrive as callback_query updates and
                // resolve the pending clarify (hermes Telegram callback
                // handler); they never enter the message pipeline.
                if let Some(query) = update.get("callback_query") {
                    handle_callback_query(&client, &token, &cfg, pairing.as_deref(), query).await;
                    continue;
                }
                let Some(message) = update.get("message") else {
                    continue;
                };
                let text = message.get("text").and_then(|v| v.as_str()).unwrap_or("");
                // Media (photo/document/video/audio/voice) is downloaded
                // and cached; media-only messages still flow (hermes).
                let attachments = download_media(&client, &token, message).await;
                if text.is_empty() && attachments.is_empty() {
                    continue;
                }
                let chat = message.get("chat").cloned().unwrap_or(json!({}));
                let chat_id = chat
                    .get("id")
                    .map(|v| match v {
                        Value::Number(n) => n.to_string(),
                        other => other.as_str().unwrap_or("").to_string(),
                    })
                    .unwrap_or_default();
                if chat_id.is_empty() {
                    continue;
                }
                let sender = message.get("from").cloned().unwrap_or(json!({}));
                let mut event = MessageEvent {
                    platform: "telegram".into(),
                    chat_id: chat_id.clone(),
                    sender_id: sender.get("id").map(|v| v.to_string()).unwrap_or_default(),
                    sender_name: sender
                        .get("first_name")
                        .or_else(|| sender.get("username"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    text: text.to_string(),
                    message_id: message
                        .get("message_id")
                        .map(|v| v.to_string())
                        .unwrap_or_default(),
                    attachments,
                };
                // Plugin gate before auth (hermes ordering).
                if !pre_gateway_dispatch_gate(&mut event).await {
                    continue;
                }
                // Auth union: configured allowlist OR an approved pairing
                // code (hermes authz union).
                let authorized = allowlisted(&cfg.allowed_chat_ids, &chat_id)
                    || pairing
                        .as_ref()
                        .map(|store| store.is_approved("telegram", &event.sender_id))
                        .unwrap_or(false);
                if !authorized {
                    eprintln!(
                        "[telegram] refusing message from chat {chat_id} — add it to \
                         messaging.telegram.allowed_chat_ids or approve a pairing code"
                    );
                    if let Some(store) = pairing.as_ref() {
                        if let Some(reply) =
                            pairing_offer(store, "telegram", &event.sender_id, &event.sender_name)
                        {
                            send_message(&client, &token, &chat_id, &reply).await;
                        }
                    }
                    continue;
                }
                let dispatcher = dispatcher.clone();
                let client = client.clone();
                let token = token.clone();
                tokio::spawn(async move {
                    let outcome = match dispatcher.handle_event(event.clone()).await {
                        Ok(outcome) => outcome,
                        Err(e) => crate::messaging::DispatchOutcome {
                            reply: format!("error: {e}"),
                            transcript_echoes: Vec::new(),
                        },
                    };
                    for echo in &outcome.transcript_echoes {
                        send_message(&client, &token, &event.chat_id, echo).await;
                    }
                    let reply = outcome.reply;
                    // MEDIA:<path> tags become native attachments (hermes
                    // extract_media); the rest is sent as text.
                    let (reply_text, media_paths) = extract_media_tags(&reply);
                    if !reply_text.trim().is_empty() {
                        // P703: ledger-protected reply delivery.
                        dispatcher
                            .send_with_ledger("telegram", &event.chat_id, &reply_text, || {
                                send_message(&client, &token, &event.chat_id, &reply_text)
                            })
                            .await;
                    }
                    for path in &media_paths {
                        if crate::media_cache::mime_for_ext(path).starts_with("image/") {
                            send_photo(&client, &token, &event.chat_id, path, None).await;
                        } else {
                            send_document(&client, &token, &event.chat_id, path, None).await;
                        }
                    }
                });
            }
        }
    }

    async fn api(
        client: &reqwest::Client,
        token: &str,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        let url = format!("{}/bot{token}/{method}", telegram_api_base());
        let response = client
            .post(&url)
            .json(&params)
            .timeout(std::time::Duration::from_secs(35))
            .send()
            .await
            .map_err(|e| AgentError::Tool(format!("telegram {method}: {e}")))?;
        let value: Value = response
            .json()
            .await
            .map_err(|e| AgentError::Tool(format!("telegram {method} parse: {e}")))?;
        if value.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            Ok(value)
        } else {
            Err(AgentError::Tool(format!(
                "telegram {method}: {}",
                value
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error")
            )))
        }
    }

    /// HTML-escape for HTML parse-mode payloads (hermes `_html.escape`).
    fn html_escape(text: &str) -> String {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&#x27;")
    }

    /// Render a clarify prompt with one inline button per choice (hermes
    /// Telegram `send_clarify` layout): full option text in the message
    /// body so mobile users can read long choices, short numeric button
    /// labels to avoid Telegram truncation, plus a final "✏️ Other (type
    /// answer)" row that flips the prompt into text-capture mode.
    /// `cl:<id>:<idx|other>` callback payloads stay inside Telegram's
    /// 64-byte callback_data cap. Returns false on API failure so the
    /// caller falls back to numbered text.
    async fn send_clarify_message(
        client: &reqwest::Client,
        token: &str,
        chat_id: &str,
        clarify_id: &str,
        question: &str,
        choices: &[String],
    ) -> bool {
        let mut text = format!("❓ {}", html_escape(question));
        let mut rows: Vec<Value> = Vec::new();
        if !choices.is_empty() {
            let option_lines = choices
                .iter()
                .enumerate()
                .map(|(idx, choice)| format!("{}. {}", idx + 1, html_escape(choice)))
                .collect::<Vec<_>>()
                .join("\n");
            text = format!("{text}\n\n{option_lines}");
            for idx in 0..choices.len() {
                rows.push(json!([{
                    "text": (idx + 1).to_string(),
                    "callback_data": format!("cl:{clarify_id}:{idx}"),
                }]));
            }
            rows.push(json!([{
                "text": "✏️ Other (type answer)",
                "callback_data": format!("cl:{clarify_id}:other"),
            }]));
        }
        let mut params = json!({"chat_id": chat_id, "text": text, "parse_mode": "HTML"});
        if !rows.is_empty() {
            params["reply_markup"] = json!({"inline_keyboard": rows});
        }
        match api(client, token, "sendMessage", params).await {
            Ok(_) => true,
            Err(e) => {
                eprintln!("[telegram] send_clarify failed: {e}");
                false
            }
        }
    }

    /// HTML exec-approval prompt (hermes `_EA_HEADER` / `_EA_CODE_OPEN` /
    /// `_EA_SMART_DENY_LINE` template attrs + `_format_exec_approval`
    /// core, HTML mode).
    const EA_CMD_BUDGET: usize = 3800;

    fn format_exec_approval_html(command: &str, description: &str, smart_denied: bool) -> String {
        let mut text = String::from("\u{26A0}\u{FE0F} <b>Command Approval Required</b>\n\n<pre>");
        text.push_str(&html_escape(&truncate_chars(command, EA_CMD_BUDGET)));
        text.push_str("</pre>\n\n");
        let description = description.trim();
        if !description.is_empty() {
            text.push_str(&html_escape(description));
        }
        if smart_denied {
            text.push_str(
                "\n\n<b>Smart DENY:</b> owner override applies to this one operation only.",
            );
        }
        text
    }

    /// Inline-keyboard approval prompt (hermes Telegram
    /// `send_exec_approval`): ✅ Allow Once / Session / Always / ❌ Deny
    /// buttons paired into 2-per-row rows (a single 4-button row
    /// truncates on mobile), `callback_data` carries a short approval id
    /// mapped to the session key in [`approval_state`].
    async fn send_approval_message(
        client: &reqwest::Client,
        token: &str,
        chat_id: &str,
        session_key: &str,
        command: &str,
        description: &str,
        allow_permanent: bool,
        allow_session: bool,
        smart_denied: bool,
    ) -> bool {
        let text = format_exec_approval_html(command, description, smart_denied);
        let approval_id = APPROVAL_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        approval_state()
            .lock()
            .unwrap()
            .insert(approval_id, session_key.to_string());
        let mut buttons: Vec<Value> = vec![json!({
            "text": "\u{2705} Allow Once",
            "callback_data": format!("ea:once:{approval_id}"),
        })];
        if !smart_denied && allow_session {
            buttons.push(json!({
                "text": "\u{2705} Session",
                "callback_data": format!("ea:session:{approval_id}"),
            }));
            if allow_permanent {
                buttons.push(json!({
                    "text": "\u{2705} Always",
                    "callback_data": format!("ea:always:{approval_id}"),
                }));
            }
        }
        buttons.push(json!({
            "text": "\u{274C} Deny",
            "callback_data": format!("ea:deny:{approval_id}"),
        }));
        let rows: Vec<Value> = buttons.chunks(2).map(|pair| json!(pair)).collect();
        let params = json!({
            "chat_id": chat_id,
            "text": text,
            "parse_mode": "HTML",
            "reply_markup": {"inline_keyboard": rows},
        });
        match api(client, token, "sendMessage", params).await {
            Ok(_) => true,
            Err(e) => {
                eprintln!("[telegram] send_exec_approval failed: {e}");
                false
            }
        }
    }

    /// Route a clarify button tap (hermes Telegram `cl:` callback branch).
    /// Every path answers the callback so the Telegram client stops the
    /// loading spinner. Unauthorized taps get ⛔; taps on resolved prompts
    /// say so; taps on expired prompts (tool side gave up) get the ⚠️
    /// notice instead of a misleading ✓.
    async fn handle_callback_query(
        client: &reqwest::Client,
        token: &str,
        cfg: &TelegramConfig,
        pairing: Option<&crate::pairing::PairingStore>,
        query: &Value,
    ) {
        let Some(data) = query.get("data").and_then(|v| v.as_str()) else {
            return;
        };
        if data.starts_with("ea:") {
            handle_approval_callback(client, token, cfg, pairing, query).await;
            return;
        }
        if !data.starts_with("cl:") {
            return;
        }
        let parts: Vec<&str> = data.splitn(3, ':').collect();
        if parts.len() != 3 {
            return;
        }
        let clarify_id = parts[1];
        let choice_token = parts[2];
        let Some(query_id) = query.get("id").and_then(|v| v.as_str()) else {
            return;
        };
        let from = query.get("from").cloned().unwrap_or(json!({}));
        let caller_id = from.get("id").map(|v| v.to_string()).unwrap_or_default();
        let user_display = from
            .get("first_name")
            .and_then(|v| v.as_str())
            .unwrap_or("User")
            .to_string();
        let message = query.get("message").cloned().unwrap_or(json!({}));
        let original_text = message
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let chat_id = message
            .get("chat")
            .and_then(|c| c.get("id"))
            .map(|v| match v {
                Value::Number(n) => n.to_string(),
                other => other.as_str().unwrap_or("").to_string(),
            })
            .unwrap_or_default();
        let message_id = message
            .get("message_id")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);

        // Auth union: configured allowlist OR an approved pairing code
        // (hermes `_is_callback_user_authorized`).
        let authorized = allowlisted(&cfg.allowed_chat_ids, &chat_id)
            || pairing
                .map(|store| store.is_approved("telegram", &caller_id))
                .unwrap_or(false);
        if !authorized {
            answer_callback(
                client,
                token,
                query_id,
                "⛔ You are not authorized to answer this prompt.",
            )
            .await;
            return;
        }

        if !crate::clarify_gateway::contains(clarify_id) {
            answer_callback(
                client,
                token,
                query_id,
                "This prompt has already been resolved.",
            )
            .await;
            return;
        }

        if choice_token == "other" {
            // Flip into text-capture mode; the next plain message in the
            // session resolves the clarify (hermes mark_awaiting_text).
            if !crate::clarify_gateway::mark_awaiting_text(clarify_id) {
                notify_clarify_expired(
                    client,
                    token,
                    query_id,
                    &chat_id,
                    message_id,
                    &original_text,
                )
                .await;
                return;
            }
            answer_callback(client, token, query_id, "✏️ Type your answer in the chat.").await;
            let edited = format!(
                "❓ {}\n\n<i>Awaiting typed response from {}…</i>",
                html_escape(&original_text),
                html_escape(&user_display)
            );
            edit_clarify_message(client, token, &chat_id, message_id, &edited).await;
            return;
        }

        let Ok(idx) = choice_token.parse::<usize>() else {
            answer_callback(client, token, query_id, "Invalid choice.").await;
            return;
        };
        // Choice text from the registered entry; fall back to the index if
        // the entry was cleaned up (hermes race fallback).
        let resolved_text = crate::clarify_gateway::peek_choice(clarify_id, choice_token)
            .unwrap_or_else(|| format!("choice {}", idx + 1));
        if crate::clarify_gateway::resolve(clarify_id, &resolved_text) {
            let preview: String = resolved_text.chars().take(60).collect();
            answer_callback(client, token, query_id, &format!("✓ {preview}")).await;
            let edited = format!(
                "❓ {}\n\n<b>{}:</b> {}",
                html_escape(&original_text),
                html_escape(&user_display),
                html_escape(&resolved_text)
            );
            edit_clarify_message(client, token, &chat_id, message_id, &edited).await;
        } else {
            // Entry evicted (clarify timeout / gateway restart) between ask
            // and tap — surface it instead of a misleading ✓.
            notify_clarify_expired(
                client,
                token,
                query_id,
                &chat_id,
                message_id,
                &original_text,
            )
            .await;
        }
    }

    /// Route an exec-approval button tap (hermes Telegram `ea:` callback
    /// branch). Resolves FIRST and renders after — a tap that lands after
    /// the approval wait timed out must not claim "Approved" (hermes
    /// #63501): the command was already denied and will not run.
    async fn handle_approval_callback(
        client: &reqwest::Client,
        token: &str,
        cfg: &TelegramConfig,
        pairing: Option<&crate::pairing::PairingStore>,
        query: &Value,
    ) {
        let Some(data) = query.get("data").and_then(|v| v.as_str()) else {
            return;
        };
        let parts: Vec<&str> = data.splitn(3, ':').collect();
        if parts.len() != 3 {
            return;
        }
        let choice = parts[1];
        let Some(query_id) = query.get("id").and_then(|v| v.as_str()) else {
            return;
        };
        let Ok(approval_id) = parts[2].parse::<u64>() else {
            answer_callback(client, token, query_id, "Invalid approval data.").await;
            return;
        };
        let from = query.get("from").cloned().unwrap_or(json!({}));
        let caller_id = from.get("id").map(|v| v.to_string()).unwrap_or_default();
        let user_display = from
            .get("first_name")
            .and_then(|v| v.as_str())
            .unwrap_or("User")
            .to_string();
        let message = query.get("message").cloned().unwrap_or(json!({}));
        let chat_id = message
            .get("chat")
            .and_then(|c| c.get("id"))
            .map(|v| match v {
                Value::Number(n) => n.to_string(),
                other => other.as_str().unwrap_or("").to_string(),
            })
            .unwrap_or_default();
        let message_id = message
            .get("message_id")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);

        // Auth union: configured allowlist OR an approved pairing code
        // (hermes `_is_callback_user_authorized`).
        let authorized = allowlisted(&cfg.allowed_chat_ids, &chat_id)
            || pairing
                .map(|store| store.is_approved("telegram", &caller_id))
                .unwrap_or(false);
        if !authorized {
            answer_callback(
                client,
                token,
                query_id,
                "\u{26D4} You are not authorized to approve commands.",
            )
            .await;
            return;
        }

        let Some(session_key) = approval_state().lock().unwrap().remove(&approval_id) else {
            answer_callback(
                client,
                token,
                query_id,
                "This approval has already been resolved.",
            )
            .await;
            return;
        };

        let choice_const = match choice {
            "once" => crate::approval_gateway::CHOICE_ONCE,
            "session" => crate::approval_gateway::CHOICE_SESSION,
            "always" => crate::approval_gateway::CHOICE_ALWAYS,
            "deny" => crate::approval_gateway::CHOICE_DENY,
            _ => {
                answer_callback(client, token, query_id, "Invalid approval data.").await;
                return;
            }
        };
        // Resolve FIRST, render after (hermes #63501): stale taps must not
        // claim success.
        let resolved = crate::approval_gateway::resolve(&session_key, choice_const);
        let (label, edit_text) = if resolved {
            let label = match choice {
                "once" => "\u{2705} Approved once",
                "session" => "\u{2705} Approved for session",
                "always" => "\u{2705} Approved permanently",
                _ => "\u{274C} Denied",
            };
            (label.to_string(), format!("{label} by {user_display}"))
        } else {
            (
                "\u{231B} Approval expired".to_string(),
                "\u{231B} Approval expired \u{2014} no command was waiting. It already timed                  out (and was denied) or was resolved elsewhere."
                    .to_string(),
            )
        };
        answer_callback(client, token, query_id, &label).await;
        edit_clarify_message(
            client,
            token,
            &chat_id,
            message_id,
            &html_escape(&edit_text),
        )
        .await;
    }

    /// answerCallbackQuery wrapper — always fire-and-forget; a failed
    /// answer only means the spinner lingers (hermes ignores errors too).
    async fn answer_callback(client: &reqwest::Client, token: &str, query_id: &str, text: &str) {
        let params = json!({"callback_query_id": query_id, "text": text});
        if let Err(e) = api(client, token, "answerCallbackQuery", params).await {
            eprintln!("[telegram] answerCallbackQuery failed: {e}");
        }
    }

    /// Edit the clarify prompt after a tap (hermes edit_message_text with
    /// reply_markup=None — the explicit null drops the inline keyboard).
    async fn edit_clarify_message(
        client: &reqwest::Client,
        token: &str,
        chat_id: &str,
        message_id: i64,
        text: &str,
    ) {
        let params = json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "text": text,
            "parse_mode": "HTML",
            "reply_markup": null,
        });
        if let Err(e) = api(client, token, "editMessageText", params).await {
            eprintln!("[telegram] editMessageText failed: {e}");
        }
    }

    /// Tell the user a clarify tap arrived too late to be delivered
    /// (hermes `_notify_clarify_expired`).
    async fn notify_clarify_expired(
        client: &reqwest::Client,
        token: &str,
        query_id: &str,
        chat_id: &str,
        message_id: i64,
        original_text: &str,
    ) {
        answer_callback(
            client,
            token,
            query_id,
            "⚠️ This prompt expired — please /retry.",
        )
        .await;
        let edited = format!(
            "❓ {}\n\n<i>⚠️ This question expired or the session reset — please /retry.</i>",
            html_escape(original_text)
        );
        edit_clarify_message(client, token, chat_id, message_id, &edited).await;
    }

    /// Send a reply, splitting at 4096-char Telegram limit (hermes chunks
    /// long replies the same way).
    /// Download the message's first media payload (photo > document >
    /// video > audio > voice, hermes precedence) via getFile and cache it.
    async fn download_media(
        client: &reqwest::Client,
        token: &str,
        message: &Value,
    ) -> Vec<MediaAttachment> {
        let (file_id, mime, name): (String, &str, String) =
            if let Some(photo) = message.get("photo").and_then(|v| v.as_array()) {
                // Bot API sends photos as a size ladder; last = largest.
                match photo
                    .last()
                    .and_then(|p| p.get("file_id").and_then(|v| v.as_str()))
                {
                    Some(file_id) => (file_id.to_string(), "image/jpeg", String::new()),
                    None => return Vec::new(),
                }
            } else if let Some(media) = message
                .get("document")
                .or_else(|| message.get("video"))
                .or_else(|| message.get("audio"))
                .or_else(|| message.get("voice"))
            {
                let Some(file_id) = media.get("file_id").and_then(|v| v.as_str()) else {
                    return Vec::new();
                };
                let mime = if message.get("voice").is_some() {
                    "audio/ogg"
                } else {
                    media
                        .get("mime_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                };
                let name = media
                    .get("file_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                (file_id.to_string(), mime, name)
            } else {
                return Vec::new();
            };
        let info = match api(client, token, "getFile", json!({"file_id": file_id})).await {
            Ok(info) => info,
            Err(e) => {
                eprintln!("[telegram] getFile failed: {e}");
                return Vec::new();
            }
        };
        let Some(file_path) = info.pointer("/result/file_path").and_then(|v| v.as_str()) else {
            return Vec::new();
        };
        let url = format!("{}/file/bot{token}/{file_path}", telegram_api_base());
        match client
            .get(&url)
            .send()
            .await
            .and_then(|r| r.error_for_status())
        {
            Ok(response) => match response.bytes().await {
                Ok(bytes) => cache_attachment(bytes.to_vec(), mime, &name)
                    .into_iter()
                    .collect(),
                Err(e) => {
                    eprintln!("[telegram] media download failed: {e}");
                    Vec::new()
                }
            },
            Err(e) => {
                eprintln!("[telegram] media download failed: {e}");
                Vec::new()
            }
        }
    }

    /// Send a photo (MEDIA: delivery, hermes extract_media path) with an
    /// optional native caption (send_message media path).
    pub async fn send_photo(
        client: &reqwest::Client,
        token: &str,
        chat_id: &str,
        path: &std::path::Path,
        caption: Option<&str>,
    ) {
        let Ok(data) = tokio::fs::read(path).await else {
            eprintln!("[telegram] cannot read media {}", path.display());
            return;
        };
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "media".to_string());
        let mut form = reqwest::multipart::Form::new()
            .text("chat_id", chat_id.to_string())
            .part(
                "photo",
                reqwest::multipart::Part::bytes(data).file_name(file_name),
            );
        if let Some(caption) = caption {
            form = form.text("caption", caption.to_string());
        }
        let url = format!("{API}/bot{token}/sendPhoto");
        if let Err(e) = client.post(&url).multipart(form).send().await {
            eprintln!("[telegram] sendPhoto failed: {e}");
        }
    }

    /// Send a non-image file (MEDIA: delivery) with an optional native
    /// caption (send_message media path).
    pub async fn send_document(
        client: &reqwest::Client,
        token: &str,
        chat_id: &str,
        path: &std::path::Path,
        caption: Option<&str>,
    ) {
        let Ok(data) = tokio::fs::read(path).await else {
            eprintln!("[telegram] cannot read media {}", path.display());
            return;
        };
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "media".to_string());
        let mut form = reqwest::multipart::Form::new()
            .text("chat_id", chat_id.to_string())
            .part(
                "document",
                reqwest::multipart::Part::bytes(data).file_name(file_name),
            );
        if let Some(caption) = caption {
            form = form.text("caption", caption.to_string());
        }
        let url = format!("{API}/bot{token}/sendDocument");
        if let Err(e) = client.post(&url).multipart(form).send().await {
            eprintln!("[telegram] sendDocument failed: {e}");
        }
    }

    /// hermes `_CAPTIONABLE_EXTS` — media kinds whose bubble carries a
    /// native caption (voice/audio notes excluded).
    fn captionable_ext(path: &std::path::Path) -> bool {
        matches!(
            path.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_lowercase())
                .as_deref(),
            Some(
                "jpg"
                    | "jpeg"
                    | "png"
                    | "webp"
                    | "gif"
                    | "mp4"
                    | "mov"
                    | "avi"
                    | "mkv"
                    | "webm"
                    | "3gp"
                    | "pdf"
                    | "doc"
                    | "docx"
                    | "txt"
                    | "md"
                    | "csv"
                    | "xlsx"
                    | "zip"
            )
        )
    }

    /// hermes `_media_caption_split`: a single captionable file with short
    /// accompanying text rides on the media bubble as its caption;
    /// everything else (multi-file, voice notes, long text) keeps the text
    /// as a separate body message. Returns `(caption, body)`.
    pub fn media_caption_split(
        text: &str,
        paths: &[std::path::PathBuf],
        limit: usize,
    ) -> (Option<String>, String) {
        let stripped = text.trim();
        if stripped.is_empty() || paths.len() != 1 || !captionable_ext(&paths[0]) {
            return (None, text.to_string());
        }
        if stripped.chars().count() > limit {
            return (None, text.to_string());
        }
        (Some(stripped.to_string()), String::new())
    }

    pub async fn send_message(client: &reqwest::Client, token: &str, chat_id: &str, text: &str) {
        for chunk in chunk_text(text, 4000) {
            let params = json!({"chat_id": chat_id, "text": chunk});
            if let Err(e) = api(client, token, "sendMessage", params).await {
                eprintln!("[telegram] sendMessage failed: {e}");
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// axum mock of the Bot API — logs (method, body) per call.
        async fn spawn_telegram_api(
            log: Arc<std::sync::Mutex<Vec<(String, Value)>>>,
            response_ok: bool,
        ) -> String {
            use axum::extract::State;
            use axum::routing::post;
            let app = axum::Router::new()
                .route(
                    "/botTEST/:method",
                    post(
                        move |State(log): State<Arc<std::sync::Mutex<Vec<(String, Value)>>>>,
                              axum::extract::Path(method): axum::extract::Path<String>,
                              axum::Json(body): axum::Json<Value>| async move {
                            log.lock().unwrap().push((method, body));
                            axum::Json(json!({ "ok": response_ok, "result": {} }))
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

        fn clarify_query(clarify_id: &str, token: &str) -> Value {
            json!({
                "id": "cb-1",
                "from": {"id": 7, "first_name": "Ann"},
                "data": format!("cl:{clarify_id}:{token}"),
                "message": {
                    "message_id": 99,
                    "text": "❓ Pick\n\n1. Alpha\n2. Beta",
                    "chat": {"id": 42},
                },
            })
        }

        fn authorized_cfg() -> TelegramConfig {
            TelegramConfig {
                allowed_chat_ids: vec!["42".into()],
                ..Default::default()
            }
        }

        #[tokio::test]
        async fn telegram_send_clarify_renders_inline_keyboard() {
            let _env_guard = crate::models_dev::test_env_lock();
            let log = Arc::new(std::sync::Mutex::new(Vec::<(String, Value)>::new()));
            let base = spawn_telegram_api(log.clone(), true).await;
            std::env::set_var("TELEGRAM_API_BASE", &base);
            let ok = send_clarify_message(
                &reqwest::Client::new(),
                "TEST",
                "42",
                "abc123def456",
                "Pick <one>",
                &["Alpha & A".into(), "Beta".into()],
            )
            .await;
            std::env::remove_var("TELEGRAM_API_BASE");
            assert!(ok);
            let reqs = log.lock().unwrap();
            assert_eq!(reqs.len(), 1);
            let (method, body) = &reqs[0];
            assert_eq!(method, "sendMessage");
            let text = body["text"].as_str().unwrap();
            // HTML parse mode with escaped question + numbered options in
            // the body (hermes mobile readability layout).
            assert_eq!(body["parse_mode"], "HTML");
            assert!(text.starts_with("❓ Pick &lt;one&gt;"));
            assert!(text.contains("1. Alpha &amp; A"));
            assert!(text.contains("2. Beta"));
            // One row per choice (short numeric labels) + Other row, with
            // cl:<id>:<idx|other> callback payloads.
            let rows = body["reply_markup"]["inline_keyboard"].as_array().unwrap();
            assert_eq!(rows.len(), 3);
            assert_eq!(rows[0][0]["text"], "1");
            assert_eq!(rows[0][0]["callback_data"], "cl:abc123def456:0");
            assert_eq!(rows[1][0]["text"], "2");
            assert_eq!(rows[1][0]["callback_data"], "cl:abc123def456:1");
            assert_eq!(rows[2][0]["text"], "✏️ Other (type answer)");
            assert_eq!(rows[2][0]["callback_data"], "cl:abc123def456:other");
        }

        #[tokio::test]
        async fn telegram_send_clarify_api_failure_returns_false() {
            let _env_guard = crate::models_dev::test_env_lock();
            let log = Arc::new(std::sync::Mutex::new(Vec::<(String, Value)>::new()));
            let base = spawn_telegram_api(log.clone(), false).await;
            std::env::set_var("TELEGRAM_API_BASE", &base);
            let ok = send_clarify_message(
                &reqwest::Client::new(),
                "TEST",
                "42",
                "abc123def456",
                "Pick",
                &["Alpha".into()],
            )
            .await;
            std::env::remove_var("TELEGRAM_API_BASE");
            // False → messaging_clarify_fn sends the numbered-text fallback.
            assert!(!ok);
        }

        #[tokio::test]
        async fn telegram_callback_numeric_choice_resolves_clarify() {
            let _env_guard = crate::models_dev::test_env_lock();
            let _clarify_guard = crate::clarify_gateway::test_lock().lock().unwrap();
            crate::clarify_gateway::reset_for_tests();
            let log = Arc::new(std::sync::Mutex::new(Vec::<(String, Value)>::new()));
            let base = spawn_telegram_api(log.clone(), true).await;
            std::env::set_var("TELEGRAM_API_BASE", &base);
            let handle = crate::clarify_gateway::register(
                "platform-telegram-42",
                "Pick",
                &["Alpha".into(), "Beta".into()],
                false,
            );
            let clarify_id = handle.clarify_id.clone();
            let query = clarify_query(&clarify_id, "1");
            handle_callback_query(
                &reqwest::Client::new(),
                "TEST",
                &authorized_cfg(),
                None,
                &query,
            )
            .await;
            std::env::remove_var("TELEGRAM_API_BASE");
            // The tool waiter received the tapped choice text.
            assert_eq!(handle.rx.await.unwrap(), "Beta");
            let reqs = log.lock().unwrap();
            let methods: Vec<&str> = reqs.iter().map(|(m, _)| m.as_str()).collect();
            assert_eq!(methods, vec!["answerCallbackQuery", "editMessageText"]);
            assert_eq!(reqs[0].1["text"], "✓ Beta");
            let edit = &reqs[1].1;
            assert!(edit["text"].as_str().unwrap().contains("<b>Ann:</b> Beta"));
            // Explicit null drops the inline keyboard.
            assert_eq!(edit["reply_markup"], Value::Null);
        }

        #[tokio::test]
        async fn telegram_callback_other_flips_to_text_capture() {
            let _env_guard = crate::models_dev::test_env_lock();
            let _clarify_guard = crate::clarify_gateway::test_lock().lock().unwrap();
            crate::clarify_gateway::reset_for_tests();
            let log = Arc::new(std::sync::Mutex::new(Vec::<(String, Value)>::new()));
            let base = spawn_telegram_api(log.clone(), true).await;
            std::env::set_var("TELEGRAM_API_BASE", &base);
            let handle = crate::clarify_gateway::register(
                "platform-telegram-42",
                "Pick",
                &["Alpha".into(), "Beta".into()],
                false,
            );
            let clarify_id = handle.clarify_id.clone();
            let query = clarify_query(&clarify_id, "other");
            handle_callback_query(
                &reqwest::Client::new(),
                "TEST",
                &authorized_cfg(),
                None,
                &query,
            )
            .await;
            std::env::remove_var("TELEGRAM_API_BASE");
            // Entry survives in text-capture mode; the next message in the
            // session resolves it (text intercept).
            let pending = crate::clarify_gateway::pending_for_session("platform-telegram-42")
                .expect("entry must survive the Other tap");
            assert!(pending.awaiting_text);
            assert!(crate::clarify_gateway::resolve(&clarify_id, "typed answer"));
            assert_eq!(handle.rx.await.unwrap(), "typed answer");
            let reqs = log.lock().unwrap();
            let methods: Vec<&str> = reqs.iter().map(|(m, _)| m.as_str()).collect();
            assert_eq!(methods, vec!["answerCallbackQuery", "editMessageText"]);
            assert_eq!(reqs[0].1["text"], "✏️ Type your answer in the chat.");
            assert!(reqs[1].1["text"]
                .as_str()
                .unwrap()
                .contains("Awaiting typed response from Ann…"));
        }

        #[tokio::test]
        async fn telegram_callback_stale_entry_answers_resolved() {
            let _env_guard = crate::models_dev::test_env_lock();
            let _clarify_guard = crate::clarify_gateway::test_lock().lock().unwrap();
            crate::clarify_gateway::reset_for_tests();
            let log = Arc::new(std::sync::Mutex::new(Vec::<(String, Value)>::new()));
            let base = spawn_telegram_api(log.clone(), true).await;
            std::env::set_var("TELEGRAM_API_BASE", &base);
            // No entry registered — a tap on a resolved/unknown prompt.
            let query = clarify_query("zzzzzzzzzzzz", "0");
            handle_callback_query(
                &reqwest::Client::new(),
                "TEST",
                &authorized_cfg(),
                None,
                &query,
            )
            .await;
            std::env::remove_var("TELEGRAM_API_BASE");
            let reqs = log.lock().unwrap();
            assert_eq!(reqs.len(), 1);
            assert_eq!(reqs[0].0, "answerCallbackQuery");
            assert_eq!(reqs[0].1["text"], "This prompt has already been resolved.");
        }

        #[tokio::test]
        async fn telegram_callback_unauthorized_rejected() {
            let _env_guard = crate::models_dev::test_env_lock();
            let _clarify_guard = crate::clarify_gateway::test_lock().lock().unwrap();
            crate::clarify_gateway::reset_for_tests();
            let log = Arc::new(std::sync::Mutex::new(Vec::<(String, Value)>::new()));
            let base = spawn_telegram_api(log.clone(), true).await;
            std::env::set_var("TELEGRAM_API_BASE", &base);
            let handle = crate::clarify_gateway::register(
                "platform-telegram-42",
                "Pick",
                &["Alpha".into()],
                false,
            );
            let clarify_id = handle.clarify_id.clone();
            let query = clarify_query(&clarify_id, "0");
            // Chat 42 is not allowlisted and no pairing store exists.
            let cfg = TelegramConfig {
                allowed_chat_ids: vec!["999".into()],
                ..Default::default()
            };
            handle_callback_query(&reqwest::Client::new(), "TEST", &cfg, None, &query).await;
            std::env::remove_var("TELEGRAM_API_BASE");
            let reqs = log.lock().unwrap();
            assert_eq!(reqs.len(), 1);
            assert_eq!(
                reqs[0].1["text"],
                "⛔ You are not authorized to answer this prompt."
            );
            // The waiter is untouched — the legitimate user can still answer.
            assert!(crate::clarify_gateway::contains(&clarify_id));
            assert!(crate::clarify_gateway::resolve(&clarify_id, "Alpha"));
        }

        #[tokio::test]
        async fn telegram_callback_expired_when_waiter_gone() {
            let _env_guard = crate::models_dev::test_env_lock();
            let _clarify_guard = crate::clarify_gateway::test_lock().lock().unwrap();
            crate::clarify_gateway::reset_for_tests();
            let log = Arc::new(std::sync::Mutex::new(Vec::<(String, Value)>::new()));
            let base = spawn_telegram_api(log.clone(), true).await;
            std::env::set_var("TELEGRAM_API_BASE", &base);
            let handle = crate::clarify_gateway::register(
                "platform-telegram-42",
                "Pick",
                &["Alpha".into(), "Beta".into()],
                false,
            );
            let clarify_id = handle.clarify_id.clone();
            // The clarify tool gave up (receiver dropped) before the tap.
            drop(handle.rx);
            let query = clarify_query(&clarify_id, "1");
            handle_callback_query(
                &reqwest::Client::new(),
                "TEST",
                &authorized_cfg(),
                None,
                &query,
            )
            .await;
            std::env::remove_var("TELEGRAM_API_BASE");
            let reqs = log.lock().unwrap();
            let methods: Vec<&str> = reqs.iter().map(|(m, _)| m.as_str()).collect();
            assert_eq!(methods, vec!["answerCallbackQuery", "editMessageText"]);
            assert_eq!(reqs[0].1["text"], "⚠️ This prompt expired — please /retry.");
            assert!(reqs[1].1["text"]
                .as_str()
                .unwrap()
                .contains("This question expired or the session reset"));
        }

        // ------------------------------------------------------------------
        // Exec-approval inline keyboards (hermes send_exec_approval, P225)
        // ------------------------------------------------------------------

        fn approval_query(approval_id: u64, choice: &str) -> Value {
            json!({
                "id": "cb-ea",
                "from": {"id": 7, "first_name": "Ann"},
                "data": format!("ea:{choice}:{approval_id}"),
                "message": {
                    "message_id": 77,
                    "text": "approval",
                    "chat": {"id": 42},
                },
            })
        }

        #[test]
        fn telegram_approval_format_escapes_and_smart_deny() {
            let text = format_exec_approval_html(
                "cat <secret> && rm -rf /",
                "dangerous <b>cmd</b>",
                false,
            );
            assert!(text.starts_with("\u{26A0}\u{FE0F} <b>Command Approval Required</b>"));
            assert!(text.contains("<pre>cat &lt;secret&gt; &amp;&amp; rm -rf /</pre>"));
            assert!(text.contains("dangerous &lt;b&gt;cmd&lt;/b&gt;"));
            assert!(!text.contains("Smart DENY"));

            let denied = format_exec_approval_html("rm -rf /", "", true);
            assert!(denied.contains("<b>Smart DENY:</b> owner override"));
        }

        #[test]
        fn telegram_approval_format_caps_command_budget() {
            let long = "x".repeat(5000);
            let text = format_exec_approval_html(&long, "", false);
            // 3800-char budget + "..." ellipsis inside the <pre> block.
            assert!(text.contains(&"x".repeat(3800)));
            assert!(!text.contains(&"x".repeat(3801)));
        }

        #[tokio::test]
        async fn telegram_approval_keyboard_layout() {
            let _env_guard = crate::models_dev::test_env_lock();
            reset_approval_state_for_tests();
            let log = Arc::new(std::sync::Mutex::new(Vec::<(String, Value)>::new()));
            let base = spawn_telegram_api(log.clone(), true).await;
            std::env::set_var("TELEGRAM_API_BASE", &base);
            let ok = send_approval_message(
                &reqwest::Client::new(),
                "TEST",
                "42",
                "platform-telegram-42",
                "rm -rf /tmp/x",
                "dangerous command",
                true,
                true,
                false,
            )
            .await;
            // Smart-denied variant: only Allow Once + Deny.
            let ok_denied = send_approval_message(
                &reqwest::Client::new(),
                "TEST",
                "42",
                "platform-telegram-42",
                "rm -rf /tmp/y",
                "smart denied",
                true,
                true,
                true,
            )
            .await;
            std::env::remove_var("TELEGRAM_API_BASE");
            assert!(ok && ok_denied);
            let reqs = log.lock().unwrap();
            // Full set: 4 buttons paired into 2 rows of 2 (hermes mobile
            // layout — a single 4-button row truncates).
            let body = &reqs[0].1;
            let rows = body["reply_markup"]["inline_keyboard"].as_array().unwrap();
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0].as_array().unwrap().len(), 2);
            let datas: Vec<&str> = rows
                .iter()
                .flat_map(|r| r.as_array().unwrap())
                .map(|b| b["callback_data"].as_str().unwrap())
                .collect();
            assert_eq!(datas.len(), 4);
            assert!(datas[0].starts_with("ea:once:"));
            assert!(datas[1].starts_with("ea:session:"));
            assert!(datas[2].starts_with("ea:always:"));
            assert!(datas[3].starts_with("ea:deny:"));
            // Smart-denied: Allow Once + Deny only, one row.
            let denied_body = &reqs[1].1;
            let denied_rows = denied_body["reply_markup"]["inline_keyboard"]
                .as_array()
                .unwrap();
            let denied_datas: Vec<&str> = denied_rows
                .iter()
                .flat_map(|r| r.as_array().unwrap())
                .map(|b| b["callback_data"].as_str().unwrap())
                .collect();
            assert_eq!(denied_datas.len(), 2);
            assert!(denied_datas[0].starts_with("ea:once:"));
            assert!(denied_datas[1].starts_with("ea:deny:"));
            // Both prompts registered their session in the approval state.
            assert_eq!(approval_state().lock().unwrap().len(), 2);
            reset_approval_state_for_tests();
        }

        #[tokio::test]
        async fn telegram_approval_button_resolves_pending() {
            let _env_guard = crate::models_dev::test_env_lock();
            reset_approval_state_for_tests();
            let log = Arc::new(std::sync::Mutex::new(Vec::<(String, Value)>::new()));
            let base = spawn_telegram_api(log.clone(), true).await;
            std::env::set_var("TELEGRAM_API_BASE", &base);
            let session_key = "platform-telegram-ea-resolve";
            let handle = crate::approval_gateway::register(
                session_key,
                "rm -rf /tmp/x",
                "dangerous command",
                false,
                true,
                true,
            );
            approval_state()
                .lock()
                .unwrap()
                .insert(4242, session_key.to_string());
            let query = approval_query(4242, "once");
            handle_callback_query(
                &reqwest::Client::new(),
                "TEST",
                &authorized_cfg(),
                None,
                &query,
            )
            .await;
            std::env::remove_var("TELEGRAM_API_BASE");
            // The agent waiter received the tapped choice.
            assert_eq!(handle.rx.await.unwrap(), "once");
            // The state entry was popped (one tap resolves one prompt).
            assert!(!approval_state().lock().unwrap().contains_key(&4242));
            let reqs = log.lock().unwrap();
            let methods: Vec<&str> = reqs.iter().map(|(m, _)| m.as_str()).collect();
            assert_eq!(methods, vec!["answerCallbackQuery", "editMessageText"]);
            assert!(reqs[0].1["text"]
                .as_str()
                .unwrap()
                .contains("Approved once"));
            assert!(reqs[1].1["text"]
                .as_str()
                .unwrap()
                .contains("Approved once by Ann"));
        }

        #[tokio::test]
        async fn telegram_approval_button_expired_when_nothing_pending() {
            let _env_guard = crate::models_dev::test_env_lock();
            reset_approval_state_for_tests();
            let log = Arc::new(std::sync::Mutex::new(Vec::<(String, Value)>::new()));
            let base = spawn_telegram_api(log.clone(), true).await;
            std::env::set_var("TELEGRAM_API_BASE", &base);
            // State maps to a session with NO pending approval (the wait
            // timed out and was denied) — the tap must not claim success
            // (hermes #63501).
            approval_state()
                .lock()
                .unwrap()
                .insert(4343, "platform-telegram-ea-expired".to_string());
            let query = approval_query(4343, "once");
            handle_callback_query(
                &reqwest::Client::new(),
                "TEST",
                &authorized_cfg(),
                None,
                &query,
            )
            .await;
            std::env::remove_var("TELEGRAM_API_BASE");
            let reqs = log.lock().unwrap();
            assert!(reqs[0].1["text"]
                .as_str()
                .unwrap()
                .contains("Approval expired"));
            assert!(reqs[1].1["text"]
                .as_str()
                .unwrap()
                .contains("no command was waiting"));
        }

        #[tokio::test]
        async fn telegram_approval_button_unauthorized() {
            let _env_guard = crate::models_dev::test_env_lock();
            reset_approval_state_for_tests();
            let log = Arc::new(std::sync::Mutex::new(Vec::<(String, Value)>::new()));
            let base = spawn_telegram_api(log.clone(), true).await;
            std::env::set_var("TELEGRAM_API_BASE", &base);
            approval_state()
                .lock()
                .unwrap()
                .insert(4444, "platform-telegram-ea-auth".to_string());
            let query = approval_query(4444, "once");
            let empty_cfg = TelegramConfig::default();
            handle_callback_query(&reqwest::Client::new(), "TEST", &empty_cfg, None, &query).await;
            std::env::remove_var("TELEGRAM_API_BASE");
            // Rejected — and the state entry stays intact for an
            // authorized tap.
            assert!(approval_state().lock().unwrap().contains_key(&4444));
            let reqs = log.lock().unwrap();
            assert_eq!(reqs.len(), 1);
            assert_eq!(reqs[0].0, "answerCallbackQuery");
            assert!(reqs[0].1["text"]
                .as_str()
                .unwrap()
                .contains("not authorized"));
            reset_approval_state_for_tests();
        }
    }
}

// ---------------------------------------------------------------------------
// Discord — Gateway v10 websocket + REST
// ---------------------------------------------------------------------------

pub mod discord {
    use super::*;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    const GATEWAY: &str = "wss://gateway.discord.gg/?v=10&encoding=json";
    const REST: &str = "https://discord.com/api/v10";
    const INTENT_GUILD_MESSAGES: u64 = 1 << 9;
    const INTENT_DIRECT_MESSAGES: u64 = 1 << 12;
    const INTENT_MESSAGE_CONTENT: u64 = 1 << 15;

    /// Discord REST base URL — normally `https://discord.com/api/v10`;
    /// the `DISCORD_API_BASE` override exists for tests and corporate
    /// proxies (mirrors the slack/telegram pattern).
    pub(crate) fn discord_api_base() -> String {
        std::env::var("DISCORD_API_BASE")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| REST.to_string())
    }

    struct Sender {
        token: String,
        prompt_timeout_secs: u64,
    }

    #[async_trait::async_trait]
    impl PlatformSender for Sender {
        async fn send_text(&self, chat_id: &str, text: &str) {
            send_channel_message(&self.token, chat_id, text).await;
        }

        /// Embed + button clarify prompt (hermes Discord `send_clarify`).
        /// Returns false on API failure so the numbered-text fallback still
        /// goes out.
        async fn send_clarify(
            &self,
            chat_id: &str,
            clarify_id: &str,
            question: &str,
            choices: &[String],
        ) -> bool {
            send_clarify_message(
                &self.token,
                chat_id,
                clarify_id,
                question,
                choices,
                self.prompt_timeout_secs,
            )
            .await
        }

        /// Emoji reactions (hermes send_message action='react'/'unreact'):
        /// PUT/DELETE `/channels/{c}/messages/{m}/reactions/{emoji}/@me`.
        async fn send_reaction(
            &self,
            chat_id: &str,
            emoji: &str,
            message_id: &str,
            remove: bool,
        ) -> Option<bool> {
            let encoded: String =
                url::form_urlencoded::byte_serialize(emoji.trim().as_bytes()).collect();
            let url = format!(
                "{}/channels/{chat_id}/messages/{message_id}/reactions/{encoded}/@me",
                discord_api_base()
            );
            let client = reqwest::Client::new();
            let request = if remove {
                client.delete(&url)
            } else {
                client.put(&url)
            };
            match request
                .header("Authorization", format!("Bot {}", self.token))
                .send()
                .await
            {
                Ok(response) => Some(response.status().is_success()),
                Err(e) => {
                    eprintln!("[discord] reaction failed: {e}");
                    Some(false)
                }
            }
        }

        /// Native attachment delivery (hermes send_message MEDIA: path):
        /// one multipart message with payload_json content + files.
        async fn send_media(
            &self,
            chat_id: &str,
            text: &str,
            paths: &[std::path::PathBuf],
        ) -> bool {
            send_media_message(&self.token, chat_id, text, paths).await
        }
    }

    pub async fn run(
        cfg: DiscordConfig,
        dispatcher: Arc<Dispatcher>,
        pairing: Option<Arc<crate::pairing::PairingStore>>,
    ) {
        let Some(token) = resolve_token(&cfg.bot_token, "DISCORD_BOT_TOKEN") else {
            eprintln!("[discord] disabled: no bot_token configured (set messaging.discord.bot_token or DISCORD_BOT_TOKEN)");
            return;
        };
        register_platform_sender(
            "discord",
            Arc::new(Sender {
                token: token.clone(),
                prompt_timeout_secs: cfg.prompt_timeout_secs,
            }),
        );
        loop {
            if let Err(e) = run_session(&cfg, &token, dispatcher.clone(), pairing.clone()).await {
                eprintln!("[discord] gateway session ended: {e} — reconnecting in 5s");
            }
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    }

    async fn run_session(
        cfg: &DiscordConfig,
        token: &str,
        dispatcher: Arc<Dispatcher>,
        pairing: Option<Arc<crate::pairing::PairingStore>>,
    ) -> Result<()> {
        use futures::{SinkExt, StreamExt};
        let (ws, _) = tokio_tungstenite::connect_async(GATEWAY)
            .await
            .map_err(|e| AgentError::Tool(format!("discord connect: {e}")))?;
        eprintln!("[discord] gateway connected");
        let (mut sink, mut stream) = ws.split();
        let mut heartbeat_interval = std::time::Duration::from_secs(41);
        let mut last_sequence: Option<u64> = None;
        let mut identified = false;
        let mut heartbeat_due = tokio::time::Instant::now() + heartbeat_interval;

        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(heartbeat_due) => {
                    let payload = json!({"op": 1, "d": last_sequence});
                    sink.send(WsMessage::Text(payload.to_string())).await.ok();
                    heartbeat_due = tokio::time::Instant::now() + heartbeat_interval;
                }
                message = stream.next() => {
                    let Some(Ok(message)) = message else {
                        return Err(AgentError::Tool("discord gateway closed".into()));
                    };
                    let WsMessage::Text(text) = message else { continue };
                    let Ok(payload) = serde_json::from_str::<Value>(&text) else { continue };
                    let op = payload.get("op").and_then(|v| v.as_u64()).unwrap_or(u64::MAX);
                    if let Some(seq) = payload.get("s").and_then(|v| v.as_u64()) {
                        last_sequence = Some(seq);
                    }
                    match op {
                        10 => {
                            heartbeat_interval = std::time::Duration::from_millis(
                                payload.pointer("/d/heartbeat_interval").and_then(|v| v.as_u64()).unwrap_or(41250),
                            );
                            heartbeat_due = tokio::time::Instant::now() + heartbeat_interval;
                            if !identified {
                                let identify = json!({
                                    "op": 2,
                                    "d": {
                                        "token": token,
                                        "intents": INTENT_GUILD_MESSAGES | INTENT_DIRECT_MESSAGES | INTENT_MESSAGE_CONTENT,
                                        "properties": {"os": "linux", "browser": "ulnclaw", "device": "ulnclaw"},
                                    }
                                });
                                sink.send(WsMessage::Text(identify.to_string())).await.ok();
                                identified = true;
                            }
                        }
                        11 => {} // heartbeat ACK
                        1 => {
                            let payload = json!({"op": 1, "d": last_sequence});
                            sink.send(WsMessage::Text(payload.to_string())).await.ok();
                        }
                        0 => {
                            let event_name = payload.get("t").and_then(|v| v.as_str()).unwrap_or("");
                            if event_name == "READY" {
                                let username = payload.pointer("/d/user/username").and_then(|v| v.as_str()).unwrap_or("?");
                                eprintln!("[discord] logged in as {username}");
                            }
                            if event_name == "MESSAGE_CREATE" {
                                handle_message_create(cfg, token, &dispatcher, payload.get("d").cloned().unwrap_or(json!({})), pairing.as_deref()).await;
                            }
                            // Clarify button taps arrive as MESSAGE_COMPONENT
                            // interactions (hermes ClarifyChoiceView routing).
                            if event_name == "INTERACTION_CREATE" {
                                handle_interaction_create(cfg, token, payload.get("d").cloned().unwrap_or(json!({})), pairing.as_deref()).await;
                            }
                        }
                        7 | 9 => {
                            return Err(AgentError::Tool("discord requested reconnect / invalid session".into()));
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    async fn handle_message_create(
        cfg: &DiscordConfig,
        token: &str,
        dispatcher: &Arc<Dispatcher>,
        data: Value,
        pairing: Option<&crate::pairing::PairingStore>,
    ) {
        // Ignore bot/webhook messages (never talk to ourselves).
        if data
            .get("author")
            .and_then(|a| a.get("bot"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            return;
        }
        let text = data.get("content").and_then(|v| v.as_str()).unwrap_or("");
        // Attachment downloads run before any gate so media-only messages
        // still flow (hermes). Discord exposes url/filename/content_type.
        let attachments = {
            let client = reqwest::Client::new();
            let mut downloaded = Vec::new();
            if let Some(items) = data.get("attachments").and_then(|v| v.as_array()) {
                for item in items {
                    let Some(url) = item.get("url").and_then(|v| v.as_str()) else {
                        continue;
                    };
                    let name = item.get("filename").and_then(|v| v.as_str()).unwrap_or("");
                    let mime = item
                        .get("content_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let size = item.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
                    if let Some(attachment) =
                        download_to_cache(&client, url, None, mime, name, size).await
                    {
                        downloaded.push(attachment);
                    }
                }
            }
            downloaded
        };
        if text.is_empty() && attachments.is_empty() {
            return;
        }
        let channel_id = data
            .get("channel_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if channel_id.is_empty() {
            return;
        }
        let author = data.get("author").cloned().unwrap_or(json!({}));
        let mut event = MessageEvent {
            platform: "discord".into(),
            chat_id: channel_id.clone(),
            sender_id: author
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            sender_name: author
                .get("global_name")
                .or_else(|| author.get("username"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            text: text.to_string(),
            message_id: data
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            attachments,
        };
        // Plugin gate before auth (hermes ordering).
        if !pre_gateway_dispatch_gate(&mut event).await {
            return;
        }
        // Auth union: configured allowlist OR an approved pairing code.
        let authorized = allowlisted(&cfg.allowed_channel_ids, &channel_id)
            || pairing
                .map(|store| store.is_approved("discord", &event.sender_id))
                .unwrap_or(false);
        if !authorized {
            eprintln!(
                "[discord] refusing message from channel {channel_id} — add it to \
                 messaging.discord.allowed_channel_ids or approve a pairing code"
            );
            if let Some(store) = pairing {
                if let Some(reply) =
                    pairing_offer(store, "discord", &event.sender_id, &event.sender_name)
                {
                    send_channel_message(token, &channel_id, &reply).await;
                }
            }
            return;
        }
        let dispatcher = dispatcher.clone();
        let token = token.to_string();
        tokio::spawn(async move {
            let outcome = match dispatcher.handle_event(event.clone()).await {
                Ok(outcome) => outcome,
                Err(e) => crate::messaging::DispatchOutcome {
                    reply: format!("error: {e}"),
                    transcript_echoes: Vec::new(),
                },
            };
            for echo in &outcome.transcript_echoes {
                send_channel_message(&token, &event.chat_id, echo).await;
            }
            let reply = outcome.reply;
            let (reply_text, media_paths) = extract_media_tags(&reply);
            if !reply_text.trim().is_empty() {
                // P703: ledger-protected reply delivery.
                dispatcher
                    .send_with_ledger("discord", &event.chat_id, &reply_text, || {
                        send_channel_message(&token, &event.chat_id, &reply_text)
                    })
                    .await;
            }
            for path in &media_paths {
                send_attachment(&token, &event.chat_id, path).await;
            }
        });
    }

    /// Upload a file as a native Discord attachment (MEDIA: delivery).
    async fn send_attachment(token: &str, channel_id: &str, path: &std::path::Path) {
        let Ok(data) = tokio::fs::read(path).await else {
            eprintln!("[discord] cannot read media {}", path.display());
            return;
        };
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "media".to_string());
        let form = reqwest::multipart::Form::new().part(
            "files[0]",
            reqwest::multipart::Part::bytes(data).file_name(file_name),
        );
        let url = format!("{}/channels/{channel_id}/messages", discord_api_base());
        let response = reqwest::Client::new()
            .post(&url)
            .header("Authorization", format!("Bot {token}"))
            .multipart(form)
            .send()
            .await;
        if let Err(e) = response.and_then(|r| r.error_for_status()) {
            eprintln!("[discord] attachment upload failed: {e}");
        }
    }

    /// One multipart message carrying content + native attachments
    /// (hermes send_message MEDIA: delivery for Discord). Long content is
    /// sent first as regular chunked messages so the attachment keeps the
    /// 2000-char payload_json limit.
    pub async fn send_media_message(
        token: &str,
        channel_id: &str,
        text: &str,
        paths: &[std::path::PathBuf],
    ) -> bool {
        let mut form = reqwest::multipart::Form::new();
        let trimmed = text.trim();
        if !trimmed.is_empty() && trimmed.chars().count() <= 2000 {
            form = form.part(
                "payload_json",
                reqwest::multipart::Part::text(json!({"content": trimmed}).to_string()),
            );
        } else if !trimmed.is_empty() {
            send_channel_message(token, channel_id, trimmed).await;
        }
        for (idx, path) in paths.iter().enumerate() {
            let Ok(data) = tokio::fs::read(path).await else {
                eprintln!("[discord] cannot read media {}", path.display());
                continue;
            };
            let file_name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "media".to_string());
            form = form.part(
                format!("files[{idx}]"),
                reqwest::multipart::Part::bytes(data).file_name(file_name),
            );
        }
        let url = format!("{}/channels/{channel_id}/messages", discord_api_base());
        match reqwest::Client::new()
            .post(&url)
            .header("Authorization", format!("Bot {token}"))
            .multipart(form)
            .send()
            .await
            .and_then(|r| r.error_for_status())
        {
            Ok(_) => true,
            Err(e) => {
                eprintln!("[discord] media send failed: {e}");
                false
            }
        }
    }

    /// POST /channels/{id}/messages with 2000-char Discord chunking.
    pub async fn send_channel_message(token: &str, channel_id: &str, text: &str) {
        let client = reqwest::Client::new();
        for chunk in chunk_text(text, 1900) {
            let result = client
                .post(format!(
                    "{}/channels/{channel_id}/messages",
                    discord_api_base()
                ))
                .header("Authorization", format!("Bot {token}"))
                .json(&json!({"content": chunk}))
                .send()
                .await;
            if let Err(e) = result {
                eprintln!("[discord] send failed: {e}");
            }
        }
    }

    // -- Clarify buttons — hermes ClarifyChoiceView parity -----------------

    const BUTTON_LABEL_LIMIT: usize = 80; // Discord button-label UTF-16 cap
    const MAX_CHOICES: usize = 24; // 25 buttons per message − the Other slot

    /// hermes `approvals.discord_prompt_timeout` semantics: default 300 s,
    /// clamped to 30–900 s so prompts never outlive Discord's 15-minute
    /// interaction-token expiry.
    fn prompt_timeout(cfg_timeout_secs: u64) -> std::time::Duration {
        let secs = if cfg_timeout_secs == 0 {
            300
        } else {
            cfg_timeout_secs.clamp(30, 900)
        };
        std::time::Duration::from_secs(secs)
    }

    fn utf16_len(text: &str) -> usize {
        text.chars().map(|c| c.len_utf16()).sum()
    }

    fn prefix_within_utf16_limit(text: &str, limit: usize) -> String {
        let mut out = String::new();
        let mut used = 0;
        for ch in text.chars() {
            let width = ch.len_utf16();
            if used + width > limit {
                break;
            }
            out.push(ch);
            used += width;
        }
        out
    }

    fn truncate_chars(text: &str, max_chars: usize) -> String {
        text.chars().take(max_chars).collect()
    }

    /// Discord button labels cap at 80 UTF-16 units and mobile width is
    /// much narrower, so hermes cuts long labels at a word boundary when
    /// possible: last space in the trailing half, then the latest soft
    /// punctuation, then a hard cut — always ending with an ellipsis.
    fn clarify_button_label(idx: usize, choice: &str) -> String {
        let prefix = format!("{}. ", idx + 1);
        let budget = BUTTON_LABEL_LIMIT.saturating_sub(utf16_len(&prefix));
        if utf16_len(choice) <= budget {
            return format!("{prefix}{choice}");
        }
        let truncated: Vec<char> = prefix_within_utf16_limit(choice, budget.saturating_sub(1))
            .trim_end()
            .chars()
            .collect();
        let len = truncated.len();
        let half = len / 2;
        let mut cut = len; // hard cut (last resort)
        if let Some(space) = truncated.iter().rposition(|c| *c == ' ') {
            if space >= half {
                cut = space;
            }
        }
        if cut == len {
            if let Some(pos) = truncated
                .iter()
                .rposition(|c| matches!(c, '-' | ',' | '.' | ')'))
            {
                if pos >= half {
                    cut = pos + 1; // inclusive: label ends on the soft char
                }
            }
        }
        let mut body: String = truncated[..cut].iter().collect();
        body = body.trim_end().to_string();
        body.push('…');
        format!("{prefix}{body}")
    }

    /// One action row per 5 buttons (Discord limits), one button per
    /// choice plus the ✏️ Other row — `clarify:<id>:<idx|other>` custom
    /// ids (hermes ClarifyChoiceView).
    fn clarify_components(clarify_id: &str, choices: &[String], disabled: bool) -> Vec<Value> {
        let mut buttons: Vec<Value> = choices
            .iter()
            .enumerate()
            .map(|(idx, choice)| {
                json!({
                    "type": 2,
                    "style": 1,
                    "label": clarify_button_label(idx, choice),
                    "custom_id": format!("clarify:{clarify_id}:{idx}"),
                    "disabled": disabled,
                })
            })
            .collect();
        buttons.push(json!({
            "type": 2,
            "style": 2,
            "label": "✏️ Other (type answer)",
            "custom_id": format!("clarify:{clarify_id}:other"),
            "disabled": disabled,
        }));
        buttons
            .chunks(5)
            .map(|row| json!({"type": 1, "components": row}))
            .collect()
    }

    fn clarify_embed(question: &str, with_choices: bool) -> Value {
        let mut description = question.trim().to_string();
        if description.chars().count() > 4088 {
            description = format!("{}...", truncate_chars(&description, 4085));
        }
        let field = if with_choices {
            json!({"name": "Choices", "value": "Pick one below, or click ✏️ Other to type a custom answer.", "inline": false})
        } else {
            json!({"name": "Reply", "value": "Reply in this channel with your answer.", "inline": false})
        };
        json!({
            "title": "❓ ulnclaw needs your input",
            "description": description,
            "color": 0xE67E22, // discord.Color.orange()
            "fields": [field],
        })
    }

    /// Plain-content mirror of the embed — embeds are invisible on some
    /// clients (hermes `_self_contained_prompt_content`).
    fn self_contained_prompt_content(header: &str, body: &str, tail: &str) -> String {
        let prefix = format!("{header}\n\n");
        let truncated_suffix = "\n... [truncated]";
        let budget = 2000usize.saturating_sub(prefix.chars().count() + tail.chars().count());
        let mut body = body.to_string();
        if body.chars().count() > budget {
            let cut = budget.saturating_sub(truncated_suffix.chars().count());
            body = format!("{}{truncated_suffix}", truncate_chars(&body, cut));
        }
        format!("{prefix}{body}{tail}")
    }

    /// Send a clarify prompt as embed + one button per choice (hermes
    /// Discord `send_clarify`). Returns false on failure (or with no
    /// choices — open-ended prompts ride the numbered-text path) so the
    /// caller falls back to numbered text.
    pub async fn send_clarify_message(
        token: &str,
        channel_id: &str,
        clarify_id: &str,
        question: &str,
        choices: &[String],
        prompt_timeout_secs: u64,
    ) -> bool {
        if choices.is_empty() {
            return false;
        }
        let choices = &choices[..choices.len().min(MAX_CHOICES)];
        let content = self_contained_prompt_content(
            "❓ **ulnclaw needs your input**",
            question.trim(),
            "\n\nPick one below, or click ✏️ Other to type a custom answer.",
        );
        let body = json!({
            "content": content,
            "embeds": [clarify_embed(question, true)],
            "components": clarify_components(clarify_id, choices, false),
        });
        let client = reqwest::Client::new();
        let response = client
            .post(format!(
                "{}/channels/{channel_id}/messages",
                discord_api_base()
            ))
            .header("Authorization", format!("Bot {token}"))
            .json(&body)
            .send()
            .await;
        match response {
            Ok(resp) if resp.status().is_success() => {
                let sent: Value = resp.json().await.unwrap_or(json!({}));
                if let Some(message_id) = sent.get("id").and_then(|v| v.as_str()) {
                    spawn_prompt_expiry(
                        token,
                        channel_id,
                        message_id,
                        clarify_id,
                        question,
                        choices.to_vec(),
                        prompt_timeout(prompt_timeout_secs),
                    );
                }
                true
            }
            Ok(resp) => {
                eprintln!("[discord] send_clarify failed: HTTP {}", resp.status());
                false
            }
            Err(e) => {
                eprintln!("[discord] send_clarify failed: {e}");
                false
            }
        }
    }

    /// Visual expiration after the prompt window (hermes ClarifyChoiceView
    /// `on_timeout`): grey the embed, disable the buttons. ulnclaw skips
    /// prompts already resolved or still capturing a typed answer — hermes
    /// overwrites the "Awaiting typed response" footer regardless, which
    /// would hide an active text capture (documented divergence).
    fn spawn_prompt_expiry(
        token: &str,
        channel_id: &str,
        message_id: &str,
        clarify_id: &str,
        question: &str,
        choices: Vec<String>,
        timeout: std::time::Duration,
    ) {
        let token = token.to_string();
        let channel_id = channel_id.to_string();
        let message_id = message_id.to_string();
        let clarify_id = clarify_id.to_string();
        let question = question.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(timeout).await;
            if !crate::clarify_gateway::contains(&clarify_id)
                || crate::clarify_gateway::is_awaiting_text(&clarify_id)
            {
                return;
            }
            let mut embed = clarify_embed(&question, true);
            embed["color"] = json!(0x99AAB5); // discord.Color.greyple()
            embed["footer"] = json!({"text": "⏱ Prompt expired — no action taken"});
            let body = json!({
                "embeds": [embed],
                "components": clarify_components(&clarify_id, &choices, true),
            });
            let client = reqwest::Client::new();
            let url = format!(
                "{}/channels/{channel_id}/messages/{message_id}",
                discord_api_base()
            );
            let result = client
                .patch(&url)
                .header("Authorization", format!("Bot {token}"))
                .json(&body)
                .send()
                .await;
            if let Err(e) = result.and_then(|r| r.error_for_status()) {
                eprintln!("[discord] prompt expiry edit failed: {e}");
            }
        });
    }

    /// Route INTERACTION_CREATE button taps (hermes ClarifyChoiceView
    /// callbacks). Only `clarify:<id>:<idx|other>` custom_ids are handled;
    /// every handled path answers via the interaction callback endpoint.
    async fn handle_interaction_create(
        cfg: &DiscordConfig,
        token: &str,
        data: Value,
        pairing: Option<&crate::pairing::PairingStore>,
    ) {
        let interaction_type = data.get("type").and_then(|v| v.as_u64()).unwrap_or(0);
        if interaction_type != 3 {
            return; // MESSAGE_COMPONENT only
        }
        let custom_id = data
            .pointer("/data/custom_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !custom_id.starts_with("clarify:") {
            return;
        }
        let parts: Vec<&str> = custom_id.splitn(3, ':').collect();
        if parts.len() != 3 {
            return;
        }
        let clarify_id = parts[1];
        let choice_token = parts[2];
        let interaction_id = data.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let interaction_token = data.get("token").and_then(|v| v.as_str()).unwrap_or("");
        if interaction_id.is_empty() || interaction_token.is_empty() {
            return;
        }
        let user = data
            .get("user")
            .or_else(|| data.pointer("/member/user"))
            .cloned()
            .unwrap_or(json!({}));
        let user_id = user
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let display_name = user
            .get("global_name")
            .or_else(|| user.get("username"))
            .and_then(|v| v.as_str())
            .unwrap_or("user")
            .to_string();
        let channel_id = data
            .get("channel_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let client = reqwest::Client::new();

        // Same union as MESSAGE_CREATE: channel allowlist OR an approved
        // pairing (ulnclaw's analogue of hermes `_component_check_auth`).
        let authorized = allowlisted(&cfg.allowed_channel_ids, &channel_id)
            || pairing
                .map(|store| store.is_approved("discord", &user_id))
                .unwrap_or(false);

        // Resolved prompts first (hermes `self.resolved` check).
        if !crate::clarify_gateway::contains(clarify_id) {
            ephemeral_notice(
                &client,
                token,
                interaction_id,
                interaction_token,
                "This prompt has already been answered~",
            )
            .await;
            return;
        }
        if !authorized {
            ephemeral_notice(
                &client,
                token,
                interaction_id,
                interaction_token,
                "You're not authorized to answer this prompt~",
            )
            .await;
            return;
        }

        let message = data.get("message").cloned().unwrap_or(json!({}));
        if choice_token == "other" {
            if crate::clarify_gateway::mark_awaiting_text(clarify_id) {
                let embed = restyle_embed(
                    &message,
                    0x3498DB, // discord.Color.blue()
                    &format!("Awaiting typed response from {display_name}…"),
                );
                update_message_response(
                    &client,
                    token,
                    interaction_id,
                    interaction_token,
                    &message,
                    embed,
                )
                .await;
            } else {
                ephemeral_notice(
                    &client,
                    token,
                    interaction_id,
                    interaction_token,
                    "⚠️ This prompt expired — please retry.",
                )
                .await;
            }
            return;
        }

        let Ok(idx) = choice_token.parse::<usize>() else {
            ephemeral_notice(
                &client,
                token,
                interaction_id,
                interaction_token,
                "Invalid choice.",
            )
            .await;
            return;
        };
        // Canonical choice text from the registry (hermes `_entries`
        // lookup); fall back to the index if the entry was cleaned up.
        let resolved_text = crate::clarify_gateway::peek_choice(clarify_id, choice_token)
            .unwrap_or_else(|| format!("choice {}", idx + 1));
        if crate::clarify_gateway::resolve(clarify_id, &resolved_text) {
            let embed = restyle_embed(
                &message,
                0x2ECC71, // discord.Color.green()
                &format!("Answered by {display_name}: {resolved_text}"),
            );
            update_message_response(
                &client,
                token,
                interaction_id,
                interaction_token,
                &message,
                embed,
            )
            .await;
        } else {
            // Entry evicted between ask and click — say so instead of
            // leaving a live-looking prompt (hermes logs only).
            ephemeral_notice(
                &client,
                token,
                interaction_id,
                interaction_token,
                "⚠️ This prompt expired — please retry.",
            )
            .await;
        }
    }

    /// Restyle the prompt's embed with the resolution footer (hermes edits
    /// `interaction.message.embeds[0]` color + footer).
    fn restyle_embed(message: &Value, color: u32, footer_text: &str) -> Value {
        let mut embed = message
            .get("embeds")
            .and_then(|v| v.as_array())
            .and_then(|rows| rows.first())
            .cloned()
            .unwrap_or(json!({}));
        embed["color"] = json!(color);
        embed["footer"] = json!({"text": footer_text});
        embed
    }

    /// Clone the message's component rows with every button disabled
    /// (hermes `child.disabled = True` before the edit).
    fn disable_components(components: Option<&Value>) -> Option<Value> {
        let rows = components?.as_array()?;
        let disabled_rows: Vec<Value> = rows
            .iter()
            .map(|row| {
                let mut row = row.clone();
                if let Some(items) = row.get_mut("components").and_then(|v| v.as_array_mut()) {
                    for item in items {
                        if let Some(object) = item.as_object_mut() {
                            object.insert("disabled".to_string(), Value::Bool(true));
                        }
                    }
                }
                row
            })
            .collect();
        Some(Value::Array(disabled_rows))
    }

    /// POST /interactions/{id}/{token}/callback.
    async fn interaction_callback(
        client: &reqwest::Client,
        token: &str,
        interaction_id: &str,
        interaction_token: &str,
        body: Value,
    ) {
        let url = format!(
            "{}/interactions/{interaction_id}/{interaction_token}/callback",
            discord_api_base()
        );
        let result = client
            .post(&url)
            .header("Authorization", format!("Bot {token}"))
            .json(&body)
            .send()
            .await;
        if let Err(e) = result.and_then(|r| r.error_for_status()) {
            eprintln!("[discord] interaction callback failed: {e}");
        }
    }

    /// Ephemeral notice — callback type 4 + EPHEMERAL flag (hermes
    /// `interaction.response.send_message(..., ephemeral=True)`).
    async fn ephemeral_notice(
        client: &reqwest::Client,
        token: &str,
        interaction_id: &str,
        interaction_token: &str,
        text: &str,
    ) {
        interaction_callback(
            client,
            token,
            interaction_id,
            interaction_token,
            json!({"type": 4, "data": {"content": text, "flags": 64}}),
        )
        .await;
    }

    /// UPDATE_MESSAGE response (callback type 7) — restyles the embed and
    /// disables the buttons in place.
    async fn update_message_response(
        client: &reqwest::Client,
        token: &str,
        interaction_id: &str,
        interaction_token: &str,
        message: &Value,
        embed: Value,
    ) {
        let mut payload = json!({"type": 7, "message": {"embeds": [embed]}});
        if let Some(components) = disable_components(message.get("components")) {
            payload["message"]["components"] = components;
        }
        interaction_callback(client, token, interaction_id, interaction_token, payload).await;
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// axum mock of Discord REST — logs (method, path, body) per call.
        async fn spawn_discord_api(
            log: Arc<std::sync::Mutex<Vec<(String, String, Value)>>>,
        ) -> String {
            use axum::extract::State;
            use axum::routing::{patch, post};
            type Log = Arc<std::sync::Mutex<Vec<(String, String, Value)>>>;
            let app = axum::Router::new()
                .route(
                    "/channels/:channel/messages",
                    post(
                        move |State(log): State<Log>,
                              axum::extract::Path(channel): axum::extract::Path<String>,
                              axum::Json(body): axum::Json<Value>| async move {
                            log.lock().unwrap().push((
                                "POST".into(),
                                format!("/channels/{channel}/messages"),
                                body,
                            ));
                            axum::Json(json!({"id": "777"}))
                        },
                    ),
                )
                .route(
                    "/channels/:channel/messages/:message",
                    patch(
                        move |State(log): State<Log>,
                              axum::extract::Path((channel, message)): axum::extract::Path<(
                            String,
                            String,
                        )>,
                              axum::Json(body): axum::Json<Value>| async move {
                            log.lock().unwrap().push((
                                "PATCH".into(),
                                format!("/channels/{channel}/messages/{message}"),
                                body,
                            ));
                            axum::Json(json!({}))
                        },
                    ),
                )
                .route(
                    "/interactions/:id/:token/callback",
                    post(
                        move |State(log): State<Log>,
                              axum::extract::Path((id, _token)): axum::extract::Path<(
                            String,
                            String,
                        )>,
                              axum::Json(body): axum::Json<Value>| async move {
                            log.lock().unwrap().push((
                                "POST".into(),
                                format!("/interactions/{id}/callback"),
                                body,
                            ));
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
            format!("http://{addr}")
        }

        fn interaction_payload(clarify_id: &str, token: &str) -> Value {
            json!({
                "type": 3,
                "id": "int-1",
                "token": "int-token",
                "channel_id": "42",
                "data": {"custom_id": format!("clarify:{clarify_id}:{token}")},
                "user": {"id": "7", "username": "ann", "global_name": "Ann"},
                "message": {
                    "embeds": [{
                        "title": "❓ ulnclaw needs your input",
                        "description": "Pick",
                        "color": 0xE67E22,
                    }],
                    "components": [{
                        "type": 1,
                        "components": [
                            {"type": 2, "style": 1, "label": "1. Alpha", "custom_id": format!("clarify:{clarify_id}:0")},
                            {"type": 2, "style": 1, "label": "2. Beta", "custom_id": format!("clarify:{clarify_id}:1")},
                        ],
                    }],
                },
            })
        }

        fn authorized_cfg() -> DiscordConfig {
            DiscordConfig {
                allowed_channel_ids: vec!["42".into()],
                ..Default::default()
            }
        }

        #[test]
        fn discord_button_label_smart_truncation() {
            // Short labels pass through with the numeric prefix.
            assert_eq!(clarify_button_label(0, "Yes"), "1. Yes");
            // Long labels cut at a word boundary in the trailing half and
            // end with an ellipsis, within the 80 UTF-16 unit cap.
            let long = "The quick brown fox jumps over the lazy dog and keeps on running through the forest forever";
            let label = clarify_button_label(1, long);
            assert!(label.starts_with("2. "));
            assert!(label.ends_with('…'));
            assert!(utf16_len(&label) <= BUTTON_LABEL_LIMIT);
            let body = label.trim_start_matches("2. ");
            let kept = body.trim_end_matches('…');
            // Kept text is a clean prefix of the choice cut at a word
            // boundary (next char is a space or the end).
            assert!(long.starts_with(kept));
            let rest = &long[kept.len()..];
            assert!(rest.starts_with(' ') || rest.is_empty());
        }

        #[test]
        fn discord_prompt_timeout_clamps_like_hermes() {
            assert_eq!(prompt_timeout(0), std::time::Duration::from_secs(300));
            assert_eq!(prompt_timeout(5), std::time::Duration::from_secs(30));
            assert_eq!(prompt_timeout(4000), std::time::Duration::from_secs(900));
            assert_eq!(prompt_timeout(300), std::time::Duration::from_secs(300));
        }

        #[tokio::test]
        async fn discord_send_clarify_posts_embed_and_buttons() {
            let _env_guard = crate::models_dev::test_env_lock();
            let log = Arc::new(std::sync::Mutex::new(Vec::<(String, String, Value)>::new()));
            let base = spawn_discord_api(log.clone()).await;
            std::env::set_var("DISCORD_API_BASE", &base);
            let ok = send_clarify_message(
                "TOKEN",
                "42",
                "cid000000001",
                "Pick one",
                &["Alpha".into(), "Beta".into()],
                300,
            )
            .await;
            std::env::remove_var("DISCORD_API_BASE");
            assert!(ok);
            let reqs = log.lock().unwrap();
            assert_eq!(reqs.len(), 1);
            let (method, path, body) = &reqs[0];
            assert_eq!(method, "POST");
            assert_eq!(path, "/channels/42/messages");
            // Self-contained content mirrors the embed (some clients hide
            // embeds); the embed carries the question; buttons the choices.
            let content = body["content"].as_str().unwrap();
            assert!(content.starts_with("❓ **ulnclaw needs your input**\n\nPick one"));
            assert!(content.contains("Pick one below, or click ✏️ Other to type a custom answer."));
            assert_eq!(body["embeds"][0]["title"], "❓ ulnclaw needs your input");
            assert_eq!(body["embeds"][0]["description"], "Pick one");
            assert_eq!(body["embeds"][0]["color"], 0xE67E22);
            let rows = body["components"].as_array().unwrap();
            assert_eq!(rows.len(), 1);
            let buttons = rows[0]["components"].as_array().unwrap();
            assert_eq!(buttons.len(), 3);
            assert_eq!(buttons[0]["label"], "1. Alpha");
            assert_eq!(buttons[0]["style"], 1);
            assert_eq!(buttons[0]["custom_id"], "clarify:cid000000001:0");
            assert_eq!(buttons[1]["custom_id"], "clarify:cid000000001:1");
            assert_eq!(buttons[2]["label"], "✏️ Other (type answer)");
            assert_eq!(buttons[2]["style"], 2);
            assert_eq!(buttons[2]["custom_id"], "clarify:cid000000001:other");
        }

        #[tokio::test]
        async fn discord_send_clarify_caps_choices_at_24() {
            let _env_guard = crate::models_dev::test_env_lock();
            let log = Arc::new(std::sync::Mutex::new(Vec::<(String, String, Value)>::new()));
            let base = spawn_discord_api(log.clone()).await;
            std::env::set_var("DISCORD_API_BASE", &base);
            let choices: Vec<String> = (0..30).map(|i| format!("Choice {i}")).collect();
            let ok =
                send_clarify_message("TOKEN", "42", "cid000000002", "Pick", &choices, 300).await;
            std::env::remove_var("DISCORD_API_BASE");
            assert!(ok);
            let reqs = log.lock().unwrap();
            let rows = reqs[0].2["components"].as_array().unwrap();
            // 24 choices + Other = 25 buttons = 5 rows of 5 (Discord max).
            assert_eq!(rows.len(), 5);
            let flat: Vec<&Value> = rows
                .iter()
                .flat_map(|row| row["components"].as_array().unwrap())
                .collect();
            assert_eq!(flat.len(), 25);
            assert_eq!(flat[23]["custom_id"], "clarify:cid000000002:23");
            assert_eq!(flat[24]["custom_id"], "clarify:cid000000002:other");
        }

        #[tokio::test]
        async fn discord_interaction_numeric_choice_resolves() {
            let _env_guard = crate::models_dev::test_env_lock();
            let _clarify_guard = crate::clarify_gateway::test_lock().lock().unwrap();
            crate::clarify_gateway::reset_for_tests();
            let log = Arc::new(std::sync::Mutex::new(Vec::<(String, String, Value)>::new()));
            let base = spawn_discord_api(log.clone()).await;
            std::env::set_var("DISCORD_API_BASE", &base);
            let handle = crate::clarify_gateway::register(
                "platform-discord-42",
                "Pick",
                &["Alpha".into(), "Beta".into()],
                false,
            );
            let clarify_id = handle.clarify_id.clone();
            let payload = interaction_payload(&clarify_id, "1");
            handle_interaction_create(&authorized_cfg(), "TOKEN", payload, None).await;
            std::env::remove_var("DISCORD_API_BASE");
            assert_eq!(handle.rx.await.unwrap(), "Beta");
            let reqs = log.lock().unwrap();
            assert_eq!(reqs.len(), 1);
            let (method, path, body) = &reqs[0];
            assert_eq!(method, "POST");
            assert_eq!(path, "/interactions/int-1/callback");
            // UPDATE_MESSAGE with green embed + resolution footer and
            // disabled buttons.
            assert_eq!(body["type"], 7);
            let embed = &body["message"]["embeds"][0];
            assert_eq!(embed["color"], 0x2ECC71);
            assert_eq!(embed["footer"]["text"], "Answered by Ann: Beta");
            let rows = body["message"]["components"].as_array().unwrap();
            assert!(rows.iter().all(|row| row["components"]
                .as_array()
                .unwrap()
                .iter()
                .all(|b| b["disabled"].as_bool() == Some(true))));
        }

        #[tokio::test]
        async fn discord_interaction_other_flips_text_capture() {
            let _env_guard = crate::models_dev::test_env_lock();
            let _clarify_guard = crate::clarify_gateway::test_lock().lock().unwrap();
            crate::clarify_gateway::reset_for_tests();
            let log = Arc::new(std::sync::Mutex::new(Vec::<(String, String, Value)>::new()));
            let base = spawn_discord_api(log.clone()).await;
            std::env::set_var("DISCORD_API_BASE", &base);
            let handle = crate::clarify_gateway::register(
                "platform-discord-42",
                "Pick",
                &["Alpha".into(), "Beta".into()],
                false,
            );
            let clarify_id = handle.clarify_id.clone();
            let payload = interaction_payload(&clarify_id, "other");
            handle_interaction_create(&authorized_cfg(), "TOKEN", payload, None).await;
            std::env::remove_var("DISCORD_API_BASE");
            // Entry survives in text-capture mode; a typed message resolves
            // it through the session intercept.
            let pending = crate::clarify_gateway::pending_for_session("platform-discord-42")
                .expect("entry must survive the Other click");
            assert!(pending.awaiting_text);
            assert!(crate::clarify_gateway::resolve(&clarify_id, "typed answer"));
            assert_eq!(handle.rx.await.unwrap(), "typed answer");
            let reqs = log.lock().unwrap();
            assert_eq!(reqs.len(), 1);
            let body = &reqs[0].2;
            assert_eq!(body["type"], 7);
            let embed = &body["message"]["embeds"][0];
            assert_eq!(embed["color"], 0x3498DB);
            assert_eq!(embed["footer"]["text"], "Awaiting typed response from Ann…");
        }

        #[tokio::test]
        async fn discord_interaction_stale_prompt_notice() {
            let _env_guard = crate::models_dev::test_env_lock();
            let _clarify_guard = crate::clarify_gateway::test_lock().lock().unwrap();
            crate::clarify_gateway::reset_for_tests();
            let log = Arc::new(std::sync::Mutex::new(Vec::<(String, String, Value)>::new()));
            let base = spawn_discord_api(log.clone()).await;
            std::env::set_var("DISCORD_API_BASE", &base);
            // No entry registered — a click on an answered/unknown prompt.
            let payload = interaction_payload("zzzzzzzzzzzz", "0");
            handle_interaction_create(&authorized_cfg(), "TOKEN", payload, None).await;
            std::env::remove_var("DISCORD_API_BASE");
            let reqs = log.lock().unwrap();
            assert_eq!(reqs.len(), 1);
            let body = &reqs[0].2;
            assert_eq!(body["type"], 4);
            assert_eq!(body["data"]["flags"], 64);
            assert_eq!(
                body["data"]["content"],
                "This prompt has already been answered~"
            );
        }

        #[tokio::test]
        async fn discord_interaction_unauthorized_notice() {
            let _env_guard = crate::models_dev::test_env_lock();
            let _clarify_guard = crate::clarify_gateway::test_lock().lock().unwrap();
            crate::clarify_gateway::reset_for_tests();
            let log = Arc::new(std::sync::Mutex::new(Vec::<(String, String, Value)>::new()));
            let base = spawn_discord_api(log.clone()).await;
            std::env::set_var("DISCORD_API_BASE", &base);
            let handle = crate::clarify_gateway::register(
                "platform-discord-42",
                "Pick",
                &["Alpha".into()],
                false,
            );
            let clarify_id = handle.clarify_id.clone();
            let payload = interaction_payload(&clarify_id, "0");
            // Channel 42 is not allowlisted and no pairing store exists.
            let cfg = DiscordConfig {
                allowed_channel_ids: vec!["999".into()],
                ..Default::default()
            };
            handle_interaction_create(&cfg, "TOKEN", payload, None).await;
            std::env::remove_var("DISCORD_API_BASE");
            let reqs = log.lock().unwrap();
            assert_eq!(reqs.len(), 1);
            assert_eq!(
                reqs[0].2["data"]["content"],
                "You're not authorized to answer this prompt~"
            );
            // The waiter is untouched — the legitimate user can still answer.
            assert!(crate::clarify_gateway::contains(&clarify_id));
            assert!(crate::clarify_gateway::resolve(&clarify_id, "Alpha"));
        }

        #[tokio::test]
        async fn discord_prompt_expiry_disables_unanswered_prompt() {
            let _env_guard = crate::models_dev::test_env_lock();
            let _clarify_guard = crate::clarify_gateway::test_lock().lock().unwrap();
            crate::clarify_gateway::reset_for_tests();
            let log = Arc::new(std::sync::Mutex::new(Vec::<(String, String, Value)>::new()));
            let base = spawn_discord_api(log.clone()).await;
            std::env::set_var("DISCORD_API_BASE", &base);
            let handle = crate::clarify_gateway::register(
                "platform-discord-42",
                "Pick",
                &["Alpha".into()],
                false,
            );
            spawn_prompt_expiry(
                "TOKEN",
                "42",
                "777",
                &handle.clarify_id,
                "Pick",
                vec!["Alpha".into()],
                std::time::Duration::from_millis(200),
            );
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            std::env::remove_var("DISCORD_API_BASE");
            let reqs = log.lock().unwrap();
            assert_eq!(reqs.len(), 1);
            let (method, path, body) = &reqs[0];
            assert_eq!(method, "PATCH");
            assert_eq!(path, "/channels/42/messages/777");
            assert_eq!(body["embeds"][0]["color"], 0x99AAB5);
            assert_eq!(
                body["embeds"][0]["footer"]["text"],
                "⏱ Prompt expired — no action taken"
            );
            let rows = body["components"].as_array().unwrap();
            assert!(rows.iter().all(|row| row["components"]
                .as_array()
                .unwrap()
                .iter()
                .all(|b| b["disabled"].as_bool() == Some(true))));
        }

        #[tokio::test]
        async fn discord_prompt_expiry_skips_text_capture() {
            let _env_guard = crate::models_dev::test_env_lock();
            let _clarify_guard = crate::clarify_gateway::test_lock().lock().unwrap();
            crate::clarify_gateway::reset_for_tests();
            let log = Arc::new(std::sync::Mutex::new(Vec::<(String, String, Value)>::new()));
            let base = spawn_discord_api(log.clone()).await;
            std::env::set_var("DISCORD_API_BASE", &base);
            let handle = crate::clarify_gateway::register(
                "platform-discord-42",
                "Pick",
                &["Alpha".into()],
                false,
            );
            assert!(crate::clarify_gateway::mark_awaiting_text(
                &handle.clarify_id
            ));
            spawn_prompt_expiry(
                "TOKEN",
                "42",
                "777",
                &handle.clarify_id,
                "Pick",
                vec!["Alpha".into()],
                std::time::Duration::from_millis(200),
            );
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            std::env::remove_var("DISCORD_API_BASE");
            // No PATCH — the "Awaiting typed response" footer stays and the
            // typed answer can still arrive (documented hermes divergence).
            assert!(log.lock().unwrap().is_empty());
            assert!(crate::clarify_gateway::resolve(
                &handle.clarify_id,
                "late answer"
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// Slack — Socket Mode websocket + chat.postMessage
// ---------------------------------------------------------------------------

pub mod slack {
    use super::*;
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    struct Sender {
        bot_token: String,
    }

    #[async_trait::async_trait]
    impl PlatformSender for Sender {
        async fn send_text(&self, chat_id: &str, text: &str) {
            post_message(&self.bot_token, chat_id, text).await;
        }

        /// Block Kit clarify buttons (hermes Slack `send_clarify`).
        /// Returns false on API failure so the numbered-text fallback still
        /// goes out.
        async fn send_clarify(
            &self,
            chat_id: &str,
            clarify_id: &str,
            question: &str,
            choices: &[String],
        ) -> bool {
            send_clarify_message(&self.bot_token, chat_id, clarify_id, question, choices).await
        }

        /// Emoji reactions (hermes send_message action='react'/'unreact'):
        /// `reactions.add` / `reactions.remove`. The message id is the
        /// Slack timestamp recorded on the inbound event.
        async fn send_reaction(
            &self,
            chat_id: &str,
            emoji: &str,
            message_id: &str,
            remove: bool,
        ) -> Option<bool> {
            let name = emoji.trim().trim_matches(':').to_string();
            if name.is_empty() || message_id.trim().is_empty() {
                return Some(false);
            }
            let method = if remove {
                "reactions.remove"
            } else {
                "reactions.add"
            };
            let params = json!({"channel": chat_id, "name": name, "timestamp": message_id.trim()});
            let client = reqwest::Client::new();
            match client
                .post(format!("{}/{}", slack_api_base(), method))
                .header("Authorization", format!("Bearer {}", self.bot_token))
                .json(&params)
                .send()
                .await
            {
                Ok(response) => match response.json::<Value>().await {
                    Ok(value) => Some(value.get("ok").and_then(|v| v.as_bool()).unwrap_or(false)),
                    Err(e) => {
                        eprintln!("[slack] {method} parse failed: {e}");
                        Some(false)
                    }
                },
                Err(e) => {
                    eprintln!("[slack] {method} failed: {e}");
                    Some(false)
                }
            }
        }

        /// Native file uploads (hermes send_message MEDIA: path): body
        /// text first, then one `files` upload per attachment.
        async fn send_media(
            &self,
            chat_id: &str,
            text: &str,
            paths: &[std::path::PathBuf],
        ) -> bool {
            if paths.is_empty() {
                return false;
            }
            if !text.trim().is_empty() {
                post_message(&self.bot_token, chat_id, text.trim()).await;
            }
            for path in paths {
                upload_file(&self.bot_token, chat_id, path).await;
            }
            true
        }
    }

    pub async fn run(
        cfg: SlackConfig,
        dispatcher: Arc<Dispatcher>,
        pairing: Option<Arc<crate::pairing::PairingStore>>,
    ) {
        let Some(bot_token) = resolve_token(&cfg.bot_token, "SLACK_BOT_TOKEN") else {
            eprintln!("[slack] disabled: no bot_token configured (set messaging.slack.bot_token or SLACK_BOT_TOKEN)");
            return;
        };
        let Some(app_token) = resolve_token(&cfg.app_token, "SLACK_APP_TOKEN") else {
            eprintln!("[slack] disabled: no app_token configured (set messaging.slack.app_token or SLACK_APP_TOKEN)");
            return;
        };
        register_platform_sender(
            "slack",
            Arc::new(Sender {
                bot_token: bot_token.clone(),
            }),
        );
        loop {
            match run_socket_session(
                &cfg,
                &bot_token,
                &app_token,
                dispatcher.clone(),
                pairing.clone(),
            )
            .await
            {
                Ok(()) => {}
                Err(e) => eprintln!("[slack] socket session ended: {e} — reconnecting in 5s"),
            }
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    }

    async fn open_socket_url(client: &reqwest::Client, app_token: &str) -> Result<String> {
        let response = client
            .post(format!("{}/apps.connections.open", slack_api_base()))
            .header("Authorization", format!("Bearer {app_token}"))
            .send()
            .await
            .map_err(|e| AgentError::Tool(format!("slack open: {e}")))?;
        let value: Value = response
            .json()
            .await
            .map_err(|e| AgentError::Tool(format!("slack open parse: {e}")))?;
        if !value.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            return Err(AgentError::Tool(format!(
                "slack apps.connections.open: {}",
                value
                    .get("error")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
            )));
        }
        value
            .get("url")
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| AgentError::Tool("slack open: no url".into()))
    }

    async fn run_socket_session(
        cfg: &SlackConfig,
        bot_token: &str,
        app_token: &str,
        dispatcher: Arc<Dispatcher>,
        pairing: Option<Arc<crate::pairing::PairingStore>>,
    ) -> Result<()> {
        let client = reqwest::Client::new();
        let url = open_socket_url(&client, app_token).await?;
        let (ws, _) = tokio_tungstenite::connect_async(&url)
            .await
            .map_err(|e| AgentError::Tool(format!("slack connect: {e}")))?;
        eprintln!("[slack] socket-mode connected");
        let (mut sink, mut stream) = ws.split();
        let own_bot_id: Option<String> = None;
        while let Some(Ok(message)) = stream.next().await {
            let WsMessage::Text(text) = message else {
                continue;
            };
            let Ok(envelope) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            let envelope_type = envelope.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let envelope_id = envelope
                .get("envelope_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if envelope_type == "hello" {
                continue;
            }
            // Ack every envelope promptly (Slack retries unacked ones).
            if !envelope_id.is_empty() {
                let ack = json!({"envelope_id": envelope_id});
                sink.send(WsMessage::Text(ack.to_string())).await.ok();
            }
            // Clarify button clicks arrive as `interactive` envelopes with
            // a block_actions payload (hermes `_handle_clarify_action`).
            if envelope_type == "interactive" {
                handle_interactive_payload(
                    cfg,
                    bot_token,
                    envelope.get("payload").cloned().unwrap_or(json!({})),
                    pairing.as_deref(),
                )
                .await;
                continue;
            }
            // Native Slack slash commands arrive as `slash_commands`
            // envelopes when the app manifest (ulnclaw slack manifest)
            // registers them. Dispatch them like chat text and answer via
            // the payload's response_url (hermes slack adapter slash flow).
            if envelope_type == "slash_commands" {
                let payload = envelope.get("payload").cloned().unwrap_or(json!({}));
                handle_slash_envelope(
                    cfg,
                    bot_token,
                    payload,
                    pairing.as_deref(),
                    dispatcher.clone(),
                )
                .await;
                continue;
            }
            if envelope_type != "events_api" {
                continue;
            }
            let event = envelope
                .pointer("/payload/event")
                .cloned()
                .unwrap_or(json!({}));
            let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if event_type == "app_mention"
                || (event_type == "message" && event.get("subtype").is_none())
            {
                if let Some(bot_id) = event.get("bot_id").and_then(|v| v.as_str()) {
                    if own_bot_id.as_deref() == Some(bot_id) || event.get("bot_profile").is_some() {
                        continue;
                    }
                }
                let channel = event
                    .get("channel")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let text = event
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let has_files = event
                    .get("files")
                    .and_then(|v| v.as_array())
                    .map(|files| !files.is_empty())
                    .unwrap_or(false);
                if channel.is_empty() || (text.is_empty() && !has_files) {
                    continue;
                }
                // Strip a leading <@BOTID> mention so the agent sees clean text.
                let text = strip_mention(&text);
                // Slack file URLs are private — the bot bearer authenticates
                // the download (hermes slack adapter behavior).
                let attachments = {
                    let client = reqwest::Client::new();
                    let auth = format!("Bearer {bot_token}");
                    let mut downloaded = Vec::new();
                    if let Some(files) = event.get("files").and_then(|v| v.as_array()) {
                        for file in files {
                            let Some(url) = file.get("url_private").and_then(|v| v.as_str()) else {
                                continue;
                            };
                            let name = file.get("name").and_then(|v| v.as_str()).unwrap_or("");
                            let mime = file.get("mimetype").and_then(|v| v.as_str()).unwrap_or("");
                            let size = file.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
                            if let Some(attachment) =
                                download_to_cache(&client, url, Some(&auth), mime, name, size).await
                            {
                                downloaded.push(attachment);
                            }
                        }
                    }
                    downloaded
                };
                let mut message_event = MessageEvent {
                    platform: "slack".into(),
                    chat_id: channel.clone(),
                    sender_id: event
                        .get("user")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    sender_name: event
                        .get("user")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    text,
                    message_id: event
                        .get("ts")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    attachments,
                };
                // Plugin gate before auth (hermes ordering).
                if !pre_gateway_dispatch_gate(&mut message_event).await {
                    continue;
                }
                // Auth union: configured allowlist OR an approved pairing code.
                let authorized = allowlisted(&cfg.allowed_channel_ids, &channel)
                    || pairing
                        .as_ref()
                        .map(|store| store.is_approved("slack", &message_event.sender_id))
                        .unwrap_or(false);
                if !authorized {
                    eprintln!(
                        "[slack] refusing message from channel {channel} — add it to \
                         messaging.slack.allowed_channel_ids or approve a pairing code"
                    );
                    if let Some(store) = pairing.as_ref() {
                        if let Some(reply) = pairing_offer(
                            store,
                            "slack",
                            &message_event.sender_id,
                            &message_event.sender_name,
                        ) {
                            post_message(bot_token, &channel, &reply).await;
                        }
                    }
                    continue;
                }
                // hermes typing parity: the assistant-thread status rides the
                // event's thread, falling back to the message's own ts for
                // top-level messages (hermes synthetic-thread session keying).
                let raw_thread_ts = event
                    .get("thread_ts")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let typing_thread = if raw_thread_ts.is_empty() {
                    message_event.message_id.clone()
                } else {
                    raw_thread_ts
                };
                let typing_status_cfg = cfg.typing_status_text.clone();
                let dispatcher = dispatcher.clone();
                let bot_token = bot_token.to_string();
                tokio::spawn(async move {
                    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
                    let heartbeat = if typing_thread.is_empty() {
                        None
                    } else {
                        Some(tokio::spawn(typing_heartbeat(
                            bot_token.clone(),
                            message_event.chat_id.clone(),
                            typing_thread,
                            typing_status_cfg,
                            std::time::Duration::from_secs(2),
                            stop_rx,
                        )))
                    };
                    let outcome = match dispatcher.handle_event(message_event.clone()).await {
                        Ok(outcome) => outcome,
                        Err(e) => crate::messaging::DispatchOutcome {
                            reply: format!("error: {e}"),
                            transcript_echoes: Vec::new(),
                        },
                    };
                    for echo in &outcome.transcript_echoes {
                        post_message(&bot_token, &message_event.chat_id, echo).await;
                    }
                    let reply = outcome.reply;
                    // MEDIA:<path> tags become native Slack file uploads
                    // (hermes files.upload flow); the rest posts as text.
                    let (reply_text, media_paths) = extract_media_tags(&reply);
                    if !reply_text.trim().is_empty() {
                        // P703: ledger-protected reply delivery.
                        dispatcher
                            .send_with_ledger("slack", &message_event.chat_id, &reply_text, || {
                                post_message(&bot_token, &message_event.chat_id, &reply_text)
                            })
                            .await;
                    }
                    for path in media_paths {
                        upload_file(&bot_token, &message_event.chat_id, &path).await;
                    }
                    // hermes stop_typing: clear the status once the reply is out.
                    if let Some(handle) = heartbeat {
                        let _ = stop_tx.send(true);
                        let _ = handle.await;
                    }
                });
            }
        }
        Ok(())
    }

    pub(crate) fn strip_mention(text: &str) -> String {
        let trimmed = text.trim_start();
        if trimmed.starts_with("<@") {
            if let Some(end) = trimmed.find('>') {
                return trimmed[end + 1..].trim().to_string();
            }
        }
        text.to_string()
    }

    /// chat.postMessage with Slack's ~40k limit chunked at 3500 chars.
    /// One native slash-command envelope (hermes slack adapter
    /// `_handle_slash_command`): auth-gate, dispatch the command text
    /// through the normal platform path, and deliver the reply via
    /// `response_url` (replace_original) with a postMessage fallback.
    async fn handle_slash_envelope(
        cfg: &SlackConfig,
        bot_token: &str,
        payload: Value,
        pairing: Option<&crate::pairing::PairingStore>,
        dispatcher: Arc<Dispatcher>,
    ) {
        let command = payload
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if command.is_empty() {
            return;
        }
        let text = payload
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let user_id = payload
            .get("user_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let channel_id = payload
            .get("channel_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let response_url = payload
            .get("response_url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if channel_id.is_empty() {
            return;
        }

        // The /ulnclaw catch-all passes the raw text through to the agent;
        // named slashes keep their leading "/" so the direct-command layer
        // recognizes them (hermes /hermes <subcommand> mapping parity).
        let dispatch_text = if command == "/ulnclaw" {
            text.clone()
        } else {
            format!("{command} {text}").trim().to_string()
        };
        if dispatch_text.is_empty() {
            return;
        }

        let mut message_event = MessageEvent {
            platform: "slack".into(),
            chat_id: channel_id.clone(),
            sender_id: user_id.clone(),
            sender_name: user_id.clone(),
            text: dispatch_text,
            message_id: String::new(),
            attachments: Vec::new(),
        };
        // Plugin gate before auth (hermes ordering).
        if !pre_gateway_dispatch_gate(&mut message_event).await {
            return;
        }
        let authorized = allowlisted(&cfg.allowed_channel_ids, &channel_id)
            || pairing
                .map(|store| store.is_approved("slack", &user_id))
                .unwrap_or(false);
        if !authorized {
            eprintln!(
                "[slack] refusing slash command from channel {channel_id} — add it to \
                 messaging.slack.allowed_channel_ids or approve a pairing code"
            );
            if let Some(store) = pairing {
                if let Some(reply) = pairing_offer(store, "slack", &user_id, &user_id) {
                    deliver_slash_reply(bot_token, &channel_id, &response_url, &reply, true).await;
                }
            }
            return;
        }

        let bot_token = bot_token.to_string();
        tokio::spawn(async move {
            let outcome = match dispatcher.handle_event(message_event).await {
                Ok(outcome) => outcome,
                Err(e) => crate::messaging::DispatchOutcome {
                    reply: format!("error: {e}"),
                    transcript_echoes: Vec::new(),
                },
            };
            // Echoes (STT transcripts) post as ordinary messages; the final
            // reply replaces the original slash invocation via response_url.
            for echo in &outcome.transcript_echoes {
                post_message(&bot_token, &channel_id, echo).await;
            }
            let (reply_text, media_paths) = extract_media_tags(&outcome.reply);
            if !reply_text.trim().is_empty() {
                // P703: ledger-protected reply delivery.
                dispatcher
                    .send_with_ledger("slack", &channel_id, &reply_text, || {
                        deliver_slash_reply(
                            &bot_token,
                            &channel_id,
                            &response_url,
                            &reply_text,
                            true,
                        )
                    })
                    .await;
            }
            for path in media_paths {
                upload_file(&bot_token, &channel_id, &path).await;
            }
        });
    }

    /// Deliver one slash-command reply: prefer the payload's `response_url`
    /// (`replace_original` semantics — hermes `_replace_slash_response`),
    /// falling back to chat.postMessage when the URL is missing or the POST
    /// fails. Only `replace_original=true` for the final message; earlier
    /// deliveries must not clobber it.
    async fn deliver_slash_reply(
        bot_token: &str,
        channel: &str,
        response_url: &str,
        text: &str,
        replace_original: bool,
    ) {
        if !response_url.is_empty() {
            let client = reqwest::Client::new();
            let body = json!({
                "text": text,
                "replace_original": replace_original,
                "response_type": "in_channel",
            });
            match client.post(response_url).json(&body).send().await {
                Ok(resp) if resp.status().is_success() => return,
                Ok(resp) => eprintln!("[slack] response_url POST returned {}", resp.status()),
                Err(e) => eprintln!("[slack] response_url POST failed: {e}"),
            }
        }
        post_message(bot_token, channel, text).await;
    }

    pub async fn post_message(bot_token: &str, channel: &str, text: &str) {
        let client = reqwest::Client::new();
        for chunk in chunk_text(text, 3500) {
            let result = client
                .post(format!("{}/chat.postMessage", slack_api_base()))
                .header("Authorization", format!("Bearer {bot_token}"))
                .json(&json!({"channel": channel, "text": chunk}))
                .send()
                .await;
            match result {
                Ok(response) => {
                    if let Ok(value) = response.json::<Value>().await {
                        if !value.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
                            eprintln!(
                                "[slack] postMessage failed: {}",
                                value
                                    .get("error")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("unknown")
                            );
                        }
                    }
                }
                Err(e) => eprintln!("[slack] postMessage failed: {e}"),
            }
        }
    }

    /// Upload a local file as a native Slack file (MEDIA: delivery).
    /// Modern three-step flow: `files.getUploadURLExternal` → PUT the
    /// binary to the signed URL → `files.completeUploadExternal` into the
    /// channel (the legacy one-shot `files.upload` is retired).
    pub async fn upload_file(bot_token: &str, channel: &str, path: &std::path::Path) {
        let data = match std::fs::read(path) {
            Ok(data) => data,
            Err(e) => {
                eprintln!("[slack] cannot read {}: {e}", path.display());
                return;
            }
        };
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "file".to_string());
        let client = reqwest::Client::new();
        let auth = format!("Bearer {bot_token}");

        // Step 1: request an upload URL + file id.
        let url_response = client
            .get(format!("{}/files.getUploadURLExternal", slack_api_base()))
            .header("Authorization", &auth)
            .query(&[
                ("filename", &file_name),
                ("length", &data.len().to_string()),
            ])
            .send()
            .await;
        let url_value = match url_response {
            Ok(response) => match response.json::<Value>().await {
                Ok(value) => value,
                Err(e) => {
                    eprintln!("[slack] getUploadURLExternal parse failed: {e}");
                    return;
                }
            },
            Err(e) => {
                eprintln!("[slack] getUploadURLExternal failed: {e}");
                return;
            }
        };
        if !url_value
            .get("ok")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            eprintln!(
                "[slack] getUploadURLExternal failed: {}",
                url_value
                    .get("error")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
            );
            return;
        }
        let upload_url = match url_value.get("upload_url").and_then(|v| v.as_str()) {
            Some(url) => url.to_string(),
            None => {
                eprintln!("[slack] getUploadURLExternal returned no upload_url");
                return;
            }
        };
        let file_id = match url_value.get("file_id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => {
                eprintln!("[slack] getUploadURLExternal returned no file_id");
                return;
            }
        };

        // Step 2: PUT the raw bytes to the signed URL.
        let put_response = client
            .put(&upload_url)
            .header("Content-Type", "application/octet-stream")
            .body(data)
            .send()
            .await;
        match put_response {
            Ok(response) if response.status().is_success() => {}
            Ok(response) => {
                eprintln!("[slack] file upload PUT failed ({})", response.status());
                return;
            }
            Err(e) => {
                eprintln!("[slack] file upload PUT failed: {e}");
                return;
            }
        }

        // Step 3: complete the upload, sharing into the channel.
        let files_json = serde_json::to_string(&json!([{ "id": file_id, "title": file_name }]))
            .unwrap_or_default();
        let channels_json = serde_json::to_string(&json!([channel])).unwrap_or_default();
        let complete = client
            .post(format!("{}/files.completeUploadExternal", slack_api_base()))
            .header("Authorization", &auth)
            .form(&[("files", files_json.as_str()), ("channel_id", channel)])
            .send()
            .await;
        match complete {
            Ok(response) => match response.json::<Value>().await {
                Ok(value) => {
                    if !value.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
                        eprintln!(
                            "[slack] completeUploadExternal failed: {}",
                            value
                                .get("error")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown")
                        );
                    }
                    let _ = channels_json;
                }
                Err(e) => eprintln!("[slack] completeUploadExternal parse failed: {e}"),
            },
            Err(e) => eprintln!("[slack] completeUploadExternal failed: {e}"),
        }
    }

    /// Slack Web API base URL — normally `https://slack.com/api`; the
    /// `SLACK_API_BASE` override exists for tests and corporate proxies
    /// (mirrors the qqbot `QQ_PORTAL_HOST` pattern).
    pub(crate) fn slack_api_base() -> String {
        env_or_none("SLACK_API_BASE").unwrap_or_else(|| "https://slack.com/api".to_string())
    }

    fn env_or_none(name: &str) -> Option<String> {
        std::env::var(name)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    }

    /// hermes `assistant.threads.setStatus` — show (or clear, when `status`
    /// is empty) the assistant thread status line next to the bot name.
    /// Requires the `assistant:write` scope; failures are logged but never
    /// fatal (hermes silently degrades to no indicator).
    pub async fn set_thread_status(
        bot_token: &str,
        channel: &str,
        thread_ts: &str,
        status: &str,
    ) -> bool {
        let client = reqwest::Client::new();
        let result = client
            .post(format!("{}/assistant.threads.setStatus", slack_api_base()))
            .header("Authorization", format!("Bearer {bot_token}"))
            .json(&json!({"channel_id": channel, "thread_ts": thread_ts, "status": status}))
            .send()
            .await;
        match result {
            Ok(response) => match response.json::<Value>().await {
                Ok(value) => {
                    let ok = value.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
                    if !ok {
                        eprintln!(
                            "[slack] assistant.threads.setStatus failed: {}",
                            value
                                .get("error")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown")
                        );
                    }
                    ok
                }
                Err(e) => {
                    eprintln!("[slack] assistant.threads.setStatus parse failed: {e}");
                    false
                }
            },
            Err(e) => {
                eprintln!("[slack] assistant.threads.setStatus failed: {e}");
                false
            }
        }
    }

    /// hermes `send_typing` status selection: configured text always wins;
    /// after 30 s the default becomes an elapsed-time heartbeat (hermes
    /// #45702) so a long turn reads as working, not stuck.
    pub fn typing_status_text(elapsed: std::time::Duration, configured: Option<&str>) -> String {
        if let Some(text) = configured.filter(|t| !t.is_empty()) {
            return text.to_string();
        }
        let secs = elapsed.as_secs();
        if secs >= 30 {
            let mins = secs / 60;
            let rem = secs % 60;
            let human = if mins > 0 {
                format!("{mins}m{rem:02}s")
            } else {
                format!("{rem}s")
            };
            format!("still working… ({human})")
        } else {
            "is thinking...".to_string()
        }
    }

    /// P722: generic-mode status-phrase rotation state (hermes
    /// `long_running_notifications: "generic"` parity).
    pub struct GenericPhraseCtx {
        catalog: crate::status_phrases::StatusPhraseCatalog,
        recent: Vec<String>,
        picker: crate::status_phrases::DefaultPicker,
        window: u64,
        phrase: Option<String>,
    }

    impl GenericPhraseCtx {
        /// Load only when Slack's long-running notifications resolve to
        /// the `generic` visibility mode (hermes `_long_running_mode`).
        pub fn load() -> Option<Self> {
            let config = crate::config::UlncLawConfig::load(None).ok()?;
            let resolved = crate::display_config::resolve(
                &config.display,
                Some("slack"),
                crate::display_config::DisplaySetting::LongRunningNotifications,
            )?;
            match resolved {
                crate::display_config::DisplayValue::Text(mode) if mode == "generic" => {}
                _ => return None,
            }
            let home = crate::config::ulnclaw_home();
            let catalog =
                crate::status_phrases::resolve_catalog(&config.display, Some("slack"), &home);
            Some(Self {
                catalog,
                recent: Vec::new(),
                picker: crate::status_phrases::DefaultPicker::new(),
                window: u64::MAX,
                phrase: None,
            })
        }

        /// Status line for the current refresh: before 30 s the usual
        /// "is thinking..."; afterwards one catalog phrase per 30 s
        /// window (the typing loop refreshes every ~2 s, so rotating
        /// per window avoids flicker).
        pub fn status_for(&mut self, elapsed: std::time::Duration) -> String {
            let secs = elapsed.as_secs();
            if secs < 30 {
                return "is thinking...".to_string();
            }
            let window = secs / 30;
            if window != self.window || self.phrase.is_none() {
                self.window = window;
                self.phrase = Some(crate::status_phrases::choose_status_phrase(
                    "status",
                    Some(&mut self.recent),
                    &mut |bound| self.picker.index(bound),
                    Some(&self.catalog),
                ));
            }
            self.phrase.clone().unwrap_or_default()
        }
    }

    /// hermes `_keep_typing` (Slack flavor): refresh the assistant-thread
    /// status every `interval` (hermes default 2 s) until `stop` flips,
    /// each call bounded so a slow round-trip cannot stall the cadence
    /// (hermes `max(0.25, min(1.5, interval - 0.25))`); on stop, clear the
    /// indicator (hermes `stop_typing` posts an empty status).
    pub async fn typing_heartbeat(
        bot_token: String,
        channel: String,
        thread_ts: String,
        configured: Option<String>,
        interval: std::time::Duration,
        mut stop: tokio::sync::watch::Receiver<bool>,
    ) {
        let started = std::time::Instant::now();
        let call_timeout =
            std::time::Duration::from_secs_f64((interval.as_secs_f64() - 0.25).clamp(0.25, 1.5));
        // P722: generic-mode phrase rotation — engaged only when the
        // display config asks for generic long-running notices AND no
        // custom typing text is configured (custom text always wins,
        // hermes typing_status_text precedence).
        let mut phrase_ctx = if configured.as_deref().map(str::is_empty).unwrap_or(true) {
            GenericPhraseCtx::load()
        } else {
            None
        };
        loop {
            if *stop.borrow() {
                break;
            }
            let status = match phrase_ctx.as_mut() {
                Some(ctx) => ctx.status_for(started.elapsed()),
                None => typing_status_text(started.elapsed(), configured.as_deref()),
            };
            let _ = tokio::time::timeout(
                call_timeout,
                set_thread_status(&bot_token, &channel, &thread_ts, &status),
            )
            .await;
            tokio::select! {
                _ = tokio::time::sleep(interval) => {}
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        break;
                    }
                }
            }
        }
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs_f64(1.5),
            set_thread_status(&bot_token, &channel, &thread_ts, ""),
        )
        .await;
    }

    // -- Clarify Block Kit buttons — hermes Slack send_clarify parity ------

    /// Slack mrkdwn control-char escape — hermes send_clarify escapes &,
    /// <, > so questions render literally instead of markup/mentions.
    fn mrkdwn_escape(text: &str) -> String {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }

    /// Send a clarify prompt as Block Kit buttons (hermes Slack
    /// `send_clarify`): one button per choice (`hermes_clarify_choice_<idx>`
    /// action_id, value packs `<clarify_id>|<idx>`, label capped at 75
    /// chars) plus a final ✏️ Other… button (`hermes_clarify_other`).
    /// Returns false with no choices (open-ended prompts ride the
    /// numbered-text path) or on API failure so the caller falls back.
    pub async fn send_clarify_message(
        bot_token: &str,
        channel: &str,
        clarify_id: &str,
        question: &str,
        choices: &[String],
    ) -> bool {
        if choices.is_empty() {
            return false;
        }
        // Section text caps at 3000 chars — budget the question so the
        // wrapper never pushes the block over the limit (overflow →
        // invalid_blocks → no buttons).
        let mut body = format!("❓ {}", mrkdwn_escape(question));
        let budget = 3000 - 3;
        if body.chars().count() > budget {
            let kept: String = body.chars().take(budget).collect();
            body = format!("{kept}...");
        }
        let mut elements: Vec<Value> = choices
            .iter()
            .enumerate()
            .map(|(idx, choice)| {
                let label = choice.trim();
                let label = if label.is_empty() {
                    format!("Option {}", idx + 1)
                } else {
                    label.chars().take(75).collect::<String>()
                };
                json!({
                    "type": "button",
                    "text": {"type": "plain_text", "text": label, "emoji": true},
                    "action_id": format!("hermes_clarify_choice_{idx}"),
                    "value": format!("{clarify_id}|{idx}"),
                })
            })
            .collect();
        elements.push(json!({
            "type": "button",
            "text": {"type": "plain_text", "text": "✏️ Other…", "emoji": true},
            "action_id": "hermes_clarify_other",
            "value": format!("{clarify_id}|other"),
        }));
        let mut blocks: Vec<Value> = vec![json!({
            "type": "section",
            "text": {"type": "mrkdwn", "text": body},
        })];
        // Slack caps an actions block at 5 elements; chunk so a larger
        // choice list degrades gracefully instead of 400ing.
        for chunk in elements.chunks(5) {
            blocks.push(json!({"type": "actions", "elements": chunk}));
        }
        let params = json!({"channel": channel, "text": body, "blocks": blocks});
        let response = reqwest::Client::new()
            .post(format!("{}/chat.postMessage", slack_api_base()))
            .header("Authorization", format!("Bearer {bot_token}"))
            .json(&params)
            .send()
            .await;
        match response {
            Ok(resp) => match resp.json::<Value>().await {
                Ok(value) if value.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) => true,
                Ok(value) => {
                    eprintln!(
                        "[slack] send_clarify failed: {}",
                        value
                            .get("error")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown")
                    );
                    false
                }
                Err(e) => {
                    eprintln!("[slack] send_clarify parse failed: {e}");
                    false
                }
            },
            Err(e) => {
                eprintln!("[slack] send_clarify failed: {e}");
                false
            }
        }
    }

    /// hermes `_clarify_resolved` cap.
    const CLARIFY_RESOLVED_MAX: usize = 1000;

    fn clarify_resolved_ledger() -> &'static std::sync::Mutex<std::collections::VecDeque<String>> {
        static LEDGER: std::sync::OnceLock<std::sync::Mutex<std::collections::VecDeque<String>>> =
            std::sync::OnceLock::new();
        LEDGER.get_or_init(|| std::sync::Mutex::new(std::collections::VecDeque::new()))
    }

    /// Double-click guard (hermes `_clarify_resolved.pop(msg_ts, True)`):
    /// the first click on a prompt message proceeds, later clicks bail.
    fn clarify_click_claimed(msg_ts: &str) -> bool {
        let mut ledger = clarify_resolved_ledger().lock().unwrap();
        if ledger.contains(&msg_ts.to_string()) {
            return false;
        }
        ledger.push_back(msg_ts.to_string());
        if ledger.len() > CLARIFY_RESOLVED_MAX {
            ledger.pop_front();
        }
        true
    }

    #[cfg(test)]
    fn reset_clarify_ledger_for_tests() {
        clarify_resolved_ledger().lock().unwrap().clear();
    }

    /// Route a block_actions clarify click (hermes `_handle_clarify_action`):
    /// authorized union (allowlist ∪ pairing) first, then the double-click
    /// guard, then Other → text-capture / numeric → resolve, rewriting the
    /// prompt message with the outcome either way.
    async fn handle_interactive_payload(
        cfg: &SlackConfig,
        bot_token: &str,
        payload: Value,
        pairing: Option<&crate::pairing::PairingStore>,
    ) {
        if payload.get("type").and_then(|v| v.as_str()).unwrap_or("") != "block_actions" {
            return;
        }
        let action = payload
            .get("actions")
            .and_then(|v| v.as_array())
            .and_then(|rows| rows.first())
            .cloned()
            .unwrap_or(json!({}));
        let action_id = action
            .get("action_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !action_id.starts_with("hermes_clarify_choice_") && action_id != "hermes_clarify_other" {
            return;
        }
        let value = action.get("value").and_then(|v| v.as_str()).unwrap_or("");
        let user_id = payload
            .pointer("/user/id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let user_name = payload
            .pointer("/user/name")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let channel_id = payload
            .pointer("/channel/id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let msg_ts = payload
            .pointer("/message/ts")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // Same union as inbound messages (hermes
        // `_is_interactive_user_authorized` → runner auth).
        let authorized = allowlisted(&cfg.allowed_channel_ids, &channel_id)
            || pairing
                .map(|store| store.is_approved("slack", &user_id))
                .unwrap_or(false);
        if !authorized {
            eprintln!("[slack] unauthorized clarify click by {user_name} ({user_id}) — ignoring");
            return;
        }

        // value packs `clarify_id|<idx|other>`.
        let Some((clarify_id, token)) = value.split_once('|') else {
            eprintln!("[slack] malformed clarify value: {value}");
            return;
        };
        // Double-click guard — the first click proceeds, later ones bail.
        if !clarify_click_claimed(&msg_ts) {
            return;
        }

        // Keep the original question so the resolved message keeps context
        // (first section block; Slack re-escapes entities in the payload,
        // so cap at the 3000-char section limit like hermes).
        let mut original_text = String::new();
        if let Some(blocks) = payload
            .pointer("/message/blocks")
            .and_then(|v| v.as_array())
        {
            for block in blocks {
                if block.get("type").and_then(|v| v.as_str()) == Some("section") {
                    original_text = block
                        .pointer("/text/text")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .chars()
                        .take(3000)
                        .collect();
                    break;
                }
            }
        }

        if action_id == "hermes_clarify_other" || token == "other" {
            if !crate::clarify_gateway::mark_awaiting_text(clarify_id) {
                update_clarify_message(
                    bot_token,
                    &channel_id,
                    &msg_ts,
                    &original_text,
                    &format!(
                        "⏳ This prompt expired — please send a new request. (by {user_name})"
                    ),
                )
                .await;
                return;
            }
            update_clarify_message(
                bot_token,
                &channel_id,
                &msg_ts,
                &original_text,
                &format!("✏️ Awaiting typed answer from {user_name}…"),
            )
            .await;
            return;
        }

        let Ok(idx) = token.parse::<usize>() else {
            eprintln!("[slack] invalid clarify choice token: {token}");
            return;
        };
        // Canonical choice text from the registry (hermes `_entries`
        // lookup); fall back to a positional label on a race.
        let resolved_text = crate::clarify_gateway::peek_choice(clarify_id, token)
            .unwrap_or_else(|| format!("choice {}", idx + 1));
        if crate::clarify_gateway::resolve(clarify_id, &resolved_text) {
            update_clarify_message(
                bot_token,
                &channel_id,
                &msg_ts,
                &original_text,
                &format!("✅ {user_name}: {resolved_text}"),
            )
            .await;
        } else {
            // Entry evicted / gateway restarted — surface expiry instead of
            // a misleading ✓ on a button the agent will never receive.
            update_clarify_message(
                bot_token,
                &channel_id,
                &msg_ts,
                &original_text,
                &format!("⏳ This prompt expired — please send a new request. (by {user_name})"),
            )
            .await;
        }
    }

    /// Rewrite a clarify message to show the outcome and drop the buttons
    /// (hermes `_update_clarify_message` — section + context blocks).
    async fn update_clarify_message(
        bot_token: &str,
        channel: &str,
        msg_ts: &str,
        question_text: &str,
        decision_text: &str,
    ) {
        let section_text = if question_text.is_empty() {
            "Clarification"
        } else {
            question_text
        };
        let params = json!({
            "channel": channel,
            "ts": msg_ts,
            "text": decision_text,
            "blocks": [
                {
                    "type": "section",
                    "text": {"type": "mrkdwn", "text": section_text},
                },
                {
                    "type": "context",
                    "elements": [{"type": "mrkdwn", "text": decision_text}],
                }
            ],
        });
        let response = reqwest::Client::new()
            .post(format!("{}/chat.update", slack_api_base()))
            .header("Authorization", format!("Bearer {bot_token}"))
            .json(&params)
            .send()
            .await;
        match response {
            Ok(resp) => match resp.json::<Value>().await {
                Ok(value) if !value.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) => {
                    eprintln!(
                        "[slack] chat.update failed: {}",
                        value
                            .get("error")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown")
                    );
                }
                Ok(_) => {}
                Err(e) => eprintln!("[slack] chat.update parse failed: {e}"),
            },
            Err(e) => eprintln!("[slack] chat.update failed: {e}"),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// axum mock of the Slack Web API — logs (method, body) per call.
        async fn spawn_slack_clarify_api(
            log: Arc<std::sync::Mutex<Vec<(String, Value)>>>,
            response_ok: bool,
        ) -> String {
            use axum::extract::State;
            use axum::routing::post;
            type Log = Arc<std::sync::Mutex<Vec<(String, Value)>>>;
            let app = axum::Router::new()
                .route(
                    "/chat.postMessage",
                    post(
                        move |State(log): State<Log>, axum::Json(body): axum::Json<Value>| async move {
                            log.lock().unwrap().push(("chat.postMessage".into(), body));
                            axum::Json(json!({"ok": response_ok, "ts": "111.222"}))
                        },
                    ),
                )
                .route(
                    "/chat.update",
                    post(
                        move |State(log): State<Log>, axum::Json(body): axum::Json<Value>| async move {
                            log.lock().unwrap().push(("chat.update".into(), body));
                            axum::Json(json!({"ok": response_ok}))
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

        fn block_actions_payload(clarify_id: &str, token: &str, action_id: &str) -> Value {
            json!({
                "type": "block_actions",
                "user": {"id": "U7", "name": "ann"},
                "channel": {"id": "C42"},
                "message": {
                    "ts": "111.222",
                    "blocks": [{
                        "type": "section",
                        "text": {"type": "mrkdwn", "text": "❓ Pick"},
                    }],
                },
                "actions": [{
                    "action_id": action_id,
                    "value": format!("{clarify_id}|{token}"),
                }],
            })
        }

        fn authorized_cfg() -> SlackConfig {
            SlackConfig {
                allowed_channel_ids: vec!["C42".into()],
                ..Default::default()
            }
        }

        #[tokio::test]
        async fn slack_send_clarify_posts_block_kit_buttons() {
            let _env_guard = crate::models_dev::test_env_lock();
            let log = Arc::new(std::sync::Mutex::new(Vec::<(String, Value)>::new()));
            let base = spawn_slack_clarify_api(log.clone(), true).await;
            std::env::set_var("SLACK_API_BASE", &base);
            let ok = send_clarify_message(
                "xoxb-TEST",
                "C42",
                "abc123def456",
                "Pick <one> & more",
                &["Alpha".into(), "Beta".into()],
            )
            .await;
            std::env::remove_var("SLACK_API_BASE");
            assert!(ok);
            let reqs = log.lock().unwrap();
            assert_eq!(reqs.len(), 1);
            let (method, body) = &reqs[0];
            assert_eq!(method, "chat.postMessage");
            // mrkdwn control chars escaped; fallback text = the body.
            let section = &body["blocks"][0];
            assert_eq!(section["type"], "section");
            assert_eq!(section["text"]["text"], "❓ Pick &lt;one&gt; &amp; more");
            assert_eq!(body["text"], "❓ Pick &lt;one&gt; &amp; more");
            // One actions block: per-choice buttons + Other.
            let actions = &body["blocks"][1];
            assert_eq!(actions["type"], "actions");
            let elements = actions["elements"].as_array().unwrap();
            assert_eq!(elements.len(), 3);
            assert_eq!(elements[0]["text"]["text"], "Alpha");
            assert_eq!(elements[0]["action_id"], "hermes_clarify_choice_0");
            assert_eq!(elements[0]["value"], "abc123def456|0");
            assert_eq!(elements[1]["action_id"], "hermes_clarify_choice_1");
            assert_eq!(elements[1]["value"], "abc123def456|1");
            assert_eq!(elements[2]["text"]["text"], "✏️ Other…");
            assert_eq!(elements[2]["action_id"], "hermes_clarify_other");
            assert_eq!(elements[2]["value"], "abc123def456|other");
        }

        #[tokio::test]
        async fn slack_send_clarify_api_failure_returns_false() {
            let _env_guard = crate::models_dev::test_env_lock();
            let log = Arc::new(std::sync::Mutex::new(Vec::<(String, Value)>::new()));
            let base = spawn_slack_clarify_api(log.clone(), false).await;
            std::env::set_var("SLACK_API_BASE", &base);
            let ok = send_clarify_message(
                "xoxb-TEST",
                "C42",
                "abc123def456",
                "Pick",
                &["Alpha".into()],
            )
            .await;
            std::env::remove_var("SLACK_API_BASE");
            // False → messaging_clarify_fn sends the numbered-text fallback.
            assert!(!ok);
        }

        #[tokio::test]
        async fn slack_interactive_numeric_choice_resolves() {
            let _env_guard = crate::models_dev::test_env_lock();
            let _clarify_guard = crate::clarify_gateway::test_lock().lock().unwrap();
            crate::clarify_gateway::reset_for_tests();
            reset_clarify_ledger_for_tests();
            let log = Arc::new(std::sync::Mutex::new(Vec::<(String, Value)>::new()));
            let base = spawn_slack_clarify_api(log.clone(), true).await;
            std::env::set_var("SLACK_API_BASE", &base);
            let handle = crate::clarify_gateway::register(
                "platform-slack-C42",
                "Pick",
                &["Alpha".into(), "Beta".into()],
                false,
            );
            let clarify_id = handle.clarify_id.clone();
            let payload =
                block_actions_payload(&clarify_id, "1", &format!("hermes_clarify_choice_1"));
            handle_interactive_payload(&authorized_cfg(), "xoxb-TEST", payload, None).await;
            std::env::remove_var("SLACK_API_BASE");
            assert_eq!(handle.rx.await.unwrap(), "Beta");
            let reqs = log.lock().unwrap();
            assert_eq!(reqs.len(), 1);
            let (method, body) = &reqs[0];
            assert_eq!(method, "chat.update");
            assert_eq!(body["channel"], "C42");
            assert_eq!(body["ts"], "111.222");
            assert_eq!(body["text"], "✅ ann: Beta");
            // Buttons dropped: section (original question) + context only.
            let blocks = body["blocks"].as_array().unwrap();
            assert_eq!(blocks.len(), 2);
            assert_eq!(blocks[0]["type"], "section");
            assert_eq!(blocks[0]["text"]["text"], "❓ Pick");
            assert_eq!(blocks[1]["type"], "context");
            assert_eq!(blocks[1]["elements"][0]["text"], "✅ ann: Beta");
        }

        #[tokio::test]
        async fn slack_interactive_other_flips_text_capture() {
            let _env_guard = crate::models_dev::test_env_lock();
            let _clarify_guard = crate::clarify_gateway::test_lock().lock().unwrap();
            crate::clarify_gateway::reset_for_tests();
            reset_clarify_ledger_for_tests();
            let log = Arc::new(std::sync::Mutex::new(Vec::<(String, Value)>::new()));
            let base = spawn_slack_clarify_api(log.clone(), true).await;
            std::env::set_var("SLACK_API_BASE", &base);
            let handle = crate::clarify_gateway::register(
                "platform-slack-C42",
                "Pick",
                &["Alpha".into(), "Beta".into()],
                false,
            );
            let clarify_id = handle.clarify_id.clone();
            let payload = block_actions_payload(&clarify_id, "other", "hermes_clarify_other");
            handle_interactive_payload(&authorized_cfg(), "xoxb-TEST", payload, None).await;
            std::env::remove_var("SLACK_API_BASE");
            // Entry survives in text-capture mode; the next typed message in
            // the session resolves it.
            let pending = crate::clarify_gateway::pending_for_session("platform-slack-C42")
                .expect("entry must survive the Other click");
            assert!(pending.awaiting_text);
            assert!(crate::clarify_gateway::resolve(&clarify_id, "typed answer"));
            assert_eq!(handle.rx.await.unwrap(), "typed answer");
            let reqs = log.lock().unwrap();
            assert_eq!(reqs.len(), 1);
            assert_eq!(reqs[0].1["text"], "✏️ Awaiting typed answer from ann…");
        }

        #[tokio::test]
        async fn slack_interactive_double_click_guarded() {
            let _env_guard = crate::models_dev::test_env_lock();
            let _clarify_guard = crate::clarify_gateway::test_lock().lock().unwrap();
            crate::clarify_gateway::reset_for_tests();
            reset_clarify_ledger_for_tests();
            let log = Arc::new(std::sync::Mutex::new(Vec::<(String, Value)>::new()));
            let base = spawn_slack_clarify_api(log.clone(), true).await;
            std::env::set_var("SLACK_API_BASE", &base);
            let handle = crate::clarify_gateway::register(
                "platform-slack-C42",
                "Pick",
                &["Alpha".into(), "Beta".into()],
                false,
            );
            let clarify_id = handle.clarify_id.clone();
            let first = block_actions_payload(&clarify_id, "0", "hermes_clarify_choice_0");
            handle_interactive_payload(&authorized_cfg(), "xoxb-TEST", first, None).await;
            // Second click on the same prompt message bails silently
            // (hermes `_clarify_resolved` atomic pop).
            let second = block_actions_payload(&clarify_id, "1", "hermes_clarify_choice_1");
            handle_interactive_payload(&authorized_cfg(), "xoxb-TEST", second, None).await;
            std::env::remove_var("SLACK_API_BASE");
            assert_eq!(handle.rx.await.unwrap(), "Alpha");
            assert_eq!(log.lock().unwrap().len(), 1);
        }

        #[tokio::test]
        async fn slack_interactive_unauthorized_ignored() {
            let _env_guard = crate::models_dev::test_env_lock();
            let _clarify_guard = crate::clarify_gateway::test_lock().lock().unwrap();
            crate::clarify_gateway::reset_for_tests();
            reset_clarify_ledger_for_tests();
            let log = Arc::new(std::sync::Mutex::new(Vec::<(String, Value)>::new()));
            let base = spawn_slack_clarify_api(log.clone(), true).await;
            std::env::set_var("SLACK_API_BASE", &base);
            let handle = crate::clarify_gateway::register(
                "platform-slack-C42",
                "Pick",
                &["Alpha".into()],
                false,
            );
            let clarify_id = handle.clarify_id.clone();
            let payload = block_actions_payload(&clarify_id, "0", "hermes_clarify_choice_0");
            // Channel C42 is not allowlisted and no pairing store exists.
            let cfg = SlackConfig {
                allowed_channel_ids: vec!["C999".into()],
                ..Default::default()
            };
            handle_interactive_payload(&cfg, "xoxb-TEST", payload, None).await;
            std::env::remove_var("SLACK_API_BASE");
            assert!(log.lock().unwrap().is_empty());
            // The waiter is untouched — the legitimate user can still answer.
            assert!(crate::clarify_gateway::contains(&clarify_id));
            assert!(crate::clarify_gateway::resolve(&clarify_id, "Alpha"));
        }

        #[tokio::test]
        async fn slack_interactive_expired_prompt_updates_message() {
            let _env_guard = crate::models_dev::test_env_lock();
            let _clarify_guard = crate::clarify_gateway::test_lock().lock().unwrap();
            crate::clarify_gateway::reset_for_tests();
            reset_clarify_ledger_for_tests();
            let log = Arc::new(std::sync::Mutex::new(Vec::<(String, Value)>::new()));
            let base = spawn_slack_clarify_api(log.clone(), true).await;
            std::env::set_var("SLACK_API_BASE", &base);
            let handle = crate::clarify_gateway::register(
                "platform-slack-C42",
                "Pick",
                &["Alpha".into(), "Beta".into()],
                false,
            );
            let clarify_id = handle.clarify_id.clone();
            // The clarify tool gave up (receiver dropped) before the click.
            drop(handle.rx);
            let payload = block_actions_payload(&clarify_id, "1", "hermes_clarify_choice_1");
            handle_interactive_payload(&authorized_cfg(), "xoxb-TEST", payload, None).await;
            std::env::remove_var("SLACK_API_BASE");
            let reqs = log.lock().unwrap();
            assert_eq!(reqs.len(), 1);
            assert_eq!(reqs[0].0, "chat.update");
            assert_eq!(
                reqs[0].1["text"],
                "⏳ This prompt expired — please send a new request. (by ann)"
            );
        }
    }
}

/// Split text into chunks no longer than `max` chars, preferring newline
/// boundaries (hermes message splitting).
pub(crate) fn chunk_text(text: &str, max: usize) -> Vec<String> {
    if text.chars().count() <= max {
        return vec![text.to_string()];
    }
    let mut out = Vec::new();
    let mut current = String::new();
    for line in text.split_inclusive('\n') {
        if current.chars().count() + line.chars().count() > max && !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
        // A single line longer than max must be hard-split.
        let mut rest = line;
        while rest.chars().count() > max {
            let split_at = rest
                .char_indices()
                .nth(max)
                .map(|(idx, _)| idx)
                .unwrap_or(rest.len());
            let (head, tail) = rest.split_at(split_at);
            out.push(head.to_string());
            rest = tail;
        }
        current.push_str(rest);
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn approval_command_parsing() {
        let cmd = parse_approval_command("/approve").unwrap();
        assert!(cmd.approve && !cmd.all && cmd.choice == crate::approval_gateway::CHOICE_ONCE);
        let cmd = parse_approval_command("/approve all session").unwrap();
        assert!(cmd.all && cmd.choice == crate::approval_gateway::CHOICE_SESSION);
        let cmd = parse_approval_command("/approve always").unwrap();
        assert_eq!(cmd.choice, crate::approval_gateway::CHOICE_ALWAYS);
        let cmd = parse_approval_command("/deny all because reasons").unwrap();
        assert!(!cmd.approve && cmd.all && cmd.choice == crate::approval_gateway::CHOICE_DENY);
        assert!(parse_approval_command("approve").is_none()); // slash form only
        assert!(parse_approval_command("/approve2").is_none());
        assert!(parse_approval_command("hello world").is_none());
    }

    #[test]
    fn approval_text_format() {
        let text = format_approval_text("rm -rf /", "dangerous command", true, true, false);
        assert!(text.contains("Dangerous command requires approval"));
        assert!(text.contains("```\nrm -rf /\n```"));
        assert!(text.contains("Reason: dangerous command"));
        assert!(text.contains("/approve session"));
        assert!(text.contains("/approve always"));
        assert!(text.contains("/deny"));
        let smart = format_approval_text("c", "d", true, true, true);
        assert!(smart.contains("Smart DENY"));
        assert!(!smart.contains("/approve session"));
        // Long commands truncate to 200 chars + ellipsis.
        let long = format_approval_text(&"x".repeat(500), "d", true, true, false);
        assert!(long.contains(&"x".repeat(200)));
        assert!(!long.contains(&"x".repeat(201)));
    }

    #[test]
    fn approval_intercept_flow() {
        let session = "platform-teams-intercept";
        // No pending approvals: informational reply.
        let cmd = parse_approval_command("/approve").unwrap();
        assert_eq!(
            apply_approval_command(session, cmd),
            "No pending approvals."
        );
        // Pending approval resolves oldest-first.
        let mut first = crate::approval_gateway::register(session, "cmd-a", "d", false, true, true);
        let mut second =
            crate::approval_gateway::register(session, "cmd-b", "d", false, true, true);
        let cmd = parse_approval_command("/approve").unwrap();
        assert_eq!(apply_approval_command(session, cmd), "✅ Allowed (once)");
        assert_eq!(first.rx.try_recv().unwrap(), "once");
        assert!(second.rx.try_recv().is_err());
        let cmd = parse_approval_command("/deny all").unwrap();
        assert_eq!(apply_approval_command(session, cmd), "❌ Denied");
        assert_eq!(second.rx.try_recv().unwrap(), "deny");
    }

    use super::*;

    #[test]
    fn config_defaults_disabled() {
        let cfg = MessagingConfig::default();
        assert!(!cfg.telegram.enabled && !cfg.discord.enabled && !cfg.slack.enabled);
    }

    #[test]
    fn allowlist_gate() {
        assert!(allowlisted(&["123".into()], "123"));
        assert!(!allowlisted(&["123".into()], "456"));
        assert!(!allowlisted(&[], "123"), "empty allowlist fails closed");
    }

    #[test]
    fn extract_media_tags_splits_paths_and_text() {
        let dir = std::env::temp_dir().join(format!(
            "ulnclaw-media-tags-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let media = dir.join("clip.mp4");
        std::fs::write(&media, b"fake").unwrap();

        // Existing path is extracted; missing paths stay as literal text.
        let reply = format!(
            "Here is your clip!\nMEDIA: {}\nMEDIA: /does/not/exist.png\nEnjoy!",
            media.display()
        );
        let (text, paths) = extract_media_tags(&reply);
        assert_eq!(paths, vec![media.clone()]);
        assert!(text.contains("Here is your clip!"));
        assert!(text.contains("Enjoy!"));
        assert!(
            text.contains("/does/not/exist.png"),
            "missing files stay literal"
        );
        assert!(!text.contains(&format!("MEDIA: {}", media.display())));

        // Tilde expansion (restore HOME — env is process-global).
        let previous_home = std::env::var_os("HOME");
        std::env::set_var("HOME", dir.to_string_lossy().to_string());
        let home_media = dir.join("home.mp3");
        std::fs::write(&home_media, b"x").unwrap();
        let (text, paths) = extract_media_tags("MEDIA: ~/home.mp3");
        assert_eq!(paths, vec![home_media]);
        assert!(text.trim().is_empty());
        match previous_home {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }

        // No tags → unchanged.
        let (text, paths) = extract_media_tags("plain reply");
        assert!(paths.is_empty());
        assert_eq!(text, "plain reply");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn attachment_note_lists_paths_and_hints() {
        assert_eq!(attachment_note(&[]), "");
        let note = attachment_note(&[MediaAttachment {
            path: std::path::PathBuf::from("/tmp/cache/abc.jpg"),
            mime: "image/jpeg".into(),
            bytes: 1234,
            original_name: "pic.jpg".into(),
        }]);
        assert!(note.contains("/tmp/cache/abc.jpg"));
        assert!(note.contains("image/jpeg"));
        assert!(note.contains("1234 bytes"));
        assert!(note.contains("pic.jpg"));
        assert!(note.contains("vision_analyze"));
    }

    #[test]
    fn split_injectable_images_filters_mime_and_size() {
        let img_ok = MediaAttachment {
            path: std::path::PathBuf::from("/tmp/cache/a.png"),
            mime: "image/png".into(),
            bytes: 1024,
            original_name: "a.png".into(),
        };
        let img_big = MediaAttachment {
            path: std::path::PathBuf::from("/tmp/cache/big.png"),
            mime: "image/png".into(),
            bytes: MAX_INLINE_IMAGE_BYTES + 1,
            original_name: "big.png".into(),
        };
        let video = MediaAttachment {
            path: std::path::PathBuf::from("/tmp/cache/v.mp4"),
            mime: "video/mp4".into(),
            bytes: 2048,
            original_name: "v.mp4".into(),
        };
        let attachments: Vec<&MediaAttachment> = vec![&img_ok, &img_big, &video];
        let (images, rest) = split_injectable_images(&attachments);
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].path, img_ok.path);
        assert_eq!(rest.len(), 2);
    }

    #[test]
    fn data_url_from_cache_encodes_file_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("img.bin");
        std::fs::write(&path, b"hello").unwrap();
        let attachment = MediaAttachment {
            path,
            mime: "image/png".into(),
            bytes: 5,
            original_name: "img.png".into(),
        };
        let url = data_url_from_cache(&attachment).expect("encodes");
        assert_eq!(url, "data:image/png;base64,aGVsbG8=");
        // Missing file degrades to None (caller keeps the path ref).
        let missing = MediaAttachment {
            path: dir.path().join("nope.bin"),
            mime: "image/png".into(),
            bytes: 5,
            original_name: "x".into(),
        };
        assert!(data_url_from_cache(&missing).is_none());
    }

    #[test]
    fn session_key_is_deterministic() {
        let event = MessageEvent {
            platform: "telegram".into(),
            chat_id: "-100123".into(),
            sender_id: "1".into(),
            sender_name: "u".into(),
            text: "hi".into(),
            message_id: "m".into(),
            attachments: Vec::new(),
        };
        assert_eq!(Dispatcher::session_key(&event), "platform-telegram--100123");
    }

    #[test]
    fn chunk_short_text_unchanged() {
        assert_eq!(chunk_text("hello", 10), vec!["hello".to_string()]);
    }

    #[test]
    fn chunk_prefers_newlines() {
        let text = "aaaa\nbbbb\ncccc";
        let chunks = chunk_text(text, 10);
        assert!(chunks.iter().all(|c| c.chars().count() <= 10));
        assert_eq!(chunks.join(""), text);
    }

    #[test]
    fn chunk_hard_splits_long_lines() {
        let text = "x".repeat(25);
        let chunks = chunk_text(&text, 10);
        assert_eq!(chunks.len(), 3);
        assert!(chunks.iter().all(|c| c.chars().count() <= 10));
        assert_eq!(chunks.join(""), text);
    }

    #[test]
    fn slack_mention_stripped() {
        assert_eq!(slack::strip_mention("<@U123> hello"), "hello");
        assert_eq!(slack::strip_mention("plain text"), "plain text");
    }

    #[test]
    fn token_resolution_prefers_config() {
        assert_eq!(
            resolve_token("abc", "ULNCLAW_NEVER_SET_XYZ").as_deref(),
            Some("abc")
        );
        assert_eq!(resolve_token("  ", "ULNCLAW_NEVER_SET_XYZ"), None);
    }

    #[test]
    fn slack_typing_status_text_selection() {
        use std::time::Duration;
        // Default while fresh; elapsed heartbeat after 30 s (hermes #45702).
        assert_eq!(
            slack::typing_status_text(Duration::from_secs(0), None),
            "is thinking..."
        );
        assert_eq!(
            slack::typing_status_text(Duration::from_secs(29), None),
            "is thinking..."
        );
        assert_eq!(
            slack::typing_status_text(Duration::from_secs(30), None),
            "still working… (30s)"
        );
        assert_eq!(
            slack::typing_status_text(Duration::from_secs(63), None),
            "still working… (1m03s)"
        );
        assert_eq!(
            slack::typing_status_text(Duration::from_secs(3661), None),
            "still working… (61m01s)"
        );
        // Configured text always wins, even after the heartbeat threshold;
        // an empty configured value falls back to the defaults.
        assert_eq!(
            slack::typing_status_text(Duration::from_secs(90), Some("brewing…")),
            "brewing…"
        );
        assert_eq!(
            slack::typing_status_text(Duration::from_secs(0), Some("")),
            "is thinking..."
        );
    }

    #[tokio::test]
    async fn slack_generic_mode_rotates_status_phrases() {
        // P722: hermes long_running_notifications="generic" parity —
        // the Slack typing line rotates through the status-phrase
        // catalog after 30 s, one phrase per 30 s window.
        use std::time::Duration;
        let _env_guard = crate::models_dev::test_env_lock();
        let _home_guard = IsolatedHome::with_config(Some(
            "[display.platforms.slack]\nlong_running_notifications = \"generic\"\n",
        ));
        let mut ctx = slack::GenericPhraseCtx::load().expect("generic mode engages");
        assert_eq!(ctx.status_for(Duration::from_secs(10)), "is thinking...");
        let first = ctx.status_for(Duration::from_secs(31));
        assert!(!first.is_empty());
        assert_ne!(first, "is thinking...");
        // Same 30 s window → stable phrase (no 2 s-refresh flicker).
        assert_eq!(ctx.status_for(Duration::from_secs(45)), first);
        // Next window → rotates, avoiding immediate repeats.
        let second = ctx.status_for(Duration::from_secs(61));
        assert_ne!(second, first);
    }

    #[tokio::test]
    async fn slack_generic_mode_disengaged_by_default() {
        // P722: without the "generic" visibility mode the typing line
        // keeps the elapsed-time heartbeat.
        let _env_guard = crate::models_dev::test_env_lock();
        let _home_guard = IsolatedHome::new();
        assert!(slack::GenericPhraseCtx::load().is_none());
        let _home_guard2 =
            IsolatedHome::with_config(Some("[display]\nlong_running_notifications = true\n"));
        assert!(slack::GenericPhraseCtx::load().is_none());
    }

    #[test]
    fn slack_api_base_env_override() {
        let _guard = crate::models_dev::test_env_lock();
        std::env::remove_var("SLACK_API_BASE");
        assert_eq!(slack::slack_api_base(), "https://slack.com/api");
        std::env::set_var("SLACK_API_BASE", "http://127.0.0.1:9");
        assert_eq!(slack::slack_api_base(), "http://127.0.0.1:9");
        std::env::remove_var("SLACK_API_BASE");
    }

    async fn spawn_slack_status_server(
        log: Arc<Mutex<Vec<(String, Value)>>>,
        response_ok: bool,
    ) -> String {
        use axum::extract::State;
        use axum::routing::post;
        let app = axum::Router::new()
            .route(
                "/assistant.threads.setStatus",
                post(
                    move |State(log): State<Arc<Mutex<Vec<(String, Value)>>>>,
                          headers: axum::http::HeaderMap,
                          axum::Json(body): axum::Json<Value>| async move {
                        let auth = headers
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("")
                            .to_string();
                        log.lock().await.push((auth, body));
                        axum::Json(json!({ "ok": response_ok }))
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
    async fn slack_set_thread_status_posts_assistant_payload() {
        let _guard = crate::models_dev::test_env_lock();
        let log: Arc<Mutex<Vec<(String, Value)>>> = Arc::new(Mutex::new(Vec::new()));
        let base = spawn_slack_status_server(log.clone(), true).await;
        std::env::set_var("SLACK_API_BASE", &base);
        assert!(slack::set_thread_status("xoxb-test", "C123", "111.222", "is thinking...").await);
        std::env::remove_var("SLACK_API_BASE");
        let entries = log.lock().await.clone();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "Bearer xoxb-test");
        assert_eq!(entries[0].1["channel_id"], "C123");
        assert_eq!(entries[0].1["thread_ts"], "111.222");
        assert_eq!(entries[0].1["status"], "is thinking...");
    }

    #[tokio::test]
    async fn slack_set_thread_status_reports_api_failure() {
        let _guard = crate::models_dev::test_env_lock();
        let log: Arc<Mutex<Vec<(String, Value)>>> = Arc::new(Mutex::new(Vec::new()));
        let base = spawn_slack_status_server(log.clone(), false).await;
        std::env::set_var("SLACK_API_BASE", &base);
        let ok = slack::set_thread_status("xoxb-test", "C1", "1.0", "x").await;
        std::env::remove_var("SLACK_API_BASE");
        assert!(!ok);
    }

    #[tokio::test]
    async fn slack_typing_heartbeat_refreshes_then_clears() {
        let _guard = crate::models_dev::test_env_lock();
        let log: Arc<Mutex<Vec<(String, Value)>>> = Arc::new(Mutex::new(Vec::new()));
        let base = spawn_slack_status_server(log.clone(), true).await;
        std::env::set_var("SLACK_API_BASE", &base);
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(slack::typing_heartbeat(
            "xoxb-test".to_string(),
            "C123".to_string(),
            "111.222".to_string(),
            None,
            std::time::Duration::from_millis(40),
            stop_rx,
        ));
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        stop_tx.send(true).unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
        std::env::remove_var("SLACK_API_BASE");
        let statuses: Vec<String> = log
            .lock()
            .await
            .iter()
            .map(|(_, body)| body["status"].as_str().unwrap_or("").to_string())
            .collect();
        assert!(statuses.len() >= 3, "expected refreshes, got {statuses:?}");
        assert_eq!(statuses[0], "is thinking...");
        // hermes stop_typing: the final call clears the status.
        assert_eq!(statuses.last().map(String::as_str), Some(""));
        assert!(statuses[..statuses.len() - 1]
            .iter()
            .all(|s| s == "is thinking..."));
    }

    // ------------------------------------------------------------------
    // Slash-confirm delivery (P244)
    // ------------------------------------------------------------------

    /// The pending-confirmation registry is process-global; serialize the
    /// tests that register/clear entries (parallel clears race).
    fn slash_confirm_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn slash_confirm_reply_parsing() {
        use crate::slash_confirm::ConfirmChoice;
        // Slash-command forms.
        assert_eq!(
            parse_slash_confirm_reply("/approve"),
            Some(ConfirmChoice::Once)
        );
        assert_eq!(parse_slash_confirm_reply("/yes"), Some(ConfirmChoice::Once));
        assert_eq!(
            parse_slash_confirm_reply("/always"),
            Some(ConfirmChoice::Always)
        );
        assert_eq!(
            parse_slash_confirm_reply("/remember"),
            Some(ConfirmChoice::Always)
        );
        assert_eq!(
            parse_slash_confirm_reply("/cancel"),
            Some(ConfirmChoice::Cancel)
        );
        assert_eq!(
            parse_slash_confirm_reply("/deny"),
            Some(ConfirmChoice::Cancel)
        );
        // Plain text + bang-prefixed (Slack-style).
        assert_eq!(
            parse_slash_confirm_reply("approve once"),
            Some(ConfirmChoice::Once)
        );
        assert_eq!(
            parse_slash_confirm_reply("!always"),
            Some(ConfirmChoice::Always)
        );
        assert_eq!(
            parse_slash_confirm_reply("nevermind"),
            Some(ConfirmChoice::Cancel)
        );
        // Unrelated text falls through.
        assert_eq!(parse_slash_confirm_reply("hello there"), None);
        assert_eq!(parse_slash_confirm_reply(""), None);
        assert_eq!(parse_slash_confirm_reply("/something-else"), None);
    }

    fn test_event(text: &str, message_id: &str) -> MessageEvent {
        MessageEvent {
            platform: "testplat".into(),
            chat_id: "chat-1".into(),
            sender_id: "u1".into(),
            sender_name: "User".into(),
            text: text.into(),
            message_id: message_id.into(),
            attachments: Vec::new(),
        }
    }

    fn test_dispatcher() -> Arc<Dispatcher> {
        let temp = tempfile::tempdir().expect("tempdir");
        let store =
            Arc::new(SqliteSessionStore::open(temp.path().join("state.db")).expect("store opens"));
        std::mem::forget(temp);
        let provider = Arc::new(
            crate::provider::openai::OpenAiProvider::builder()
                .endpoint("http://127.0.0.1:9/v1")
                .model("test-model")
                .name("test")
                .build()
                .expect("provider builds"),
        );
        let agent = crate::agent::Agent::new(provider, crate::tools::ToolRegistry::new())
            .with_store(store.clone());
        Dispatcher::new(Arc::new(agent), store)
    }

    /// Dispatcher whose provider is unreachable (turns fail AFTER the
    /// session row is created) — used by the routing test.
    fn test_dispatcher_parts() -> (
        Arc<crate::agent::Agent>,
        Arc<SqliteSessionStore>,
        Arc<Dispatcher>,
    ) {
        let temp = tempfile::tempdir().expect("tempdir");
        let store =
            Arc::new(SqliteSessionStore::open(temp.path().join("state.db")).expect("store opens"));
        std::mem::forget(temp);
        let provider = Arc::new(
            crate::provider::openai::OpenAiProvider::builder()
                .endpoint("http://127.0.0.1:9/v1")
                .model("test-model")
                .name("test")
                .build()
                .expect("provider builds"),
        );
        let agent = Arc::new(
            crate::agent::Agent::new(provider, crate::tools::ToolRegistry::new())
                .with_store(store.clone()),
        );
        let dispatcher = Dispatcher::new(agent.clone(), store.clone());
        (agent, store, dispatcher)
    }

    #[tokio::test]
    async fn profile_routing_delegates_matching_events_to_profile_stack() {
        // P709: events matching a configured route run under the
        // routed profile's dispatcher (its store gains the session);
        // non-matching events stay on the default stack.
        let (default_agent, default_store, _) = test_dispatcher_parts();
        let (_profile_agent, profile_store, profile_dispatcher) = test_dispatcher_parts();
        let captured = profile_dispatcher.clone();
        let factory: Arc<dyn ProfileDispatcherFactory> = Arc::new(move |_name: String| {
            let dispatcher = captured.clone();
            async move { Ok(dispatcher) }
        });
        let route = crate::profile_routing::ProfileRouteSpec {
            name: "work".into(),
            platform: "testplat".into(),
            profile: "work-profile".into(),
            chat_id: Some("routed-chat".into()),
            ..Default::default()
        };
        let routes = crate::profile_routing::parse_profile_routes(&[route]);
        let hub = ProfileRoutingHub::new(routes, factory);
        let router = Dispatcher::new_with_routing(default_agent, default_store.clone(), hub);

        // Routed chat: the turn lands in the PROFILE store (the turn
        // itself fails — unreachable test provider — but the session
        // row is created before the provider call).
        let _ = router
            .handle_event(MessageEvent {
                platform: "testplat".into(),
                chat_id: "routed-chat".into(),
                sender_id: "u1".into(),
                sender_name: "User".into(),
                text: "hello routed".into(),
                message_id: "m1".into(),
                attachments: Vec::new(),
            })
            .await;
        assert_eq!(profile_store.count_sessions().unwrap(), 1);
        assert_eq!(default_store.count_sessions().unwrap(), 0);

        // Non-matching chat stays on the default stack.
        let _ = router
            .handle_event(MessageEvent {
                platform: "testplat".into(),
                chat_id: "other-chat".into(),
                sender_id: "u1".into(),
                sender_name: "User".into(),
                text: "hello default".into(),
                message_id: "m2".into(),
                attachments: Vec::new(),
            })
            .await;
        assert_eq!(profile_store.count_sessions().unwrap(), 1);
        assert_eq!(default_store.count_sessions().unwrap(), 1);
    }

    #[tokio::test]
    async fn profile_routing_falls_back_when_factory_fails() {
        // P709: a profile build failure degrades to the default
        // profile — routing never drops a message.
        let (default_agent, default_store, _) = test_dispatcher_parts();
        let factory: Arc<dyn ProfileDispatcherFactory> =
            Arc::new(move |_name: String| async move { Err("boom".to_string()) });
        let route = crate::profile_routing::ProfileRouteSpec {
            name: "broken".into(),
            platform: "testplat".into(),
            profile: "broken-profile".into(),
            chat_id: Some("routed-chat".into()),
            ..Default::default()
        };
        let routes = crate::profile_routing::parse_profile_routes(&[route]);
        let hub = ProfileRoutingHub::new(routes, factory);
        let router = Dispatcher::new_with_routing(default_agent, default_store.clone(), hub);
        let _ = router
            .handle_event(MessageEvent {
                platform: "testplat".into(),
                chat_id: "routed-chat".into(),
                sender_id: "u1".into(),
                sender_name: "User".into(),
                text: "hello fallback".into(),
                message_id: "m1".into(),
                attachments: Vec::new(),
            })
            .await;
        // The default stack served the event.
        assert_eq!(default_store.count_sessions().unwrap(), 1);
    }

    #[tokio::test]
    async fn reload_mcp_prompts_then_approve_reloads_and_injects_note() {
        let _guard = slash_confirm_test_lock();
        crate::slash_confirm::clear_all_for_tests();
        let dispatcher = test_dispatcher();
        let key = "platform-testplat-chat-1".to_string();

        // /reload-mcp is gated (approvals.mcp_reload_confirm defaults on):
        // the text-fallback prompt goes out and a confirm registers.
        let outcome = dispatcher
            .handle_event(test_event("/reload-mcp", "m1"))
            .await
            .unwrap();
        assert!(
            outcome.reply.contains("Confirm /reload-mcp"),
            "{}",
            outcome.reply
        );
        assert!(outcome.reply.contains("/approve"), "{}", outcome.reply);
        assert!(crate::slash_confirm::get_pending(&key).is_some());

        // /approve resolves the pending confirm and runs the reload.
        let outcome = dispatcher
            .handle_event(test_event("/approve", "m2"))
            .await
            .unwrap();
        assert!(
            outcome.reply.contains("No MCP servers connected"),
            "{}",
            outcome.reply
        );
        assert!(crate::slash_confirm::get_pending(&key).is_none());

        // The change note lands at the END of the session history.
        let histories = chat_histories().lock().await;
        let history = histories.get(&key).expect("history created");
        let last = history.last().expect("note appended");
        assert!(last
            .content
            .as_deref()
            .unwrap()
            .contains("[system note] MCP tools were just reloaded"));
    }

    #[tokio::test]
    async fn reload_mcp_cancel_keeps_everything() {
        let _guard = slash_confirm_test_lock();
        crate::slash_confirm::clear_all_for_tests();
        let dispatcher = test_dispatcher();
        let key = "platform-testplat-chat-1".to_string();

        dispatcher
            .handle_event(test_event("/reload-mcp", "m1"))
            .await
            .unwrap();
        let outcome = dispatcher
            .handle_event(test_event("cancel", "m2"))
            .await
            .unwrap();
        assert!(outcome.reply.contains("cancelled"), "{}", outcome.reply);
        assert!(crate::slash_confirm::get_pending(&key).is_none());
        let histories = chat_histories().lock().await;
        let history = histories.get(&key);
        assert!(
            history.map(|h| h.is_empty()).unwrap_or(true),
            "no change note after cancel"
        );
    }

    #[tokio::test]
    async fn unrelated_message_does_not_resolve_pending_confirm() {
        let _guard = slash_confirm_test_lock();
        crate::slash_confirm::clear_all_for_tests();
        let dispatcher = test_dispatcher();
        let key = "platform-testplat-chat-1".to_string();

        dispatcher
            .handle_event(test_event("/reload-mcp", "m1"))
            .await
            .unwrap();
        // Busy flag short-circuits the agent turn; the point is the
        // intercept: "hello" matches no confirm keyword, so the pending
        // confirm survives (not stale yet).
        dispatcher.busy.lock().await.insert(key.clone(), true);
        let outcome = dispatcher
            .handle_event(test_event("hello", "m2"))
            .await
            .unwrap();
        assert!(outcome.reply.contains("queued"), "{}", outcome.reply);
        assert!(crate::slash_confirm::get_pending(&key).is_some());
        dispatcher.busy.lock().await.insert(key.clone(), false);
        crate::slash_confirm::clear_all_for_tests();
    }

    #[tokio::test]
    async fn request_slash_confirm_registers_before_sending_and_supersedes() {
        let _guard = slash_confirm_test_lock();
        crate::slash_confirm::clear_all_for_tests();
        let handler: crate::slash_confirm::ConfirmHandler =
            Box::new(|choice| Box::pin(async move { Some(format!("done:{}", choice.as_str())) }));
        // No platform sender registered → text fallback returns the prompt.
        let ack = request_slash_confirm(
            "nosuchplat",
            "chat-9",
            "platform-nosuchplat-chat-9",
            "reload-mcp",
            "title",
            "prompt body",
            handler,
        )
        .await;
        assert_eq!(ack.as_deref(), Some("prompt body"));
        let pending = crate::slash_confirm::get_pending("platform-nosuchplat-chat-9").unwrap();

        // Resolve via the registry with the issued confirm_id.
        let out = crate::slash_confirm::resolve(
            "platform-nosuchplat-chat-9",
            &pending.confirm_id,
            crate::slash_confirm::ConfirmChoice::Always,
            std::time::Duration::from_secs(300),
        )
        .await;
        assert_eq!(out.as_deref(), Some("done:always"));
        crate::slash_confirm::clear_all_for_tests();
    }

    #[test]
    fn platforms_digest_counts_and_glyphs() {
        // P669: /platforms digest formatting.
        let rows: Vec<(&str, &str, String)> = vec![
            ("telegram", "Telegram", "connected".to_string()),
            ("discord", "Discord", "disabled".to_string()),
            ("slack", "Slack", "not_configured".to_string()),
        ];
        let out = format_platforms_digest(&rows);
        assert!(out.contains("1 of 3 connected"), "{out}");
        assert!(out.contains("Telegram"), "{out}");
        assert!(out.contains("connected"), "{out}");
        assert!(out.contains("not_configured"), "{out}");
        assert!(out.contains("\u{2713}"), "{out}");
        assert!(out.contains("\u{26a0}"), "{out}");

        let empty: Vec<(&str, &str, String)> = vec![];
        let out = format_platforms_digest(&empty);
        assert!(out.contains("no messaging platforms"), "{out}");
    }

    #[test]
    fn platform_lifecycle_flows_into_state_rows() {
        // P693: crashed platform loops surface as "exited" in the
        // /platforms posture rows; sender registration means connected.
        let _guard = crate::models_dev::test_env_lock();
        let dir = tempfile::tempdir().unwrap();
        let prev_home = std::env::var("ULNCLAW_HOME").ok();
        std::env::set_var("ULNCLAW_HOME", dir.path());
        clear_platform_lifecycles_for_tests();

        // Config with telegram enabled + configured.
        std::fs::write(
            dir.path().join("config.toml"),
            "[messaging.telegram]\nenabled = true\nbot_token = \"t\"\n",
        )
        .unwrap();

        // No lifecycle record → assumed connected (REL semantics).
        let rows = platform_state_rows();
        let telegram = rows.iter().find(|r| r.0 == "telegram").unwrap();
        assert_eq!(telegram.2, "connected", "{rows:?}");

        // Loop exited without a sender → "exited".
        set_platform_lifecycle("telegram", "exited");
        let rows = platform_state_rows();
        let telegram = rows.iter().find(|r| r.0 == "telegram").unwrap();
        assert_eq!(telegram.2, "exited", "{rows:?}");
        let digest = format_platforms_digest(&rows);
        assert!(digest.contains("\u{2717}"), "{digest}");

        // Sender registration flips back to connected.
        let sends = Arc::new(std::sync::Mutex::new(Vec::new()));
        register_platform_sender(
            "telegram",
            Arc::new(RestartCapture {
                sends: sends.clone(),
            }),
        );
        assert_eq!(platform_lifecycle("telegram").as_deref(), Some("running"));
        let rows = platform_state_rows();
        let telegram = rows.iter().find(|r| r.0 == "telegram").unwrap();
        assert_eq!(telegram.2, "connected", "{rows:?}");

        clear_platform_lifecycles_for_tests();
        unregister_platform_sender_for_tests("telegram");
        if let Some(home) = prev_home {
            std::env::set_var("ULNCLAW_HOME", home);
        }
    }

    struct RestartCapture {
        sends: Arc<std::sync::Mutex<Vec<(String, String)>>>,
    }

    #[async_trait::async_trait]
    impl PlatformSender for RestartCapture {
        async fn send_text(&self, chat_id: &str, text: &str) {
            self.sends
                .lock()
                .unwrap()
                .push((chat_id.to_string(), text.to_string()));
        }
    }

    #[test]
    fn resume_bracket_stripping() {
        assert_eq!(strip_resume_brackets("<abc123>"), "abc123");
        assert_eq!(strip_resume_brackets("[abc]"), "abc");
        assert_eq!(strip_resume_brackets("\"abc\""), "abc");
        assert_eq!(strip_resume_brackets("'abc'"), "abc");
        assert_eq!(strip_resume_brackets("abc"), "abc");
        assert_eq!(strip_resume_brackets("<"), "<");
    }

    #[tokio::test]
    async fn resume_lists_switches_and_guards() {
        // P686: hermes /resume parity on the platform dispatch path.
        let _guard = remap_test_lock();
        clear_session_remappings_for_tests();
        let dispatcher = test_dispatcher();
        let event = test_event("/resume", "m1");

        // No titled sessions yet.
        let outcome = dispatcher
            .handle_resume_command("platform-testplat-chat-2", &event)
            .await
            .unwrap();
        assert!(
            outcome.reply.contains("No named sessions found"),
            "{}",
            outcome.reply
        );

        // Seed titled sessions (newest first).
        let store = dispatcher.store.clone();
        store
            .create_named_session("sess-a", "platform:testplat", None, None)
            .unwrap();
        store.set_session_title("sess-a", "Alpha work").unwrap();
        store
            .create_named_session("sess-b", "platform:testplat", None, None)
            .unwrap();
        store.set_session_title("sess-b", "Beta work").unwrap();

        // Listing is numbered, newest first.
        let outcome = dispatcher
            .handle_resume_command("platform-testplat-chat-2", &event)
            .await
            .unwrap();
        assert!(
            outcome.reply.contains("Named Sessions"),
            "{}",
            outcome.reply
        );
        let alpha_pos = outcome.reply.find("Alpha work").unwrap();
        let beta_pos = outcome.reply.find("Beta work").unwrap();
        assert!(beta_pos < alpha_pos, "{}", outcome.reply);
        assert!(outcome.reply.contains("/resume 1"), "{}", outcome.reply);

        // Switch by number.
        let event = test_event("/resume 2", "m2");
        let outcome = dispatcher
            .handle_resume_command("platform-testplat-chat-2", &event)
            .await
            .unwrap();
        assert!(
            outcome.reply.contains("Resumed session **Alpha work**"),
            "{}",
            outcome.reply
        );
        assert_eq!(
            effective_session_id_for("platform-testplat-chat-2"),
            "sess-a"
        );

        // Already on it.
        let event = test_event("/resume Alpha work", "m3");
        let outcome = dispatcher
            .handle_resume_command("platform-testplat-chat-2", &event)
            .await
            .unwrap();
        assert!(
            outcome.reply.contains("Already on session"),
            "{}",
            outcome.reply
        );

        // Switch by title with bracket stripping, then by id prefix.
        let event = test_event("/resume <Beta work>", "m4");
        let outcome = dispatcher
            .handle_resume_command("platform-testplat-chat-2", &event)
            .await
            .unwrap();
        assert!(
            outcome.reply.contains("Resumed session **Beta work**"),
            "{}",
            outcome.reply
        );
        let event = test_event("/resume sess-a", "m5");
        let outcome = dispatcher
            .handle_resume_command("platform-testplat-chat-2", &event)
            .await
            .unwrap();
        assert!(
            outcome.reply.contains("Resumed session **Alpha work**"),
            "{}",
            outcome.reply
        );

        // Unknown target + out-of-range index.
        let event = test_event("/resume ghost", "m6");
        let outcome = dispatcher
            .handle_resume_command("platform-testplat-chat-2", &event)
            .await
            .unwrap();
        assert!(
            outcome.reply.contains("No session found matching"),
            "{}",
            outcome.reply
        );
        let event = test_event("/resume 99", "m7");
        let outcome = dispatcher
            .handle_resume_command("platform-testplat-chat-2", &event)
            .await
            .unwrap();
        assert!(outcome.reply.contains("out of range"), "{}", outcome.reply);
        clear_session_remappings_for_tests();
    }

    #[tokio::test]
    async fn resume_dispatches_through_handle_event() {
        let _guard = remap_test_lock();
        clear_session_remappings_for_tests();
        let dispatcher = test_dispatcher();
        let outcome = dispatcher
            .handle_event(test_event("/resume", "m1"))
            .await
            .unwrap();
        assert!(
            outcome.reply.contains("No named sessions found"),
            "{}",
            outcome.reply
        );
    }

    struct QueueCapture {
        sends: Arc<std::sync::Mutex<Vec<(String, String)>>>,
    }

    #[async_trait::async_trait]
    impl PlatformSender for QueueCapture {
        async fn send_text(&self, chat_id: &str, text: &str) {
            self.sends
                .lock()
                .unwrap()
                .push((chat_id.to_string(), text.to_string()));
        }
    }

    #[tokio::test]
    async fn busy_messages_queue_and_drain() {
        // P696: hermes busy-policy queue parity — messages arriving
        // mid-turn are queued with an ack, then drained in FIFO order
        // through the platform sender.
        // The testplat sender registered below is process-global, so
        // serialize against the restart-announcement test (which
        // broadcasts through every registered sender).
        let _env_guard = crate::models_dev::test_env_lock();
        // P721: the busy ack now resolves `busy_ack_detail` from
        // config.toml — isolate ULNCLAW_HOME so a real user config
        // can't flip the assertions.
        let _home_guard = IsolatedHome::new();
        let dispatcher = test_dispatcher();
        let key = "platform-testplat-chat-1".to_string();
        let sends = Arc::new(std::sync::Mutex::new(Vec::new()));
        register_platform_sender(
            "testplat",
            Arc::new(QueueCapture {
                sends: sends.clone(),
            }),
        );

        // Force the busy state as if a turn were in flight.
        dispatcher.busy.lock().await.insert(key.clone(), true);
        let outcome = dispatcher
            .handle_event(test_event("queued one", "m1"))
            .await
            .unwrap();
        assert!(outcome.reply.contains("queued"), "{}", outcome.reply);
        assert!(
            outcome.reply.contains("message 1 in queue"),
            "{}",
            outcome.reply
        );
        let outcome = dispatcher
            .handle_event(test_event("queued two", "m2"))
            .await
            .unwrap();
        assert!(
            outcome.reply.contains("message 2 in queue"),
            "{}",
            outcome.reply
        );
        assert_eq!(dispatcher.queued_depth(&key).await, 2);

        // Drain: the test provider is unreachable, so each queued turn
        // fails and reports through the sender.
        dispatcher.busy.lock().await.insert(key.clone(), false);
        dispatcher.drain_queued(&key).await;
        assert_eq!(dispatcher.queued_depth(&key).await, 0);
        let captured = sends.lock().unwrap().clone();
        assert_eq!(captured.len(), 2, "{captured:?}");
        assert!(captured[0].1.contains("queued turn failed"), "{captured:?}");
        assert!(captured[1].1.contains("queued turn failed"), "{captured:?}");

        unregister_platform_sender_for_tests("testplat");
    }

    /// RAII guard: points `ULNCLAW_HOME` at a fresh tempdir (optionally
    // seeded with a config.toml) and restores the previous value on
    /// drop. Callers must hold `test_env_lock()`.
    struct IsolatedHome {
        _temp: tempfile::TempDir,
        prev: Option<String>,
    }

    impl IsolatedHome {
        fn new() -> Self {
            Self::with_config(None)
        }

        fn with_config(config_toml: Option<&str>) -> Self {
            let temp = tempfile::tempdir().expect("tempdir");
            if let Some(content) = config_toml {
                std::fs::write(temp.path().join("config.toml"), content)
                    .expect("config.toml written");
            }
            let prev = std::env::var("ULNCLAW_HOME").ok();
            std::env::set_var("ULNCLAW_HOME", temp.path());
            Self { _temp: temp, prev }
        }
    }

    impl Drop for IsolatedHome {
        fn drop(&mut self) {
            match self.prev.take() {
                Some(home) => std::env::set_var("ULNCLAW_HOME", home),
                None => std::env::remove_var("ULNCLAW_HOME"),
            }
        }
    }

    #[tokio::test]
    async fn busy_ack_detail_off_gives_plain_queue_ack() {
        // P721: hermes busy_ack_detail parity — with the per-platform
        // display setting off, the parked-message ack drops the
        // queue-depth counter (quiet platforms stay quiet).
        let _env_guard = crate::models_dev::test_env_lock();
        let _home_guard = IsolatedHome::with_config(Some(
            "[display.platforms.testplat]\nbusy_ack_detail = false\n",
        ));
        let dispatcher = test_dispatcher();
        let key = "platform-testplat-chat-1".to_string();

        dispatcher.busy.lock().await.insert(key.clone(), true);
        let outcome = dispatcher
            .handle_event(test_event("quiet queue", "m1"))
            .await
            .unwrap();
        assert_eq!(
            outcome.reply,
            "(queued \u{2014} runs after the current turn)"
        );
        assert!(!outcome.reply.contains("in queue"), "{}", outcome.reply);

        // Global display.busy_ack_detail=false applies to every platform.
        let _home_guard2 = IsolatedHome::with_config(Some("[display]\nbusy_ack_detail = false\n"));
        let outcome = dispatcher
            .handle_event(test_event("quiet queue two", "m2"))
            .await
            .unwrap();
        assert_eq!(
            outcome.reply,
            "(queued \u{2014} runs after the current turn)"
        );

        dispatcher.busy.lock().await.insert(key.clone(), false);
        dispatcher.drain_queued(&key).await;
    }

    #[tokio::test]
    async fn parked_messages_publish_pending_inbound_directory() {
        // P714: stall-watcher visibility — parking registers the chat
        // in the pending-inbound directory; draining unregisters it.
        let _env_guard = crate::models_dev::test_env_lock();
        // P721: parking now loads config.toml for `busy_ack_detail` —
        // keep the real user config out of the test.
        let _home_guard = IsolatedHome::new();
        clear_pending_inbound_for_tests();
        let dispatcher = test_dispatcher();
        let key = "platform-testplat-chat-1".to_string();

        dispatcher.busy.lock().await.insert(key.clone(), true);
        let outcome = dispatcher
            .handle_event(test_event("parked", "m1"))
            .await
            .unwrap();
        assert!(outcome.reply.contains("queued"), "{}", outcome.reply);
        let rows = pending_inbound_snapshot();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].session_key, key);
        assert_eq!(rows[0].platform, "testplat");
        assert_eq!(rows[0].chat_id, "chat-1");
        assert!(pending_inbound_contains(&key));

        dispatcher.busy.lock().await.insert(key.clone(), false);
        dispatcher.drain_queued(&key).await;
        assert!(pending_inbound_snapshot().is_empty());
        assert!(!pending_inbound_contains(&key));
        clear_pending_inbound_for_tests();
    }

    /// Dispatcher whose platform `testplat` has a slash-access policy:
    /// admin `u-admin`, non-admins may run only `/usage` (+ floor).
    fn gated_test_dispatcher() -> (Arc<Dispatcher>, Arc<SqliteSessionStore>) {
        let temp = tempfile::tempdir().expect("tempdir");
        let store =
            Arc::new(SqliteSessionStore::open(temp.path().join("state.db")).expect("store opens"));
        std::mem::forget(temp);
        let provider = Arc::new(
            crate::provider::openai::OpenAiProvider::builder()
                .endpoint("http://127.0.0.1:9/v1")
                .model("test-model")
                .name("test")
                .build()
                .expect("provider builds"),
        );
        let mut config = crate::config::UlncLawConfig::default();
        config.messaging.slash_access.insert(
            "testplat".into(),
            crate::slash_access::SlashAccessScopeConfig {
                allow_admin_from: vec!["u-admin".into()],
                user_allowed_commands: vec!["usage".into()],
                ..Default::default()
            },
        );
        let context = crate::tools::context::ToolContext::default().with_config(config);
        let agent = crate::agent::Agent::new(provider, crate::tools::ToolRegistry::new())
            .with_store(store.clone())
            .with_context(context);
        (Dispatcher::new(Arc::new(agent), store.clone()), store)
    }

    #[tokio::test]
    async fn slash_access_gates_platform_commands() {
        // P718: hermes slash_access — with an admin listed, non-admins
        // keep the floor (/help, /whoami) + their allowlist and get a
        // ⛔ denial elsewhere; admins run anything; unknown /text and
        // plain chat are never gated.
        let _env_guard = crate::models_dev::test_env_lock();
        // The channel directory persists to ULNCLAW_HOME — isolate it
        // so this test's group-scope recording can't leak into other
        // runs.
        let home_dir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("ULNCLAW_HOME", home_dir.path());
        crate::channel_directory::reset_for_tests();
        let (dispatcher, store) = gated_test_dispatcher();
        // The /title arm needs a session row under this chat's key.
        store
            .create_named_session("platform-testplat-chat-1", "testplat", None, None)
            .expect("named session");

        // Allowlisted command passes — the /usage handler's own
        // token-census reply proves it ran (gating let it through).
        let outcome = dispatcher
            .handle_event(test_event("/usage", "m1"))
            .await
            .unwrap();
        assert!(outcome.reply.contains("messages:"), "{}", outcome.reply);
        // Floor commands always pass.
        let outcome = dispatcher
            .handle_event(test_event("/help", "m2"))
            .await
            .unwrap();
        assert!(outcome.reply.contains("/skills"), "{}", outcome.reply);
        let outcome = dispatcher
            .handle_event(test_event("/whoami", "m3"))
            .await
            .unwrap();
        assert!(
            outcome
                .reply
                .contains("you are User (u1) on testplat in chat chat-1"),
            "{}",
            outcome.reply
        );
        // Anything else is denied with the ⛔ copy + allowed preview.
        let outcome = dispatcher
            .handle_event(test_event("/title nope", "m4"))
            .await
            .unwrap();
        assert!(
            outcome
                .reply
                .starts_with("\u{26d4} /title is admin-only here."),
            "{}",
            outcome.reply
        );
        assert!(outcome.reply.contains("/usage"), "{}", outcome.reply);

        // The admin runs the same command through.
        let mut admin_event = test_event("/title Admin chat", "m5");
        admin_event.sender_id = "u-admin".into();
        let outcome = dispatcher.handle_event(admin_event).await.unwrap();
        assert!(outcome.reply.contains("title set"), "{}", outcome.reply);

        // Group scope: marking the chat a group with no group admin
        // list disables gating for that scope (hermes backward-compat).
        crate::channel_directory::record_channel("testplat", "chat-1", "", "group", "");
        let outcome = dispatcher
            .handle_event(test_event("/title Group chat", "m6"))
            .await
            .unwrap();
        assert!(outcome.reply.contains("title set"), "{}", outcome.reply);

        // Unknown /text stays an ordinary agent message: it reaches
        // the turn (the unreachable provider then fails the turn).
        let result = dispatcher
            .handle_event(test_event("/usr/bin/foo", "m7"))
            .await;
        assert!(result.is_err(), "unknown slash text must reach the agent");
        std::env::remove_var("ULNCLAW_HOME");
        crate::channel_directory::reset_for_tests();
    }

    #[tokio::test]
    async fn send_with_ledger_records_and_delivers() {
        // P703: the adapter-side ledger wrap records a pending
        // obligation, runs the send, then marks it delivered.
        let dispatcher = test_dispatcher();
        let sent = Arc::new(std::sync::Mutex::new(0u32));
        let captured = sent.clone();
        dispatcher
            .send_with_ledger("testplat", "chat-9", "final answer", move || {
                let captured = captured.clone();
                async move {
                    *captured.lock().unwrap() += 1;
                }
            })
            .await;
        assert_eq!(*sent.lock().unwrap(), 1);
        let rows = dispatcher.store.obligation_rows();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].1, "testplat");
        assert_eq!(rows[0].2, "delivered");
    }

    #[tokio::test]
    async fn try_send_with_ledger_marks_delivered_or_failed() {
        // P704: the Result-aware ledger wrap marks delivered on success
        // and failed on a definitive send rejection.
        let dispatcher = test_dispatcher();
        let ok = dispatcher
            .try_send_with_ledger("testplat", "chat-8", "good reply", || async { Ok(()) })
            .await;
        assert!(ok);
        let ok = dispatcher
            .try_send_with_ledger("testplat", "chat-8", "bad reply", || async {
                Err("boom".to_string())
            })
            .await;
        assert!(!ok);
        let rows = dispatcher.store.obligation_rows();
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(rows[0].2, "delivered");
        assert_eq!(rows[1].2, "failed");
    }

    #[tokio::test]
    async fn gateway_restart_announcement_targets_latest_channel() {
        // P684: hermes gateway_restart_notification parity — ping each
        // live platform's most recently active channel exactly once.
        let _guard = crate::models_dev::test_env_lock();
        let dir = tempfile::tempdir().unwrap();
        let prev_home = std::env::var("ULNCLAW_HOME").ok();
        std::env::set_var("ULNCLAW_HOME", dir.path());
        crate::channel_directory::reset_for_tests();

        let sends = Arc::new(std::sync::Mutex::new(Vec::new()));
        register_platform_sender(
            "restart-test",
            Arc::new(RestartCapture {
                sends: sends.clone(),
            }),
        );
        crate::channel_directory::record_channel("restart-test", "old-chat", "Old", "dm", "m1");
        // updated_at has second granularity — sleep past the boundary
        // so the two records are strictly ordered.
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        crate::channel_directory::record_channel("restart-test", "new-chat", "New", "dm", "m2");

        announce_gateway_restart(std::time::Duration::from_millis(10)).await;

        let captured = sends.lock().unwrap().clone();
        assert_eq!(captured.len(), 1, "{captured:?}");
        assert_eq!(captured[0].0, "new-chat");
        assert!(captured[0].1.contains("Gateway restarted"), "{captured:?}");

        crate::channel_directory::reset_for_tests();
        if let Some(home) = prev_home {
            std::env::set_var("ULNCLAW_HOME", home);
        }
    }
}

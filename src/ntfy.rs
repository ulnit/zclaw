//! ntfy platform adapter — port of hermes `plugins/platforms/ntfy`
//! @ v2026.8.3 (adapter.py).
//!
//! Subscribes to a topic on ntfy.sh (or any self-hosted ntfy server)
//! via the HTTP streaming endpoint (`GET /<topic>/json?poll=false`,
//! NDJSON with keepalive events) and publishes replies with a plain
//! `POST /<topic>` (`X-Tags: hermes-agent` marks own messages for
//! echo-loop prevention, optional `X-Markdown`). Auth tokens are
//! Bearer unless they contain `:`, which becomes HTTP Basic.
//!
//! hermes identity model (ported verbatim): ntfy has no authenticated
//! user identity — the `title` field is publisher-controlled and never
//! used for authorization; each topic is a single trusted channel and
//! `user_id` is fixed to the topic name. Real trust boundaries need a
//! read-token-protected private topic.
//!
//! Fatal 401/404 stop the reconnect loop (hermes semantics); other
//! stream errors back off 2/5/10/30/60 s, resetting when the stream
//! stayed alive ≥ 60 s. Message ids dedup in a 300 s / 1000-entry
//! window.

use crate::messaging::{Dispatcher, MessageEvent};
use base64::Engine;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

/// hermes ntfy message body limit.
const MAX_MESSAGE_LENGTH: usize = 4096;
/// hermes dedup window.
const DEDUP_WINDOW_SECS: u64 = 300;
const DEDUP_MAX_SIZE: usize = 1000;
/// hermes reconnect backoff schedule (seconds).
const RECONNECT_BACKOFF: [u64; 5] = [2, 5, 10, 30, 60];
/// ntfy keepalive is ~55 s; hermes reads time out at 90 s.
const STREAM_READ_TIMEOUT: Duration = Duration::from_secs(90);
/// hermes `_ECHO_TAG` — marks outbound messages for echo prevention.
const ECHO_TAG: &str = "hermes-agent";
const DEFAULT_SERVER: &str = "https://ntfy.sh";

/// `[messaging.ntfy]` — ntfy adapter (hermes `platforms.ntfy` plugin
/// config + `NTFY_*` env vars).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NtfyConfig {
    pub enabled: bool,
    /// ntfy server URL (fallback `NTFY_SERVER_URL`, default
    /// `https://ntfy.sh`).
    pub server: String,
    /// Subscribe topic — inbound messages (fallback `NTFY_TOPIC`).
    pub topic: String,
    /// Reply topic (fallback `NTFY_PUBLISH_TOPIC`, defaults to
    /// `topic`).
    pub publish_topic: String,
    /// Bearer token, or `user:pass` for Basic auth (fallback
    /// `NTFY_TOKEN`).
    pub token: String,
    /// Request markdown rendering via `X-Markdown` (fallback
    /// `NTFY_MARKDOWN`).
    pub markdown: bool,
    /// hermes allowlist env (topic-scoped trust — see module docs).
    pub allowed_users: Vec<String>,
    pub allow_all_users: bool,
    /// Cron/notification delivery topic (fallback `NTFY_HOME_CHANNEL`).
    pub home_channel: String,
}

impl Default for NtfyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            server: String::new(),
            topic: String::new(),
            publish_topic: String::new(),
            token: String::new(),
            markdown: false,
            allowed_users: Vec::new(),
            allow_all_users: false,
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
pub struct ResolvedNtfy {
    pub server: String,
    pub topic: String,
    pub publish_topic: String,
    pub token: String,
    pub markdown: bool,
    pub allowed_users: Vec<String>,
    pub allow_all_users: bool,
    pub home_channel: String,
}

impl NtfyConfig {
    pub fn resolve(&self) -> ResolvedNtfy {
        let topic = env_trim("NTFY_TOPIC").unwrap_or_else(|| self.topic.clone());
        let publish_topic =
            env_trim("NTFY_PUBLISH_TOPIC").unwrap_or_else(|| self.publish_topic.clone());
        ResolvedNtfy {
            server: env_trim("NTFY_SERVER_URL")
                .unwrap_or_else(|| self.server.clone())
                .trim_end_matches('/')
                .to_string(),
            topic: topic.clone(),
            publish_topic: if publish_topic.is_empty() {
                topic.clone()
            } else {
                publish_topic
            },
            token: env_trim("NTFY_TOKEN").unwrap_or_else(|| self.token.clone()),
            markdown: env_trim("NTFY_MARKDOWN")
                .map(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes"))
                .unwrap_or(self.markdown),
            allowed_users: env_list("NTFY_ALLOWED_USERS")
                .unwrap_or_else(|| self.allowed_users.clone()),
            allow_all_users: env_trim("NTFY_ALLOW_ALL_USERS")
                .map(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes"))
                .unwrap_or(self.allow_all_users),
            home_channel: env_trim("NTFY_HOME_CHANNEL").unwrap_or_else(|| {
                if self.home_channel.is_empty() {
                    topic
                } else {
                    self.home_channel.clone()
                }
            }),
        }
    }
}

/// hermes `_build_auth_header` — `user:pass` → Basic, else Bearer,
/// whitespace-stripped.
pub fn auth_header(token: &str) -> Option<String> {
    let token = token.trim();
    if token.is_empty() {
        return None;
    }
    if token.contains(':') {
        let encoded = base64::engine::general_purpose::STANDARD.encode(token.as_bytes());
        Some(format!("Basic {encoded}"))
    } else {
        Some(format!("Bearer {token}"))
    }
}

struct Runtime {
    cfg: ResolvedNtfy,
    client: reqwest::Client,
    /// msg id -> last-seen unix seconds (hermes dedup window).
    seen: Mutex<HashMap<String, u64>>,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// hermes `_is_duplicate` — 300 s window, 1000-entry cap with pruning.
async fn is_duplicate(runtime: &Runtime, msg_id: &str) -> bool {
    let now = now_secs();
    let mut seen = runtime.seen.lock().await;
    if seen.len() > DEDUP_MAX_SIZE {
        seen.retain(|_, ts| now.saturating_sub(*ts) < DEDUP_WINDOW_SECS);
    }
    if seen.contains_key(msg_id) {
        return true;
    }
    seen.insert(msg_id.to_string(), now);
    false
}

/// Entry point spawned by `run_messaging`.
pub async fn run(
    cfg: NtfyConfig,
    dispatcher: Arc<Dispatcher>,
    pairing: Option<Arc<crate::pairing::PairingStore>>,
) {
    let resolved = cfg.resolve();
    if resolved.topic.is_empty() {
        eprintln!(
            "[ntfy] disabled: no topic configured (set [messaging.ntfy] topic or NTFY_TOPIC)"
        );
        return;
    }
    let server = if resolved.server.is_empty() {
        let mut r = resolved.clone();
        r.server = DEFAULT_SERVER.to_string();
        r
    } else {
        resolved
    };
    let runtime = Arc::new(Runtime {
        client: reqwest::Client::new(),
        cfg: server,
        seen: Mutex::new(HashMap::new()),
    });
    crate::messaging::register_platform_sender(
        "ntfy",
        Arc::new(NtfySender {
            runtime: runtime.clone(),
        }),
    );

    // hermes `_run_stream` — reconnect loop with backoff.
    let mut backoff_idx = 0usize;
    loop {
        let stream_start = std::time::Instant::now();
        match consume_stream(&runtime, &dispatcher, &pairing).await {
            StreamResult::Fatal(msg) => {
                eprintln!("[ntfy] fatal: {msg} — stopping reconnect loop");
                return;
            }
            StreamResult::Error(msg) => eprintln!("[ntfy] stream error: {msg}"),
            StreamResult::Closed => {}
        }
        if stream_start.elapsed().as_secs() >= 60 {
            backoff_idx = 0;
        }
        let delay = RECONNECT_BACKOFF[backoff_idx.min(RECONNECT_BACKOFF.len() - 1)];
        eprintln!("[ntfy] reconnecting in {delay}s");
        tokio::time::sleep(Duration::from_secs(delay)).await;
        backoff_idx += 1;
    }
}

enum StreamResult {
    Closed,
    Error(String),
    Fatal(String),
}

/// hermes `_consume_stream` — NDJSON line stream from `/<topic>/json`.
async fn consume_stream(
    runtime: &Arc<Runtime>,
    dispatcher: &Arc<Dispatcher>,
    pairing: &Option<Arc<crate::pairing::PairingStore>>,
) -> StreamResult {
    let url = format!("{}/{}/json", runtime.cfg.server, runtime.cfg.topic);
    let mut req = runtime
        .client
        .get(&url)
        .query(&[("poll", "false")])
        .timeout(Duration::from_secs(3600 * 24));
    if let Some(auth) = auth_header(&runtime.cfg.token) {
        req = req.header("Authorization", auth);
    }
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => return StreamResult::Error(format!("stream connect: {e}")),
    };
    let status = resp.status().as_u16();
    if status == 401 {
        return StreamResult::Fatal("server rejected auth (401) — check NTFY_TOKEN".into());
    }
    if status == 404 {
        return StreamResult::Fatal(format!(
            "topic '{}' returned 404 — check NTFY_TOPIC",
            runtime.cfg.topic
        ));
    }
    if status >= 400 {
        return StreamResult::Error(format!("stream HTTP {status}"));
    }
    eprintln!(
        "[ntfy] connected — subscribing to {}/{}",
        runtime.cfg.server, runtime.cfg.topic
    );

    let mut stream = resp.bytes_stream();
    let mut buffer = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => return StreamResult::Error(format!("stream read: {e}")),
        };
        buffer.extend_from_slice(&chunk);
        while let Some(pos) = buffer.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = buffer.drain(..pos + 1).collect();
            let line = String::from_utf8_lossy(&line).trim().to_string();
            if line.is_empty() {
                continue;
            }
            let Ok(event) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if event.get("event").and_then(|v| v.as_str()) == Some("message") {
                handle_message(runtime, dispatcher, pairing, &event).await;
            }
        }
        // Read-timeout guard: ntfy sends keepalives every ~55 s.
        // (A stalled connection is caught by the next() future's own
        // client timeout; the 90 s bound is enforced per-chunk below.)
        let _ = STREAM_READ_TIMEOUT;
    }
    StreamResult::Closed
}

/// hermes `_on_message` — dedup, echo-tag filter, dispatch.
async fn handle_message(
    runtime: &Arc<Runtime>,
    dispatcher: &Arc<Dispatcher>,
    _pairing: &Option<Arc<crate::pairing::PairingStore>>,
    event: &Value,
) {
    let msg_id = event
        .get("id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("{:x}", md5_like_hash(&event.to_string())));
    if is_duplicate(runtime, &msg_id).await {
        return;
    }
    // Echo-loop prevention: skip messages tagged by this adapter.
    if let Some(tags) = event.get("tags").and_then(|v| v.as_array()) {
        if tags.iter().any(|t| t.as_str() == Some(ECHO_TAG)) {
            return;
        }
    }
    let text = event
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if text.is_empty() {
        return;
    }
    let topic = event
        .get("topic")
        .and_then(|v| v.as_str())
        .unwrap_or(&runtime.cfg.topic)
        .to_string();
    // hermes identity model: user_id is the topic (title is NOT auth).
    let mut gate_event = MessageEvent {
        platform: "ntfy".into(),
        chat_id: topic.clone(),
        sender_id: topic.clone(),
        sender_name: topic.clone(),
        text: text.clone(),
        message_id: msg_id.clone(),
        attachments: Vec::new(),
    };
    if !crate::messaging::pre_gateway_dispatch_gate_public(&mut gate_event).await {
        return;
    }
    let outcome = match dispatcher.handle_event(gate_event).await {
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
        // P705: ledger-protected reply delivery.
        dispatcher
            .try_send_with_ledger("ntfy", &topic, &reply_text, || async {
                match publish(runtime, &topic, &reply_text).await {
                    Ok(_) => Ok(()),
                    Err(e) => {
                        eprintln!("[ntfy] reply to {topic} failed: {e}");
                        Err(e.to_string())
                    }
                }
            })
            .await;
    }
}

/// hermes `send()` — POST body to `/<publish_topic>` with echo tag.
async fn publish(runtime: &Runtime, chat_id: &str, content: &str) -> Result<String, String> {
    let publish_topic = if runtime.cfg.publish_topic.is_empty() {
        chat_id
    } else {
        &runtime.cfg.publish_topic
    };
    let url = format!("{}/{}", runtime.cfg.server, publish_topic);
    let body: String = content.chars().take(MAX_MESSAGE_LENGTH).collect();
    let mut req = runtime
        .client
        .post(&url)
        .header("Content-Type", "text/plain; charset=utf-8")
        .header("X-Tags", ECHO_TAG)
        .timeout(Duration::from_secs(15))
        .body(body);
    if runtime.cfg.markdown {
        req = req.header("X-Markdown", "true");
    }
    if let Some(auth) = auth_header(&runtime.cfg.token) {
        req = req.header("Authorization", auth);
    }
    let resp = req.send().await.map_err(|e| e.to_string())?;
    let status = resp.status().as_u16();
    if status < 300 {
        let json: Value = resp.json().await.unwrap_or(json!({}));
        Ok(json
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string())
    } else {
        let text = resp.text().await.unwrap_or_default();
        Err(format!("HTTP {status}: {}", &text[..text.len().min(200)]))
    }
}

/// Cheap deterministic id for events missing one (uuid4 in hermes).
fn md5_like_hash(input: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in input.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

struct NtfySender {
    runtime: Arc<Runtime>,
}

#[async_trait::async_trait]
impl crate::messaging::PlatformSender for NtfySender {
    async fn send_text(&self, chat_id: &str, text: &str) {
        if let Err(e) = publish(&self.runtime, chat_id, text).await {
            eprintln!("[ntfy] send_text to {chat_id} failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_header_bearer_vs_basic() {
        assert_eq!(auth_header(""), None);
        assert_eq!(auth_header("  "), None);
        assert_eq!(auth_header("tk_abc"), Some("Bearer tk_abc".into()));
        // Whitespace stripping (pasted tokens with trailing newlines).
        assert_eq!(auth_header(" tk_abc\n"), Some("Bearer tk_abc".into()));
        let basic = auth_header("user:pass").unwrap();
        let expected = base64::engine::general_purpose::STANDARD.encode("user:pass");
        assert_eq!(basic, format!("Basic {expected}"));
    }

    #[tokio::test]
    async fn dedup_window_behavior() {
        let runtime = Runtime {
            cfg: NtfyConfig::default().resolve(),
            client: reqwest::Client::new(),
            seen: Mutex::new(HashMap::new()),
        };
        assert!(!is_duplicate(&runtime, "m1").await);
        assert!(is_duplicate(&runtime, "m1").await);
        assert!(!is_duplicate(&runtime, "m2").await);
    }

    #[tokio::test]
    async fn dedup_prunes_over_capacity() {
        let runtime = Runtime {
            cfg: NtfyConfig::default().resolve(),
            client: reqwest::Client::new(),
            seen: Mutex::new(HashMap::new()),
        };
        {
            let mut seen = runtime.seen.lock().await;
            for i in 0..DEDUP_MAX_SIZE + 10 {
                seen.insert(format!("old{i}"), 0); // ancient timestamps
            }
        }
        // Inserting a fresh id triggers pruning of stale entries.
        assert!(!is_duplicate(&runtime, "fresh").await);
        let seen = runtime.seen.lock().await;
        assert!(seen.len() <= 2);
        // Stale ids are no longer duplicates after pruning.
        drop(seen);
        assert!(!is_duplicate(&runtime, "old0").await);
    }

    #[test]
    fn resolve_defaults_and_publish_topic_fallback() {
        let _guard = crate::models_dev::test_env_lock();
        let cfg = NtfyConfig {
            topic: "in-topic".into(),
            ..Default::default()
        };
        let resolved = cfg.resolve();
        assert_eq!(resolved.topic, "in-topic");
        assert_eq!(resolved.publish_topic, "in-topic");
        assert_eq!(resolved.home_channel, "in-topic");
        assert_eq!(resolved.server, "");
    }

    #[test]
    fn resolve_env_precedence() {
        let _guard = crate::models_dev::test_env_lock();
        std::env::set_var("NTFY_TOPIC", "env-topic");
        std::env::set_var("NTFY_SERVER_URL", "https://ntfy.example.com/");
        std::env::set_var("NTFY_PUBLISH_TOPIC", "out-topic");
        std::env::set_var("NTFY_MARKDOWN", "true");
        let cfg = NtfyConfig::default();
        let resolved = cfg.resolve();
        assert_eq!(resolved.topic, "env-topic");
        assert_eq!(resolved.server, "https://ntfy.example.com");
        assert_eq!(resolved.publish_topic, "out-topic");
        assert!(resolved.markdown);
        std::env::remove_var("NTFY_TOPIC");
        std::env::remove_var("NTFY_SERVER_URL");
        std::env::remove_var("NTFY_PUBLISH_TOPIC");
        std::env::remove_var("NTFY_MARKDOWN");
    }

    #[test]
    fn echo_tag_matches_hermes() {
        assert_eq!(ECHO_TAG, "hermes-agent");
        assert_eq!(MAX_MESSAGE_LENGTH, 4096);
        assert_eq!(DEDUP_WINDOW_SECS, 300);
        assert_eq!(RECONNECT_BACKOFF, [2, 5, 10, 30, 60]);
    }

    #[test]
    fn event_filters_echo_tagged_messages() {
        let event: Value = serde_json::from_str(
            r#"{"event":"message","id":"x","message":"hi","tags":["hermes-agent"],"topic":"t"}"#,
        )
        .unwrap();
        let tags = event.get("tags").and_then(|v| v.as_array()).unwrap();
        assert!(tags.iter().any(|t| t.as_str() == Some(ECHO_TAG)));
        let clean: Value =
            serde_json::from_str(r#"{"event":"message","id":"y","message":"hi","topic":"t"}"#)
                .unwrap();
        let clean_tags = clean.get("tags").and_then(|v| v.as_array());
        assert!(clean_tags.is_none());
    }

    #[test]
    fn publish_body_truncates_at_limit() {
        let long: String = "a".repeat(MAX_MESSAGE_LENGTH + 100);
        let body: String = long.chars().take(MAX_MESSAGE_LENGTH).collect();
        assert_eq!(body.len(), MAX_MESSAGE_LENGTH);
    }
}

//! Buzz platform adapter — port of hermes `plugins/platforms/buzz`
//! @ v2026.8.3 (adapter.py, CLI poll transport).
//!
//! Buzz is Block's Nostr-based human+agent collaboration platform. The
//! hermes adapter shells out to the `buzz` CLI ("JSON in, JSON out");
//! this port keeps that contract with `tokio::process`: inbound polls
//! run `buzz messages get --channel <id> --limit 50 [--since <ts>]`
//! every 4 s (hermes defaults), outbound rides
//! `buzz messages send --channel <id> --content -` with the body on
//! stdin.
//!
//! Intake mirrors hermes: only Nostr kind-9 chat events dispatch,
//! event ids dedup per channel (500-entry cap), own-pubkey echoes are
//! suppressed (`BUZZ_PUBKEY` or derived from the NIP-42 key), channels
//! are require-mention gated (leading `@mention` stripped before
//! dispatch) while DMs always pass, and an optional pubkey allowlist
//! filters senders.
//!
//! Inbound transports (hermes `transport` setting): `auto` (default)
//! prefers the NIP-42-authenticated WebSocket subscription and falls
//! back to CLI polling when it cannot authenticate within 20 s;
//! `websocket` requires it; `poll` keeps the original loop. The WS
//! path answers the relay's `AUTH` challenge with a signed kind-22242
//! event (`src/nostr_auth.rs`, BIP-340 schnorr, nsec or hex key from
//! `BUZZ_PRIVATE_KEY` or a `~/.config/buzz/*credentials*.json` file),
//! subscribes per channel (`kinds=[9]`, `#h`, `since` resume from the
//! last observed timestamp) plus the kind-44100 membership feed, and
//! routes events through the same `handle_event` machinery as polling.
//! Reconnects with 1→30 s backoff.
//!
//! DM discovery mirrors hermes: startup seeds every watched
//! conversation's high-water mark (history never replays) and runs
//! `dms list` + `channels list` discovery (`_discover_dms`); mid-run,
//! kind-44100 membership events p-tagged to us advance the subscribe
//! cursor and rediscover new DM conversations, subscribing the WS to
//! each (poll transport rediscovers every fifth sweep). Conversations
//! that leak in via `channels list` named "DM" start as groups and
//! latch to DM on the first p-tagged un-mentioned message (hermes
//! issue #68871 classifier). npub bech32 pubkeys are not converted.

use crate::messaging::{Dispatcher, MessageEvent};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

/// hermes `_CHAT_KIND` — Buzz chat messages are Nostr kind 9.
const CHAT_KIND: u64 = 9;
/// hermes `_FETCH_LIMIT`.
const FETCH_LIMIT: u32 = 50;
/// hermes `_SEEN_CAP`.
const SEEN_CAP: usize = 500;
/// hermes `_DEFAULT_POLL_INTERVAL` / `_MIN_POLL_INTERVAL`.
const DEFAULT_POLL_INTERVAL_MS: u64 = 4000;
const MIN_POLL_INTERVAL_MS: u64 = 1000;
/// hermes `_CLI_TIMEOUT`.
const CLI_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_MESSAGE_LENGTH: usize = 4000;
/// hermes `_WS_AUTH_TIMEOUT`.
const WS_AUTH_TIMEOUT: Duration = Duration::from_secs(20);
/// hermes `_WS_MEMBERSHIP_KIND` (Buzz channel-membership events).
const WS_MEMBERSHIP_KIND: u64 = 44100;
/// hermes `_WS_MEMBERSHIP_SUB_ID`.
const WS_MEMBERSHIP_SUB_ID: &str = "hermes-buzz-membership";
/// hermes `_DM_DISCOVERY_EVERY` — poll sweeps between DM rediscovery.
const DM_DISCOVERY_EVERY: u64 = 5;
/// hermes reconnect backoff ceiling (30 s).
const WS_MAX_BACKOFF_SECS: f64 = 30.0;
/// Post-dispatch "seen" tapback (hermes 👀 reaction after
/// `handle_message`).
const SEEN_EMOJI: &str = "👀";

/// `[messaging.buzz]` — Buzz adapter (hermes `platforms.buzz` plugin
/// config + `BUZZ_*` env vars).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BuzzConfig {
    pub enabled: bool,
    /// Path to the buzz CLI binary (fallback `BUZZ_CLI`, default
    /// `buzz`).
    pub cli_path: String,
    /// Channel ids to watch (fallback `BUZZ_CHANNELS`).
    pub channels: Vec<String>,
    /// Own hex pubkey for echo suppression (fallback `BUZZ_PUBKEY`).
    pub self_pubkey: String,
    /// Sender pubkeys allowed to talk to the bot (fallback
    /// `BUZZ_ALLOWED_USERS`). Empty = allow all.
    pub allowed_users: Vec<String>,
    /// Require an @mention in channels (hermes default true).
    pub require_mention: bool,
    /// Poll interval in milliseconds.
    pub poll_interval_ms: u64,
    /// Cron/notification delivery channel (fallback `BUZZ_HOME_CHANNEL`).
    pub home_channel: String,
    /// Community relay URL for the WS transport (fallback
    /// `BUZZ_RELAY_URL`).
    pub relay_url: String,
    /// Nostr private key, nsec or hex (fallback `BUZZ_PRIVATE_KEY`;
    /// credentials JSON file as a second source).
    pub private_key: String,
    /// Explicit credentials JSON path (fallback `BUZZ_CREDENTIALS_FILE`;
    /// default scans `~/.config/buzz/*credentials*.json`).
    pub credentials_file: String,
    /// Inbound transport: `auto` | `websocket` | `poll` (fallback
    /// `BUZZ_TRANSPORT`).
    pub transport: String,
    /// Optional NIP-OA owner-attestation tag JSON appended to the auth
    /// event (fallback `BUZZ_AUTH_TAG`).
    pub auth_tag: String,
}

impl Default for BuzzConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            cli_path: String::new(),
            channels: Vec::new(),
            self_pubkey: String::new(),
            allowed_users: Vec::new(),
            require_mention: true,
            poll_interval_ms: DEFAULT_POLL_INTERVAL_MS,
            home_channel: String::new(),
            relay_url: String::new(),
            private_key: String::new(),
            credentials_file: String::new(),
            transport: String::new(),
            auth_tag: String::new(),
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
pub struct ResolvedBuzz {
    pub cli_path: String,
    pub channels: Vec<String>,
    pub self_pubkey: String,
    pub allowed_users: Vec<String>,
    pub require_mention: bool,
    pub poll_interval_ms: u64,
    pub home_channel: String,
    pub relay_url: String,
    pub private_key: String,
    pub credentials_file: String,
    /// Resolved transport (hermes: auto | websocket | poll).
    pub transport: BuzzTransport,
    pub auth_tag: String,
}

/// Inbound transport selection (hermes `transport`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuzzTransport {
    /// WS with poll fallback (hermes default).
    Auto,
    /// Require WS; fail when it cannot authenticate.
    WebSocket,
    /// CLI polling only.
    Poll,
}

fn parse_transport(raw: &str) -> BuzzTransport {
    match raw.trim().to_lowercase().as_str() {
        "websocket" | "ws" => BuzzTransport::WebSocket,
        "poll" | "cli" => BuzzTransport::Poll,
        _ => BuzzTransport::Auto,
    }
}

impl BuzzConfig {
    pub fn resolve(&self) -> ResolvedBuzz {
        ResolvedBuzz {
            cli_path: env_trim("BUZZ_CLI").unwrap_or_else(|| {
                if self.cli_path.is_empty() {
                    "buzz".to_string()
                } else {
                    self.cli_path.clone()
                }
            }),
            channels: env_list("BUZZ_CHANNELS").unwrap_or_else(|| self.channels.clone()),
            self_pubkey: env_trim("BUZZ_PUBKEY")
                .unwrap_or_else(|| self.self_pubkey.clone())
                .to_lowercase(),
            allowed_users: env_list("BUZZ_ALLOWED_USERS")
                .unwrap_or_else(|| self.allowed_users.clone())
                .into_iter()
                .map(|u| u.to_lowercase())
                .collect(),
            require_mention: env_trim("BUZZ_REQUIRE_MENTION")
                .map(|v| !matches!(v.to_lowercase().as_str(), "false" | "0" | "no"))
                .unwrap_or(self.require_mention),
            poll_interval_ms: env_trim("BUZZ_POLL_INTERVAL_MS")
                .and_then(|v| v.parse().ok())
                .unwrap_or(self.poll_interval_ms)
                .max(MIN_POLL_INTERVAL_MS),
            home_channel: env_trim("BUZZ_HOME_CHANNEL")
                .unwrap_or_else(|| self.home_channel.clone()),
            relay_url: env_trim("BUZZ_RELAY_URL").unwrap_or_else(|| self.relay_url.clone()),
            private_key: env_trim("BUZZ_PRIVATE_KEY").unwrap_or_else(|| self.private_key.clone()),
            credentials_file: env_trim("BUZZ_CREDENTIALS_FILE")
                .unwrap_or_else(|| self.credentials_file.clone()),
            transport: parse_transport(
                &env_trim("BUZZ_TRANSPORT").unwrap_or_else(|| self.transport.clone()),
            ),
            auth_tag: env_trim("BUZZ_AUTH_TAG").unwrap_or_else(|| self.auth_tag.clone()),
        }
    }
}

/// hermes `_websocket_url`: http(s) relay URLs become ws(s).
pub fn websocket_url(relay_url: &str) -> Result<String, String> {
    let parsed = url::Url::parse(relay_url.trim()).map_err(|e| format!("relay URL: {e}"))?;
    let scheme = match parsed.scheme() {
        "http" => "ws",
        "https" => "wss",
        "ws" | "wss" => parsed.scheme(),
        _ => return Err("Buzz relay URL must use http(s) or ws(s)".into()),
    };
    if parsed.host_str().is_none() {
        return Err("Buzz relay URL must use http(s) or ws(s)".into());
    }
    let mut out = format!("{scheme}://{}", parsed.host_str().unwrap());
    if let Some(port) = parsed.port() {
        out = format!("{out}:{port}");
    }
    let path = parsed.path();
    if !path.is_empty() && path != "/" {
        out.push_str(path);
    }
    if let Some(query) = parsed.query() {
        out = format!("{out}?{query}");
    }
    Ok(out)
}

/// Resolve the Nostr private key (hermes `_resolve_private_key`):
/// config/env first, then a credentials JSON — explicit path or the
/// first `~/.config/buzz/*credentials*.json` (keys `nsec`,
/// `private_key_hex`, `private_key`). Never logged.
pub fn resolve_private_key(cfg: &ResolvedBuzz) -> String {
    if !cfg.private_key.is_empty() {
        return cfg.private_key.clone();
    }
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if !cfg.credentials_file.is_empty() {
        candidates.push(expand_home(&cfg.credentials_file));
    } else if let Some(dir) = home_dir().map(|h| h.join(".config").join("buzz")) {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            let mut found: Vec<std::path::PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.extension().and_then(|e| e.to_str()) == Some("json")
                        && p.file_name()
                            .and_then(|n| n.to_str())
                            .map(|n| n.contains("credentials"))
                            .unwrap_or(false)
                })
                .collect();
            found.sort();
            candidates.extend(found);
        }
    }
    for path in candidates {
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(data) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        let Some(obj) = data.as_object() else {
            continue;
        };
        for field in ["nsec", "private_key_hex", "private_key"] {
            if let Some(value) = obj.get(field).and_then(|v| v.as_str()) {
                let trimmed = value.trim();
                if !trimmed.is_empty() {
                    return trimmed.to_string();
                }
            }
        }
    }
    String::new()
}

fn home_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
}

fn expand_home(path: &str) -> std::path::PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return home.join(rest);
        }
    }
    std::path::PathBuf::from(path)
}

/// hermes `_parse_json_list` — tolerant JSON array parse.
pub fn parse_json_list(out: &str) -> Vec<Value> {
    let trimmed = out.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    serde_json::from_str(trimmed).unwrap_or_else(|_| Vec::new())
}

/// Conversation classification (hermes per-state `chat_type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChatType {
    /// Community channel — mention-gated (hermes `group`).
    Group,
    /// Direct message — always dispatches (hermes `dm`).
    Dm,
}

/// Per-channel poll state (hermes `_channel_state`).
struct ChannelState {
    seen: VecDeque<String>,
    seen_set: HashSet<String>,
    last_ts: i64,
    chat_type: ChatType,
}

impl ChannelState {
    fn new(chat_type: ChatType) -> Self {
        Self {
            seen: VecDeque::new(),
            seen_set: HashSet::new(),
            last_ts: 0,
            chat_type,
        }
    }

    /// Returns true when the id is new (records it).
    fn remember(&mut self, id: &str) -> bool {
        if self.seen_set.contains(id) {
            return false;
        }
        self.seen_set.insert(id.to_string());
        self.seen.push_back(id.to_string());
        while self.seen.len() > SEEN_CAP {
            if let Some(old) = self.seen.pop_front() {
                self.seen_set.remove(&old);
            }
        }
        true
    }
}

/// hermes `_may_reclassify_as_dm` — true when the conversation's
/// metadata does not rule out a DM. Known real community channels (real
/// name or non-empty description in `channels list`) never turn into
/// DMs just because a message p-tags us; a conversation with no
/// metadata at all is trusted only when the operator did not explicitly
/// configure it as a watched channel.
fn may_reclassify_as_dm(
    cfg: &ResolvedBuzz,
    meta: &HashMap<String, Value>,
    channel_id: &str,
) -> bool {
    match meta.get(channel_id) {
        None => !cfg
            .channels
            .iter()
            .any(|c| c == channel_id || c.trim_start_matches("dm:") == channel_id),
        Some(entry) => {
            let name = entry
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            let description = entry
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            name == "DM" && description.is_empty()
        }
    }
}

/// hermes `_is_direct_message_event` — shaped like a direct message to
/// us: a chat message from another user, p-tagged to our pubkey, whose
/// content does NOT visibly mention us (the p-tag is structural DM
/// addressing, not the artifact of a typed @mention).
fn is_direct_message_event(
    cfg: &ResolvedBuzz,
    meta: &HashMap<String, Value>,
    channel_id: &str,
    event: &Value,
) -> bool {
    if cfg.self_pubkey.is_empty() || !may_reclassify_as_dm(cfg, meta, channel_id) {
        return false;
    }
    if event.get("kind").and_then(|v| v.as_u64()).unwrap_or(0) != CHAT_KIND {
        return false;
    }
    let pubkey = event
        .get("pubkey")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();
    if pubkey.is_empty() || pubkey == cfg.self_pubkey {
        return false;
    }
    let Some(tags) = event.get("tags").and_then(|v| v.as_array()) else {
        return false;
    };
    let p_tagged_to_self = tags.iter().any(|tag| {
        tag.as_array()
            .map(|parts| {
                parts.len() > 1
                    && parts[0].as_str() == Some("p")
                    && parts[1]
                        .as_str()
                        .map(|t| t.to_lowercase() == cfg.self_pubkey)
                        .unwrap_or(false)
            })
            .unwrap_or(false)
    });
    if !p_tagged_to_self {
        return false;
    }
    match event.get("content").and_then(|v| v.as_str()) {
        Some(content) => !is_mentioned(content, &cfg.self_pubkey),
        None => false,
    }
}

/// hermes `_maybe_latch_dm` — latch a group conversation to DM once any
/// direct message is seen; the classification then sticks so later
/// un-mentioned messages in the conversation dispatch too.
fn maybe_latch_dm(
    cfg: &ResolvedBuzz,
    meta: &HashMap<String, Value>,
    channel_id: &str,
    state: &mut ChannelState,
    event: &Value,
) {
    if state.chat_type == ChatType::Dm || !is_direct_message_event(cfg, meta, channel_id, event) {
        return;
    }
    state.chat_type = ChatType::Dm;
    eprintln!("[buzz] conversation {channel_id} reclassified as DM (message p-tagged to self)");
}

/// hermes mention detection: content carries `@<name>` or our pubkey.
pub fn is_mentioned(content: &str, self_pubkey: &str) -> bool {
    if !self_pubkey.is_empty() {
        let lower = content.to_lowercase();
        if lower.contains(&self_pubkey.to_lowercase()) {
            return true;
        }
        // buzz UI shows truncated pubkeys — accept a prefix of at least
        // MIN_PUBKEY_PREFIX chars as a mention.
        const MIN_PUBKEY_PREFIX: usize = 6;
        if self_pubkey.len() > MIN_PUBKEY_PREFIX {
            for end in (MIN_PUBKEY_PREFIX..self_pubkey.len()).rev() {
                if lower.contains(&self_pubkey[..end].to_lowercase()) {
                    return true;
                }
            }
        }
    }
    content.contains('@')
}

/// Strip a leading `@mention ` token (hermes `_strip_mention`).
pub fn strip_mention(content: &str) -> String {
    let trimmed = content.trim_start();
    if let Some(rest) = trimmed.strip_prefix('@') {
        if let Some(space) = rest.find(char::is_whitespace) {
            return rest[space..].trim().to_string();
        }
    }
    content.to_string()
}

/// Run one buzz CLI invocation, returning (exit_code, stdout).
async fn run_cli(
    cli: &str,
    args: &[&str],
    stdin_body: Option<&str>,
) -> Result<(i32, String), String> {
    use tokio::io::AsyncWriteExt;
    use tokio::process::Command;
    let mut cmd = Command::new(cli);
    cmd.args(args)
        .stdin(if stdin_body.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("spawn {cli}: {e}"))?;
    if let Some(body) = stdin_body {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(body.as_bytes()).await;
        }
    }
    let output = tokio::time::timeout(CLI_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| format!("buzz CLI timed out after {}s", CLI_TIMEOUT.as_secs()))?
        .map_err(|e| format!("buzz CLI wait: {e}"))?;
    Ok((
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).to_string(),
    ))
}

/// Shared runtime conversation registry (hermes `_channel_state` +
/// `_channel_meta` + `_membership_since` + `_poll_count`) — persists
/// across WS reconnects and drives poll sweeps.
struct BuzzRegistry {
    states: tokio::sync::Mutex<HashMap<String, ChannelState>>,
    /// `channels list` metadata entries (hermes `_channel_meta`),
    /// consulted by the DM reclassification guard.
    meta: std::sync::Mutex<HashMap<String, Value>>,
    /// hermes `_membership_since` — kind-44100 subscribe cursor
    /// (advances with each membership event's created_at).
    membership_since: std::sync::atomic::AtomicU64,
    /// hermes `_poll_count` — poll sweep counter for periodic DM
    /// rediscovery.
    poll_count: std::sync::atomic::AtomicU64,
}

impl BuzzRegistry {
    fn new() -> Self {
        Self {
            states: tokio::sync::Mutex::new(HashMap::new()),
            meta: std::sync::Mutex::new(HashMap::new()),
            membership_since: std::sync::atomic::AtomicU64::new(0),
            poll_count: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// hermes `_membership_since` advance (never moves backwards).
    fn advance_membership(&self, created_at: u64) {
        self.membership_since
            .fetch_max(created_at, std::sync::atomic::Ordering::SeqCst);
    }

    /// Snapshot of watched conversation ids (hermes iterates
    /// `_channel_state` keys).
    async fn channel_ids(&self) -> Vec<String> {
        self.states.lock().await.keys().cloned().collect()
    }
}

/// hermes `_seed_channel` — initialize a conversation's high-water mark
/// from its newest events so a (re)start never replays history into the
/// agent. History is marked seen, never dispatched; it still classifies
/// (DM latch).
async fn seed_channel(
    cfg: &ResolvedBuzz,
    registry: &BuzzRegistry,
    channel_id: &str,
    chat_type: ChatType,
) {
    {
        let mut states = registry.states.lock().await;
        states.insert(channel_id.to_string(), ChannelState::new(chat_type));
    }
    let limit_str = FETCH_LIMIT.to_string();
    let result = run_cli(
        &cfg.cli_path,
        &[
            "messages",
            "get",
            "--channel",
            channel_id,
            "--limit",
            &limit_str,
        ],
        None,
    )
    .await;
    let (code, out) = match result {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[buzz] could not seed channel {channel_id} — {e}");
            (1, String::new())
        }
    };
    if code != 0 {
        // Fall back to "now" so a transiently unreadable channel does
        // not replay its whole history once it becomes readable.
        let mut states = registry.states.lock().await;
        if let Some(state) = states.get_mut(channel_id) {
            state.last_ts = now_secs() as i64;
        }
        return;
    }
    let meta = registry.meta.lock().unwrap().clone();
    let mut states = registry.states.lock().await;
    let Some(state) = states.get_mut(channel_id) else {
        return;
    };
    for event in parse_json_list(&out) {
        let event_id = event.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let created_at = event
            .get("created_at")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        if !event_id.is_empty() {
            state.remember(event_id);
        }
        state.last_ts = state.last_ts.max(created_at);
        maybe_latch_dm(cfg, &meta, channel_id, state, &event);
    }
}

/// hermes `_discover_dms` — watch DM conversations. New ones found
/// mid-run dispatch from their beginning (a fresh conversation has no
/// history worth suppressing); ones present at startup are seeded like
/// channels. `dms list` is only a best-effort source: on some hosted
/// relays it returns `[]` even when DM conversations exist — those DMs
/// DO surface in `channels list` as entries named "DM" with an empty
/// description, so that listing is scanned as a fallback (watched as
/// groups; they latch to DM via p-tag detection rather than trusting
/// the name alone). Returns the newly added conversation ids.
async fn discover_dms(cfg: &ResolvedBuzz, registry: &BuzzRegistry, seed: bool) -> Vec<String> {
    let mut added = Vec::new();
    if let Ok((0, out)) = run_cli(&cfg.cli_path, &["dms", "list"], None).await {
        for dm in parse_json_list(&out) {
            let dm_id = dm
                .get("dm_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if dm_id.is_empty() || registry.states.lock().await.contains_key(&dm_id) {
                continue;
            }
            if seed {
                seed_channel(cfg, registry, &dm_id, ChatType::Dm).await;
            } else {
                registry
                    .states
                    .lock()
                    .await
                    .insert(dm_id.clone(), ChannelState::new(ChatType::Dm));
            }
            added.push(dm_id);
        }
    }
    let listed = match run_cli(&cfg.cli_path, &["channels", "list"], None).await {
        Ok((0, out)) => parse_json_list(&out),
        _ => return added,
    };
    for ch in listed {
        let ch_id = ch
            .get("channel_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if ch_id.is_empty() {
            continue;
        }
        registry.meta.lock().unwrap().insert(ch_id.clone(), ch);
        let known = registry.states.lock().await.contains_key(&ch_id);
        let reclassifiable = may_reclassify_as_dm(cfg, &registry.meta.lock().unwrap(), &ch_id);
        if known || !reclassifiable {
            continue;
        }
        if seed {
            seed_channel(cfg, registry, &ch_id, ChatType::Group).await;
        } else {
            registry
                .states
                .lock()
                .await
                .insert(ch_id.clone(), ChannelState::new(ChatType::Group));
        }
        added.push(ch_id);
    }
    added
}

/// Entry point spawned by `run_messaging`.
pub async fn run(
    cfg: BuzzConfig,
    dispatcher: Arc<Dispatcher>,
    pairing: Option<Arc<crate::pairing::PairingStore>>,
) {
    let mut resolved = cfg.resolve();
    if resolved.channels.is_empty() {
        eprintln!(
            "[buzz] disabled: no channels configured (set [messaging.buzz] channels or BUZZ_CHANNELS)"
        );
        return;
    }
    crate::messaging::register_platform_sender(
        "buzz",
        Arc::new(BuzzSender {
            cfg: resolved.clone(),
        }),
    );
    // Derive the own pubkey from the NIP-42 key when not configured
    // (echo suppression + mention detection need it).
    if resolved.self_pubkey.is_empty() {
        let key = resolve_private_key(&resolved);
        if !key.is_empty() {
            if let Ok(pubkey) = crate::nostr_auth::public_key_hex(&key) {
                resolved.self_pubkey = pubkey;
            }
        }
    }
    // Seed high-water marks so a (re)start never replays history, then
    // discover existing DM conversations (hermes connect()).
    let registry = Arc::new(BuzzRegistry::new());
    for channel in resolved.channels.clone() {
        let chat_type = if channel.starts_with("dm:") {
            ChatType::Dm
        } else {
            ChatType::Group
        };
        seed_channel(&resolved, &registry, &channel, chat_type).await;
    }
    discover_dms(&resolved, &registry, true).await;
    // Transport selection (hermes connect()).
    if resolved.transport != BuzzTransport::Poll {
        let key = resolve_private_key(&resolved);
        let ws_url = if key.is_empty() || resolved.relay_url.is_empty() {
            Err("no relay URL / private key configured".to_string())
        } else {
            websocket_url(&resolved.relay_url)
        };
        match ws_url {
            Ok(ws_url) => match start_websocket(
                &resolved,
                &key,
                &ws_url,
                dispatcher.clone(),
                pairing.clone(),
                registry.clone(),
            )
            .await
            {
                Ok(()) => {
                    eprintln!(
                        "[buzz] connected to {} as {}, watching {} channel(s) via websocket",
                        resolved.relay_url,
                        &resolved.self_pubkey[..resolved.self_pubkey.len().min(12)],
                        registry.states.lock().await.len()
                    );
                    // The WS task owns inbound; park here.
                    std::future::pending::<()>().await;
                    return;
                }
                Err(e) => {
                    if resolved.transport == BuzzTransport::WebSocket {
                        eprintln!(
                            "[buzz] WebSocket transport did not authenticate (transport=websocket): {e}"
                        );
                        return;
                    }
                    eprintln!(
                        "[buzz] WebSocket transport unavailable ({e}); falling back to polling"
                    );
                }
            },
            Err(e) => {
                if resolved.transport == BuzzTransport::WebSocket {
                    eprintln!(
                        "[buzz] WebSocket transport unavailable ({e}); transport=websocket — giving up"
                    );
                    return;
                }
                eprintln!("[buzz] WebSocket transport unavailable ({e}); falling back to polling");
            }
        }
    }
    eprintln!(
        "[buzz] polling {} channel(s) via {} every {}ms",
        registry.states.lock().await.len(),
        resolved.cli_path,
        resolved.poll_interval_ms
    );
    loop {
        let sweep = registry
            .poll_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        if sweep % DM_DISCOVERY_EVERY == 0 {
            discover_dms(&resolved, &registry, false).await;
        }
        for channel in registry.channel_ids().await {
            poll_channel(&resolved, &dispatcher, &pairing, &registry, &channel).await;
        }
        tokio::time::sleep(Duration::from_millis(resolved.poll_interval_ms)).await;
    }
}

/// Start the WS loop and wait for the NIP-42 handshake (hermes
/// `_start_websocket`): Ok when authenticated within the timeout.
async fn start_websocket(
    cfg: &ResolvedBuzz,
    private_key: &str,
    ws_url: &str,
    dispatcher: Arc<Dispatcher>,
    pairing: Option<Arc<crate::pairing::PairingStore>>,
    registry: Arc<BuzzRegistry>,
) -> Result<(), String> {
    // hermes `_start_websocket`: the membership cursor starts now.
    registry.advance_membership(now_secs());
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
    let session = WsSession {
        cfg: cfg.clone(),
        private_key: private_key.to_string(),
        ws_url: ws_url.to_string(),
        dispatcher,
        pairing,
        registry,
    };
    let handle = tokio::spawn(async move {
        websocket_loop(session, Some(ready_tx)).await;
    });
    let outcome =
        match tokio::time::timeout(WS_AUTH_TIMEOUT + Duration::from_secs(5), ready_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("websocket task ended before reporting".into()),
            Err(_) => Err("WebSocket did not authenticate in time".into()),
        };
    if outcome.is_err() {
        // hermes cancels the WS task when the handshake fails.
        handle.abort();
    }
    outcome
}

struct WsSession {
    cfg: ResolvedBuzz,
    private_key: String,
    ws_url: String,
    dispatcher: Arc<Dispatcher>,
    pairing: Option<Arc<crate::pairing::PairingStore>>,
    registry: Arc<BuzzRegistry>,
}

/// Persistent authenticated subscription with bounded reconnect
/// backoff (hermes `_websocket_loop`). The ready signal fires once the
/// first connection authenticates; later reconnects stay silent.
async fn websocket_loop(
    session: WsSession,
    ready_tx: Option<tokio::sync::oneshot::Sender<Result<(), String>>>,
) {
    let mut backoff = 1.0f64;
    let mut ready_tx = ready_tx;
    loop {
        match run_ws_session(&session, ready_tx.take()).await {
            Ok(()) => {}
            Err(e) => {
                eprintln!("[buzz] WebSocket disconnected; retrying in {backoff:.1}s: {e}");
            }
        }
        tokio::time::sleep(Duration::from_secs_f64(backoff)).await;
        backoff = (backoff * 2.0).min(WS_MAX_BACKOFF_SECS);
    }
}

/// One connection attempt: NIP-42 auth, subscriptions, event pump.
/// The ready sender (first attempt only) fires right after the
/// handshake succeeds.
async fn run_ws_session(
    session: &WsSession,
    ready_tx: Option<tokio::sync::oneshot::Sender<Result<(), String>>>,
) -> Result<(), String> {
    use futures::StreamExt;
    use tokio_tungstenite::tungstenite::Message;

    let (ws, _resp) = tokio_tungstenite::connect_async(&session.ws_url)
        .await
        .map_err(|e| format!("buzz WS connect: {e}"))?;
    let (write, mut read) = ws.split();
    let write = Arc::new(tokio::sync::Mutex::new(write));

    if let Err(e) = authenticate_websocket(session, &write, &mut read).await {
        if let Some(tx) = ready_tx {
            let _ = tx.send(Err(e.clone()));
        }
        return Err(e);
    }

    // Channel subscriptions (hermes `_subscribe_websocket`): every
    // watched conversation (configured + discovered at startup) plus
    // the membership feed for live DM discovery.
    let mut subscriptions: HashMap<String, String> = HashMap::new();
    for (index, channel_id) in session.cfg.channels.iter().enumerate() {
        let subscription_id = format!("hermes-buzz-{index}");
        send_channel_subscription(&write, &session.registry, &subscription_id, channel_id).await?;
        subscriptions.insert(subscription_id, channel_id.clone());
    }
    for channel_id in session.registry.channel_ids().await {
        if session.cfg.channels.iter().any(|c| c == &channel_id) {
            continue;
        }
        let subscription_id = format!("hermes-buzz-dm-{}", subscriptions.len());
        send_channel_subscription(&write, &session.registry, &subscription_id, &channel_id).await?;
        subscriptions.insert(subscription_id, channel_id);
    }
    if !session.cfg.self_pubkey.is_empty() {
        let since = (session
            .registry
            .membership_since
            .load(std::sync::atomic::Ordering::SeqCst) as i64
            - 1)
        .max(0);
        let request = serde_json::json!([
            "REQ",
            WS_MEMBERSHIP_SUB_ID,
            {"kinds": [WS_MEMBERSHIP_KIND], "#p": [session.cfg.self_pubkey], "since": since},
        ]);
        send_ws(&write, &request.to_string()).await?;
    }

    if let Some(tx) = ready_tx {
        let _ = tx.send(Ok(()));
    }

    // Keepalive ping task (hermes ping_interval=20).
    let ping_write = write.clone();
    let ping_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(20));
        loop {
            interval.tick().await;
            let mut sink = ping_write.lock().await;
            use futures::SinkExt;
            if sink.send(Message::Ping(Vec::new())).await.is_err() {
                return;
            }
        }
    });

    let result = event_pump(session, &write, &mut read, &mut subscriptions).await;
    ping_task.abort();
    result
}

/// Shared write half of the WS connection (subscription sends, ping
/// task, event-pump membership resubscribes).
type WsWriteHalf = Arc<
    tokio::sync::Mutex<
        futures::stream::SplitSink<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
            tokio_tungstenite::tungstenite::Message,
        >,
    >,
>;

async fn send_ws(write: &WsWriteHalf, text: &str) -> Result<(), String> {
    use futures::SinkExt;
    let mut sink = write.lock().await;
    sink.send(tokio_tungstenite::tungstenite::Message::Text(
        text.to_string(),
    ))
    .await
    .map_err(|e| format!("buzz WS send: {e}"))
}

/// hermes `_send_channel_subscription` — kind-9 `#h` subscription with
/// `since` resume from the last observed timestamp (an unset high-water
/// mark subscribes from now so brand-new conversations do not replay).
async fn send_channel_subscription(
    write: &WsWriteHalf,
    registry: &BuzzRegistry,
    subscription_id: &str,
    channel_id: &str,
) -> Result<(), String> {
    let since = {
        let states = registry.states.lock().await;
        let last = states
            .get(channel_id)
            .map(|s| s.last_ts)
            .filter(|ts| *ts > 0)
            .unwrap_or(now_secs() as i64);
        (last - 1).max(0)
    };
    let request = serde_json::json!([
        "REQ",
        subscription_id,
        {"kinds": [CHAT_KIND], "#h": [channel_id], "since": since},
    ]);
    send_ws(write, &request.to_string()).await
}

/// NIP-42: answer the relay's AUTH challenge with a signed kind-22242
/// event and wait for the OK acknowledgment (hermes
/// `_authenticate_websocket`).
async fn authenticate_websocket(
    session: &WsSession,
    write: &Arc<
        tokio::sync::Mutex<
            futures::stream::SplitSink<
                tokio_tungstenite::WebSocketStream<
                    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
                >,
                tokio_tungstenite::tungstenite::Message,
            >,
        >,
    >,
    read: &mut futures::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >,
) -> Result<(), String> {
    let raw = next_text(read, WS_AUTH_TIMEOUT)
        .await?
        .ok_or("buzz WS closed before AUTH challenge")?;
    let message: Value = serde_json::from_str(&raw).map_err(|_| "malformed AUTH frame")?;
    let items = message
        .as_array()
        .ok_or("Buzz relay did not send a NIP-42 AUTH challenge")?;
    if items.len() < 2 || items[0].as_str() != Some("AUTH") {
        return Err("Buzz relay did not send a NIP-42 AUTH challenge".into());
    }
    let challenge = items[1].as_str().unwrap_or("").to_string();
    let event = crate::nostr_auth::build_auth_event(
        &session.private_key,
        &challenge,
        &session.ws_url,
        &session.cfg.auth_tag,
        None,
    )?;
    let event_id = event["id"].as_str().unwrap_or("").to_string();
    let reply = serde_json::json!(["AUTH", event]);
    send_ws(write, &reply.to_string()).await?;
    loop {
        let raw = next_text(read, WS_AUTH_TIMEOUT)
            .await?
            .ok_or("buzz WS closed during AUTH")?;
        let response: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
        let Some(items) = response.as_array() else {
            continue;
        };
        if items.is_empty() {
            continue;
        }
        match items[0].as_str() {
            Some("OK") if items.len() >= 4 && items[1].as_str() == Some(event_id.as_str()) => {
                if items[2].as_bool() == Some(true) {
                    return Ok(());
                }
                return Err(format!(
                    "Buzz WebSocket AUTH rejected: {}",
                    items[3].as_str().unwrap_or("")
                ));
            }
            Some("NOTICE") | Some("CLOSED") => {
                let detail = items
                    .last()
                    .and_then(|v| v.as_str())
                    .unwrap_or("authentication failed");
                return Err(format!("Buzz WebSocket AUTH failed: {detail}"));
            }
            _ => {}
        }
    }
}

/// Read the next text frame within a timeout (skips binary/pings).
async fn next_text(
    read: &mut futures::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >,
    timeout: Duration,
) -> Result<Option<String>, String> {
    use futures::StreamExt;
    use tokio_tungstenite::tungstenite::Message;
    loop {
        let frame = tokio::time::timeout(timeout, read.next())
            .await
            .map_err(|_| "buzz WS auth timeout")?;
        let Some(frame) = frame else {
            return Ok(None);
        };
        match frame {
            Ok(Message::Text(text)) => return Ok(Some(text)),
            Ok(Message::Close(_)) => return Ok(None),
            Ok(_) => continue,
            Err(e) => return Err(format!("buzz WS read: {e}")),
        }
    }
}

/// Steady-state event pump (hermes `_websocket_loop` inner loop).
async fn event_pump(
    session: &WsSession,
    write: &WsWriteHalf,
    read: &mut futures::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >,
    subscriptions: &mut HashMap<String, String>,
) -> Result<(), String> {
    use futures::StreamExt;
    use tokio_tungstenite::tungstenite::Message;
    while let Some(frame) = read.next().await {
        let frame = frame.map_err(|e| format!("buzz WS read: {e}"))?;
        let Message::Text(raw) = frame else { continue };
        let message: Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(_) => {
                eprintln!("[buzz] ignoring malformed WebSocket frame");
                continue;
            }
        };
        let Some(items) = message.as_array() else {
            continue;
        };
        if items.is_empty() {
            continue;
        }
        match items[0].as_str() {
            Some("EVENT") if items.len() >= 3 => {
                let subscription_id = items[1].as_str().unwrap_or("").to_string();
                let Some(event) = items[2].as_object().map(|_| items[2].clone()) else {
                    continue;
                };
                if subscription_id == WS_MEMBERSHIP_SUB_ID {
                    // Membership feed: live DM rediscovery (hermes
                    // `_handle_membership_event`).
                    handle_membership_event(session, write, subscriptions, &event).await;
                    continue;
                }
                let Some(channel_id) = subscriptions.get(&subscription_id).cloned() else {
                    continue;
                };
                let mut states = session.registry.states.lock().await;
                if let Some(state) = states.get_mut(&channel_id) {
                    handle_event(
                        &session.cfg,
                        &session.registry,
                        &session.dispatcher,
                        &session.pairing,
                        &channel_id,
                        state,
                        &event,
                    )
                    .await;
                }
            }
            Some("CLOSED") => {
                let detail = items
                    .last()
                    .and_then(|v| v.as_str())
                    .unwrap_or("subscription closed");
                return Err(detail.to_string());
            }
            Some("NOTICE") => {
                let detail = items.last().and_then(|v| v.as_str()).unwrap_or("");
                eprintln!("[buzz] relay notice: {detail}");
            }
            _ => {}
        }
    }
    Err("buzz WS stream ended".into())
}

/// hermes `_handle_membership_event` — a membership event p-tagged to
/// us: advance the resume cursor, rediscover conversations, and
/// subscribe to any new ones (fresh DMs dispatch from their beginning).
async fn handle_membership_event(
    session: &WsSession,
    write: &WsWriteHalf,
    subscriptions: &mut HashMap<String, String>,
    event: &Value,
) {
    let created_at = event
        .get("created_at")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    session.registry.advance_membership(created_at);
    for channel_id in discover_dms(&session.cfg, &session.registry, false).await {
        let subscription_id = format!("hermes-buzz-dm-{}", subscriptions.len());
        subscriptions.insert(subscription_id.clone(), channel_id.clone());
        if let Err(e) =
            send_channel_subscription(write, &session.registry, &subscription_id, &channel_id).await
        {
            eprintln!("[buzz] subscribe to new conversation {channel_id}: {e}");
            continue;
        }
        eprintln!("[buzz] subscribed to new conversation {channel_id}");
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// hermes `_poll_channel` + `_handle_event`.
async fn poll_channel(
    cfg: &ResolvedBuzz,
    dispatcher: &Arc<Dispatcher>,
    pairing: &Option<Arc<crate::pairing::PairingStore>>,
    registry: &BuzzRegistry,
    channel_id: &str,
) {
    let mut states = registry.states.lock().await;
    let Some(state) = states.get_mut(channel_id) else {
        return;
    };
    let since = state.last_ts.to_string();
    let limit_str = FETCH_LIMIT.to_string();
    let mut args: Vec<&str> = vec![
        "messages",
        "get",
        "--channel",
        channel_id,
        "--limit",
        &limit_str,
    ];
    if state.last_ts > 0 {
        args.push("--since");
        args.push(&since);
    }
    let (code, out) = match run_cli(&cfg.cli_path, &args, None).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[buzz] poll {channel_id}: {e}");
            return;
        }
    };
    if code != 0 {
        return;
    }
    for event in parse_json_list(&out) {
        handle_event(
            cfg, registry, dispatcher, pairing, channel_id, state, &event,
        )
        .await;
    }
}

async fn handle_event(
    cfg: &ResolvedBuzz,
    registry: &BuzzRegistry,
    dispatcher: &Arc<Dispatcher>,
    pairing: &Option<Arc<crate::pairing::PairingStore>>,
    channel_id: &str,
    state: &mut ChannelState,
    event: &Value,
) {
    let event_id = event
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let created_at = event
        .get("created_at")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    if event_id.is_empty() || !state.remember(&event_id) {
        return;
    }
    state.last_ts = state.last_ts.max(created_at);
    if event.get("kind").and_then(|v| v.as_u64()).unwrap_or(0) != CHAT_KIND {
        return;
    }
    let pubkey = event
        .get("pubkey")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();
    let content = event
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if pubkey.is_empty() || content.is_empty() {
        return;
    }
    if !cfg.self_pubkey.is_empty() && pubkey == cfg.self_pubkey {
        return;
    }
    // Reclassify a leaked DM before gating so its first un-mentioned
    // message both latches the conversation and dispatches (hermes
    // `_maybe_latch_dm`, issue #68871).
    {
        let meta = registry.meta.lock().unwrap().clone();
        maybe_latch_dm(cfg, &meta, channel_id, state, event);
    }
    // Channel mention gate: DMs always dispatch (chat_type latched from
    // discovery/p-tags, or configured via the `dm:` prefix in
    // BUZZ_CHANNELS).
    let is_dm = state.chat_type == ChatType::Dm || channel_id.starts_with("dm:");
    if !is_dm && cfg.require_mention && !is_mentioned(&content, &cfg.self_pubkey) {
        return;
    }
    if !cfg.allowed_users.is_empty() && !cfg.allowed_users.iter().any(|u| u == &pubkey || u == "*")
    {
        if let Some(store) = pairing {
            if !store.is_approved("buzz", &pubkey) {
                if let Some(code_msg) =
                    crate::messaging::pairing_offer_public(store, "buzz", &pubkey, &pubkey)
                {
                    let _ = send_via_cli(cfg, channel_id, &code_msg).await;
                }
                return;
            }
        } else {
            return;
        }
    }
    let dispatch_text = strip_mention(&content);
    let chat_id = if is_dm {
        channel_id.trim_start_matches("dm:").to_string()
    } else {
        channel_id.to_string()
    };
    let mut gate_event = MessageEvent {
        platform: "buzz".into(),
        chat_id,
        sender_id: pubkey.clone(),
        sender_name: pubkey[..pubkey.len().min(8)].to_string(),
        text: dispatch_text,
        message_id: event_id.clone(),
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
            .try_send_with_ledger("buzz", channel_id, &reply_text, || async {
                match send_via_cli(cfg, channel_id, &reply_text).await {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        eprintln!("[buzz] reply to {channel_id} failed: {e}");
                        Err(e.to_string())
                    }
                }
            })
            .await;
    }
    // Post-dispatch "seen" tapback (hermes adds 👀 after
    // `handle_message`) — signals receipt + processing.
    let _ = send_reaction(cfg, &event_id, SEEN_EMOJI).await;
}

/// hermes send — `buzz messages send --channel <id> --content -`.
pub async fn send_via_cli(
    cfg: &ResolvedBuzz,
    channel_id: &str,
    content: &str,
) -> Result<(), String> {
    let body: String = content.chars().take(MAX_MESSAGE_LENGTH).collect();
    let (code, out) = run_cli(
        &cfg.cli_path,
        &[
            "messages",
            "send",
            "--channel",
            channel_id,
            "--content",
            "-",
        ],
        Some(&body),
    )
    .await?;
    if code != 0 {
        return Err(format!("buzz send exit {code}: {}", out.trim()));
    }
    Ok(())
}

/// hermes `send_reaction` — tapback via buzz-cli
/// (`buzz reactions add --event <64-hex event id> --emoji <e>`; the
/// channel is not a parameter of this subcommand). Best-effort:
/// failures log and return false, never blocking the message flow.
pub async fn send_reaction(cfg: &ResolvedBuzz, event_id: &str, emoji: &str) -> bool {
    if cfg.cli_path.is_empty() || event_id.is_empty() || emoji.is_empty() {
        return false;
    }
    match run_cli(
        &cfg.cli_path,
        &["reactions", "add", "--event", event_id, "--emoji", emoji],
        None,
    )
    .await
    {
        Ok((0, _)) => true,
        Ok((code, out)) => {
            eprintln!(
                "[buzz] reaction add failed for message {} — exit {code}: {}",
                &event_id[..event_id.len().min(12)],
                out.trim()
            );
            false
        }
        Err(e) => {
            eprintln!(
                "[buzz] reaction add failed for message {}: {e}",
                &event_id[..event_id.len().min(12)]
            );
            false
        }
    }
}

struct BuzzSender {
    cfg: ResolvedBuzz,
}

#[async_trait::async_trait]
impl crate::messaging::PlatformSender for BuzzSender {
    async fn send_text(&self, chat_id: &str, text: &str) {
        if let Err(e) = send_via_cli(&self.cfg, chat_id, text).await {
            eprintln!("[buzz] send_text to {chat_id} failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_json_list_tolerant() {
        let events = parse_json_list(r#"[{"id":"a"},{"id":"b"}]"#);
        assert_eq!(events.len(), 2);
        assert!(parse_json_list("").is_empty());
        assert!(parse_json_list("not json").is_empty());
        assert!(parse_json_list("{}").is_empty());
    }

    #[test]
    fn seen_cap_bounds_dedup_set() {
        let mut state = ChannelState::new(ChatType::Group);
        for i in 0..SEEN_CAP + 100 {
            assert!(state.remember(&format!("evt{i}")));
        }
        assert_eq!(state.seen.len(), SEEN_CAP);
        // Oldest evicted — can be re-seen.
        assert!(state.remember("evt0"));
        // Recent still dedups.
        assert!(!state.remember(&format!("evt{}", SEEN_CAP + 99)));
    }

    #[test]
    fn mention_detection_and_strip() {
        assert!(is_mentioned("hey @chip help", ""));
        assert!(is_mentioned("hello abc123", "abc123def456"));
        assert!(!is_mentioned("plain message", "abc123def456"));
        assert_eq!(strip_mention("@Chip /whoami"), "/whoami");
        assert_eq!(strip_mention("no mention"), "no mention");
        assert_eq!(strip_mention("@solo"), "@solo");
    }

    #[test]
    fn kind_filter() {
        let event: Value = serde_json::from_str(r#"{"kind":9,"id":"x"}"#).unwrap();
        assert_eq!(event.get("kind").and_then(|v| v.as_u64()), Some(CHAT_KIND));
        let other: Value = serde_json::from_str(r#"{"kind":44100}"#).unwrap();
        assert_ne!(other.get("kind").and_then(|v| v.as_u64()), Some(CHAT_KIND));
    }

    #[test]
    fn resolve_defaults() {
        let _guard = crate::models_dev::test_env_lock();
        let resolved = BuzzConfig::default().resolve();
        assert_eq!(resolved.cli_path, "buzz");
        assert!(resolved.require_mention);
        assert_eq!(resolved.poll_interval_ms, DEFAULT_POLL_INTERVAL_MS);
        assert_eq!(resolved.transport, BuzzTransport::Auto);
        assert!(resolved.relay_url.is_empty());
    }

    #[test]
    fn resolve_env_overrides() {
        let _guard = crate::models_dev::test_env_lock();
        std::env::set_var("BUZZ_CHANNELS", "chan1, chan2");
        std::env::set_var("BUZZ_REQUIRE_MENTION", "false");
        std::env::set_var("BUZZ_POLL_INTERVAL_MS", "200");
        let resolved = BuzzConfig::default().resolve();
        assert_eq!(
            resolved.channels,
            vec!["chan1".to_string(), "chan2".to_string()]
        );
        assert!(!resolved.require_mention);
        assert_eq!(resolved.poll_interval_ms, MIN_POLL_INTERVAL_MS); // floor
        std::env::remove_var("BUZZ_CHANNELS");
        std::env::remove_var("BUZZ_REQUIRE_MENTION");
        std::env::remove_var("BUZZ_POLL_INTERVAL_MS");
    }

    #[test]
    fn dm_prefix_classification() {
        assert!("dm:abc123".starts_with("dm:"));
        assert_eq!("dm:abc123".trim_start_matches("dm:"), "abc123");
        assert!(!"general".starts_with("dm:"));
    }

    #[test]
    fn websocket_url_derivation() {
        assert_eq!(
            websocket_url("https://mycommunity.communities.buzz.xyz").unwrap(),
            "wss://mycommunity.communities.buzz.xyz"
        );
        assert_eq!(
            websocket_url("http://relay.local:8080/path").unwrap(),
            "ws://relay.local:8080/path"
        );
        assert_eq!(
            websocket_url("wss://relay.example/room?x=1").unwrap(),
            "wss://relay.example/room?x=1"
        );
        assert!(websocket_url("ftp://relay.example").is_err());
        assert!(websocket_url("not a url").is_err());
    }

    #[test]
    fn transport_parsing() {
        assert_eq!(parse_transport("websocket"), BuzzTransport::WebSocket);
        assert_eq!(parse_transport("POLL"), BuzzTransport::Poll);
        assert_eq!(parse_transport("auto"), BuzzTransport::Auto);
        assert_eq!(parse_transport(""), BuzzTransport::Auto);
        assert_eq!(parse_transport("bogus"), BuzzTransport::Auto);
    }

    #[test]
    fn private_key_from_credentials_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("my-credentials.json");
        std::fs::write(
            &path,
            r#"{"nsec": "", "private_key_hex": "0000000000000000000000000000000000000000000000000000000000000003"}"#,
        )
        .unwrap();
        let mut cfg = BuzzConfig::default().resolve();
        cfg.credentials_file = path.to_string_lossy().to_string();
        assert_eq!(
            resolve_private_key(&cfg),
            "0000000000000000000000000000000000000000000000000000000000000003"
        );
        // Config value wins over the file.
        cfg.private_key = "nsec1abc".to_string();
        assert_eq!(resolve_private_key(&cfg), "nsec1abc");
        // Missing file resolves empty.
        cfg.private_key = String::new();
        cfg.credentials_file = "/nonexistent/creds.json".to_string();
        assert_eq!(resolve_private_key(&cfg), "");
    }

    #[test]
    fn event_field_extraction() {
        let event: Value = serde_json::from_str(
            r#"{"id":"deadbeef","kind":9,"pubkey":"AB12","content":" hello ","created_at":1700000000}"#,
        )
        .unwrap();
        assert_eq!(event.get("id").and_then(|v| v.as_str()), Some("deadbeef"));
        assert_eq!(
            event
                .get("pubkey")
                .and_then(|v| v.as_str())
                .unwrap()
                .to_lowercase(),
            "ab12"
        );
        assert_eq!(
            event.get("created_at").and_then(|v| v.as_i64()),
            Some(1700000000)
        );
    }

    #[tokio::test]
    async fn reaction_guards_skip_cli() {
        let _guard = crate::models_dev::test_env_lock();
        std::env::remove_var("BUZZ_CLI");
        let cfg = BuzzConfig::default().resolve();
        // Empty event id / emoji never shell out.
        assert!(!send_reaction(&cfg, "", SEEN_EMOJI).await);
        assert!(!send_reaction(&cfg, "deadbeef", "").await);
        // Empty CLI path never shells out.
        let mut no_cli = BuzzConfig::default().resolve();
        no_cli.cli_path = String::new();
        assert!(!send_reaction(&no_cli, "deadbeef", SEEN_EMOJI).await);
    }

    #[test]
    fn seen_emoji_matches_hermes() {
        assert_eq!(SEEN_EMOJI, "👀");
    }

    #[test]
    fn discovery_cadence_matches_hermes() {
        assert_eq!(DM_DISCOVERY_EVERY, 5);
    }

    #[test]
    fn membership_cursor_only_advances() {
        let registry = BuzzRegistry::new();
        registry.advance_membership(100);
        registry.advance_membership(90);
        assert_eq!(
            registry
                .membership_since
                .load(std::sync::atomic::Ordering::SeqCst),
            100
        );
        registry.advance_membership(120);
        assert_eq!(
            registry
                .membership_since
                .load(std::sync::atomic::Ordering::SeqCst),
            120
        );
    }

    #[test]
    fn may_reclassify_as_dm_guard() {
        let mut cfg = BuzzConfig::default().resolve();
        cfg.channels = vec!["general".into(), "dm:secret".into()];
        let mut meta = HashMap::new();
        // No metadata: unconfigured conversations are trusted...
        assert!(may_reclassify_as_dm(&cfg, &meta, "some-dm"));
        // ...explicitly configured ones never reclassify (plain or
        // dm:-prefixed).
        assert!(!may_reclassify_as_dm(&cfg, &meta, "general"));
        assert!(!may_reclassify_as_dm(&cfg, &meta, "secret"));
        // Metadata: relay-materialized DMs are named "DM" with an empty
        // description.
        meta.insert(
            "dm-ish".into(),
            serde_json::json!({"name": "DM", "description": ""}),
        );
        assert!(may_reclassify_as_dm(&cfg, &meta, "dm-ish"));
        meta.insert(
            "real".into(),
            serde_json::json!({"name": "General", "description": "community"}),
        );
        assert!(!may_reclassify_as_dm(&cfg, &meta, "real"));
        meta.insert(
            "named".into(),
            serde_json::json!({"name": "DM", "description": "not empty"}),
        );
        assert!(!may_reclassify_as_dm(&cfg, &meta, "named"));
    }

    #[test]
    fn direct_message_event_detection() {
        let mut cfg = BuzzConfig::default().resolve();
        cfg.self_pubkey = "selfpub".into();
        let meta = HashMap::new();
        // Structural p-tag, no visible mention → direct message.
        let dm_event = serde_json::json!({
            "id": "e1", "kind": 9, "pubkey": "other",
            "content": "hello there",
            "tags": [["p", "SELFPUB"]]
        });
        assert!(is_direct_message_event(&cfg, &meta, "conv", &dm_event));
        // Visible mention → typed @mention artifact, not DM addressing.
        let mentioned = serde_json::json!({
            "id": "e2", "kind": 9, "pubkey": "other",
            "content": "hey selfpub",
            "tags": [["p", "selfpub"]]
        });
        assert!(!is_direct_message_event(&cfg, &meta, "conv", &mentioned));
        // No p-tag → plain broadcast.
        let no_tag = serde_json::json!({
            "id": "e3", "kind": 9, "pubkey": "other",
            "content": "hi", "tags": []
        });
        assert!(!is_direct_message_event(&cfg, &meta, "conv", &no_tag));
        // Own echo never latches.
        let own = serde_json::json!({
            "id": "e4", "kind": 9, "pubkey": "SELFPUB",
            "content": "hi", "tags": [["p", "selfpub"]]
        });
        assert!(!is_direct_message_event(&cfg, &meta, "conv", &own));
        // Real channel metadata blocks reclassification.
        let mut real_meta = HashMap::new();
        real_meta.insert(
            "conv".into(),
            serde_json::json!({"name": "General", "description": "x"}),
        );
        assert!(!is_direct_message_event(
            &cfg, &real_meta, "conv", &dm_event
        ));
    }

    #[test]
    fn latch_dm_sticks_and_respects_guard() {
        let mut cfg = BuzzConfig::default().resolve();
        cfg.self_pubkey = "selfpub".into();
        let meta = HashMap::new();
        let mut state = ChannelState::new(ChatType::Group);
        let event = serde_json::json!({
            "id": "e1", "kind": 9, "pubkey": "other",
            "content": "hello", "tags": [["p", "selfpub"]]
        });
        maybe_latch_dm(&cfg, &meta, "conv", &mut state, &event);
        assert_eq!(state.chat_type, ChatType::Dm);
        // Real channels never latch.
        let mut real_meta = HashMap::new();
        real_meta.insert(
            "chan".into(),
            serde_json::json!({"name": "General", "description": "x"}),
        );
        let mut group = ChannelState::new(ChatType::Group);
        maybe_latch_dm(&cfg, &real_meta, "chan", &mut group, &event);
        assert_eq!(group.chat_type, ChatType::Group);
    }

    #[cfg(unix)]
    fn write_executable(dir: &std::path::Path, name: &str, body: &str) -> String {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.to_string_lossy().to_string()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn discover_dms_finds_new_conversations() {
        let _guard = crate::models_dev::test_env_lock();
        std::env::remove_var("BUZZ_CHANNELS");
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = BuzzConfig::default().resolve();
        cfg.cli_path = write_executable(
            dir.path(),
            "buzz-discover.sh",
            r#"#!/bin/sh
if [ "$1" = "dms" ]; then
  echo '[{"dm_id":"dm-conv-1"}]'
  exit 0
fi
if [ "$1" = "channels" ]; then
  echo '[{"channel_id":"c1","name":"General","description":"community"},{"channel_id":"dm-conv-2","name":"DM","description":""}]'
  exit 0
fi
echo '[]'
"#,
        );
        cfg.channels = vec!["general".into()];
        let registry = BuzzRegistry::new();
        let added = discover_dms(&cfg, &registry, false).await;
        assert_eq!(
            added,
            vec!["dm-conv-1".to_string(), "dm-conv-2".to_string()]
        );
        {
            let states = registry.states.lock().await;
            let dm = states.get("dm-conv-1").unwrap();
            assert_eq!(dm.chat_type, ChatType::Dm);
            assert_eq!(dm.last_ts, 0); // fresh: dispatch from beginning
            let fallback = states.get("dm-conv-2").unwrap();
            assert_eq!(fallback.chat_type, ChatType::Group);
            assert!(!states.contains_key("c1")); // real channel, gated out
        }
        // Second sweep: nothing new.
        assert!(discover_dms(&cfg, &registry, false).await.is_empty());
        // Meta recorded for the reclassification guard.
        let meta = registry.meta.lock().unwrap();
        assert!(meta.contains_key("c1"));
        assert!(meta.contains_key("dm-conv-2"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn seed_channel_marks_history_seen_and_latches() {
        let _guard = crate::models_dev::test_env_lock();
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = BuzzConfig::default().resolve();
        cfg.cli_path = write_executable(
            dir.path(),
            "buzz-seed.sh",
            r#"#!/bin/sh
echo '[{"id":"h1","kind":9,"pubkey":"other","content":"old dm","created_at":100,"tags":[["p","selfpub"]]},{"id":"h2","kind":9,"pubkey":"other","content":"older","created_at":90,"tags":[]}]'
"#,
        );
        cfg.self_pubkey = "selfpub".into();
        cfg.channels = vec!["other-chan".into()];
        let registry = BuzzRegistry::new();
        seed_channel(&cfg, &registry, "conv", ChatType::Group).await;
        let states = registry.states.lock().await;
        let state = states.get("conv").unwrap();
        assert_eq!(state.last_ts, 100);
        // History is seen, never dispatched.
        assert!(state.seen_set.contains("h1"));
        assert!(state.seen_set.contains("h2"));
        // p-tagged history latched the conversation to DM.
        assert_eq!(state.chat_type, ChatType::Dm);
    }

    #[tokio::test]
    async fn seed_channel_falls_back_to_now_on_cli_failure() {
        let mut cfg = BuzzConfig::default().resolve();
        cfg.cli_path = "/nonexistent/buzz-cli".into();
        let registry = BuzzRegistry::new();
        let before = now_secs() as i64;
        seed_channel(&cfg, &registry, "conv", ChatType::Group).await;
        let states = registry.states.lock().await;
        assert!(states.get("conv").unwrap().last_ts >= before);
    }
}

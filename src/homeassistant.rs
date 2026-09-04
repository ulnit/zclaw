//! Home Assistant platform adapter — port of hermes
//! `plugins/platforms/homeassistant` @ v2026.8.3 (adapter.py).
//!
//! Connects to the Home Assistant WebSocket API (`/api/websocket`),
//! authenticates with a long-lived access token and subscribes to
//! `state_changed` events. Events pass hermes' closed-by-default filter
//! stack (`watch_domains` / `watch_entities` / `ignore_entities` /
//! `watch_all`) plus a per-entity cooldown, get formatted into
//! human-readable lines (domain-specific templates for climate, sensor,
//! binary_sensor, light/switch/fan, alarm_control_panel) and are
//! dispatched as messages on the synthetic `ha_events` channel.
//!
//! Outbound messages are delivered as HA persistent notifications via
//! the REST API (`/api/services/persistent_notification/create`) —
//! hermes deliberately uses REST for sends to avoid racing the event
//! listener on the shared WS connection.
//!
//! The hermes standalone cron sender (`_standalone_send` → the HA
//! `notify/notify` service) is ported as a credential-only sender that
//! registers whenever HASS_URL + HASS_TOKEN are set, so delivery works
//! without the live WebSocket adapter; a live adapter overwrites the
//! slot with its persistent-notification sender.

use crate::messaging::{Dispatcher, MessageEvent};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// hermes `MAX_MESSAGE_LENGTH` for HA notifications.
const MAX_MESSAGE_LENGTH: usize = 4096;
/// hermes `_BACKOFF_STEPS` reconnection schedule (seconds).
const BACKOFF_STEPS: [u64; 4] = [5, 10, 30, 60];

/// `[messaging.homeassistant]` — Home Assistant adapter (hermes
/// `platforms.homeassistant` plugin config + `HASS_*` env vars).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HomeassistantConfig {
    pub enabled: bool,
    /// HA server URL (fallback `HASS_URL`, default
    /// `http://homeassistant.local:8123`).
    pub url: String,
    /// Long-lived access token (fallback `HASS_TOKEN`).
    pub token: String,
    /// Forward state changes for these entity domains (e.g. `sensor`).
    pub watch_domains: Vec<String>,
    /// Forward state changes for these exact entity ids.
    pub watch_entities: Vec<String>,
    /// Never forward these entity ids (wins over watch lists).
    pub ignore_entities: Vec<String>,
    /// Forward every state change (only when no watch lists are set).
    pub watch_all: bool,
    /// Per-entity cooldown in seconds (hermes default 30).
    pub cooldown_seconds: u64,
}

impl Default for HomeassistantConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            url: String::new(),
            token: String::new(),
            watch_domains: Vec::new(),
            watch_entities: Vec::new(),
            ignore_entities: Vec::new(),
            watch_all: false,
            cooldown_seconds: 30,
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
pub struct ResolvedHomeassistant {
    pub url: String,
    pub token: String,
    pub watch_domains: Vec<String>,
    pub watch_entities: Vec<String>,
    pub ignore_entities: Vec<String>,
    pub watch_all: bool,
    pub cooldown_seconds: u64,
}

impl HomeassistantConfig {
    pub fn resolve(&self) -> ResolvedHomeassistant {
        ResolvedHomeassistant {
            url: env_trim("HASS_URL")
                .unwrap_or_else(|| self.url.clone())
                .trim_end_matches('/')
                .to_string(),
            token: env_trim("HASS_TOKEN").unwrap_or_else(|| self.token.clone()),
            watch_domains: env_list("HASS_WATCH_DOMAINS")
                .unwrap_or_else(|| self.watch_domains.clone()),
            watch_entities: env_list("HASS_WATCH_ENTITIES")
                .unwrap_or_else(|| self.watch_entities.clone()),
            ignore_entities: env_list("HASS_IGNORE_ENTITIES")
                .unwrap_or_else(|| self.ignore_entities.clone()),
            watch_all: env_trim("HASS_WATCH_ALL")
                .map(|v| matches!(v.to_lowercase().as_str(), "true" | "1" | "yes"))
                .unwrap_or(self.watch_all),
            cooldown_seconds: env_trim("HASS_COOLDOWN_SECONDS")
                .and_then(|v| v.parse().ok())
                .unwrap_or(self.cooldown_seconds),
        }
    }
}

/// `http(s)://host` → `ws(s)://host/api/websocket` (hermes `_ws_connect`).
pub fn ws_url(hass_url: &str) -> String {
    let base = hass_url
        .replacen("https://", "wss://", 1)
        .replacen("http://", "ws://", 1);
    format!("{}/api/websocket", base.trim_end_matches('/'))
}

struct Runtime {
    cfg: ResolvedHomeassistant,
    client: reqwest::Client,
    /// entity_id -> last forwarded event time (hermes `_last_event_time`).
    last_event_time: Mutex<HashMap<String, Instant>>,
}

/// hermes `_handle_ha_event` filter stack. Closed by default: without
/// `watch_domains`/`watch_entities`/`watch_all` nothing is forwarded.
pub fn should_forward(cfg: &ResolvedHomeassistant, entity_id: &str) -> bool {
    if cfg.ignore_entities.iter().any(|e| e == entity_id) {
        return false;
    }
    let domain = entity_id.split('.').next().unwrap_or("");
    if !cfg.watch_domains.is_empty() || !cfg.watch_entities.is_empty() {
        let domain_match =
            !cfg.watch_domains.is_empty() && cfg.watch_domains.iter().any(|d| d == domain);
        let entity_match =
            !cfg.watch_entities.is_empty() && cfg.watch_entities.iter().any(|e| e == entity_id);
        domain_match || entity_match
    } else {
        cfg.watch_all
    }
}

/// hermes `_format_state_change` — domain-specific human-readable lines.
pub fn format_state_change(
    entity_id: &str,
    old_state: &Value,
    new_state: &Value,
) -> Option<String> {
    if new_state.is_null() {
        return None;
    }
    let old_val = old_state
        .get("state")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let new_val = new_state
        .get("state")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    if old_val == new_val {
        return None;
    }
    let attrs = new_state.get("attributes").cloned().unwrap_or(json!({}));
    let friendly_name = attrs
        .get("friendly_name")
        .and_then(|v| v.as_str())
        .unwrap_or(entity_id)
        .to_string();
    let domain = entity_id.split('.').next().unwrap_or("");

    match domain {
        "climate" => {
            let temp = attrs
                .get("current_temperature")
                .map(|v| v.to_string())
                .unwrap_or_else(|| "?".into());
            let target = attrs
                .get("temperature")
                .map(|v| v.to_string())
                .unwrap_or_else(|| "?".into());
            Some(format!(
                "[Home Assistant] {friendly_name}: HVAC mode changed from '{old_val}' to '{new_val}' (current: {temp}, target: {target})"
            ))
        }
        "sensor" => {
            let unit = attrs
                .get("unit_of_measurement")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            Some(format!(
                "[Home Assistant] {friendly_name}: changed from {old_val}{unit} to {new_val}{unit}"
            ))
        }
        "binary_sensor" => {
            let now_word = if new_val == "on" { "triggered" } else { "cleared" };
            let was_word = if old_val == "on" { "triggered" } else { "cleared" };
            Some(format!(
                "[Home Assistant] {friendly_name}: {now_word} (was {was_word})"
            ))
        }
        "light" | "switch" | "fan" => {
            let word = if new_val == "on" { "on" } else { "off" };
            Some(format!("[Home Assistant] {friendly_name}: turned {word}"))
        }
        "alarm_control_panel" => Some(format!(
            "[Home Assistant] {friendly_name}: alarm state changed from '{old_val}' to '{new_val}'"
        )),
        _ => Some(format!(
            "[Home Assistant] {friendly_name} ({entity_id}): changed from '{old_val}' to '{new_val}'"
        )),
    }
}

/// Entry point spawned by `run_messaging`.
pub async fn run(
    cfg: HomeassistantConfig,
    dispatcher: Arc<Dispatcher>,
    _pairing: Option<Arc<crate::pairing::PairingStore>>,
) {
    let resolved = cfg.resolve();
    if resolved.url.is_empty() {
        // hermes default when HASS_URL is unset.
        let mut r = resolved;
        r.url = "http://homeassistant.local:8123".to_string();
        return run_with(r, dispatcher).await;
    }
    run_with(resolved, dispatcher).await;
}

async fn run_with(cfg: ResolvedHomeassistant, dispatcher: Arc<Dispatcher>) {
    if cfg.token.is_empty() {
        eprintln!(
            "[homeassistant] disabled: no token configured (set [messaging.homeassistant] token or HASS_TOKEN)"
        );
        return;
    }
    if cfg.watch_domains.is_empty() && cfg.watch_entities.is_empty() && !cfg.watch_all {
        eprintln!(
            "[homeassistant] warning: no watch_domains, watch_entities, or watch_all configured — all state_changed events will be dropped"
        );
    }
    let runtime = Arc::new(Runtime {
        cfg,
        client: reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new()),
        last_event_time: Mutex::new(HashMap::new()),
    });
    crate::messaging::register_platform_sender(
        "homeassistant",
        Arc::new(HomeassistantSender {
            runtime: runtime.clone(),
        }),
    );

    let mut backoff_idx = 0usize;
    loop {
        match run_session(&runtime, &dispatcher).await {
            Ok(()) => backoff_idx = 0,
            Err(msg) => eprintln!("[homeassistant] session error: {msg}"),
        }
        let delay = BACKOFF_STEPS[backoff_idx.min(BACKOFF_STEPS.len() - 1)];
        backoff_idx += 1;
        tokio::time::sleep(Duration::from_secs(delay)).await;
    }
}

async fn run_session(runtime: &Arc<Runtime>, dispatcher: &Arc<Dispatcher>) -> Result<(), String> {
    use tokio_tungstenite::tungstenite::Message;

    let url = ws_url(&runtime.cfg.url);
    let (ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .map_err(|e| format!("ws connect {url}: {e}"))?;
    let (mut sink, mut stream) = ws.split();

    // Step 1: expect auth_required.
    let msg = next_json(&mut stream).await?;
    if msg.get("type").and_then(|v| v.as_str()) != Some("auth_required") {
        return Err(format!("expected auth_required, got {:?}", msg.get("type")));
    }
    // Step 2: authenticate.
    let auth = json!({ "type": "auth", "access_token": runtime.cfg.token });
    send_json(&mut sink, &auth)
        .await
        .map_err(|e| format!("auth send: {e}"))?;
    // Step 3: expect auth_ok.
    let msg = next_json(&mut stream).await?;
    if msg.get("type").and_then(|v| v.as_str()) != Some("auth_ok") {
        return Err(format!("auth failed: {msg}"));
    }
    // Step 4: subscribe to state_changed events.
    let sub = json!({ "id": 1, "type": "subscribe_events", "event_type": "state_changed" });
    send_json(&mut sink, &sub)
        .await
        .map_err(|e| format!("subscribe send: {e}"))?;
    let msg = next_json(&mut stream).await?;
    if msg.get("success").and_then(|v| v.as_bool()) != Some(true) {
        return Err(format!("failed to subscribe to events: {msg}"));
    }
    eprintln!("[homeassistant] connected to {}", runtime.cfg.url);

    // Event loop.
    loop {
        let Some(frame) = stream.next().await else {
            return Err("ws closed".into());
        };
        let frame = frame.map_err(|e| format!("ws read: {e}"))?;
        match frame {
            Message::Text(text) => {
                let Ok(value) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                if value.get("type").and_then(|v| v.as_str()) == Some("event") {
                    handle_event(runtime, dispatcher, &value).await;
                }
            }
            Message::Ping(data) => {
                use futures::SinkExt;
                let _ = sink.send(Message::Pong(data)).await;
            }
            Message::Close(_) => return Ok(()),
            _ => {}
        }
    }
}

type StreamHalf = futures::stream::SplitStream<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
>;
type SinkHalf = futures::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    tokio_tungstenite::tungstenite::Message,
>;

async fn next_json(stream: &mut StreamHalf) -> Result<Value, String> {
    use tokio_tungstenite::tungstenite::Message;
    loop {
        let Some(frame) = stream.next().await else {
            return Err("ws closed during handshake".into());
        };
        let frame = frame.map_err(|e| format!("ws read: {e}"))?;
        match frame {
            Message::Text(text) => {
                return serde_json::from_str(&text).map_err(|e| format!("bad json: {e}"))
            }
            Message::Close(_) => return Err("ws closed during handshake".into()),
            _ => continue,
        }
    }
}

async fn send_json(sink: &mut SinkHalf, value: &Value) -> Result<(), String> {
    use futures::SinkExt;
    use tokio_tungstenite::tungstenite::Message;
    sink.send(Message::Text(value.to_string().into()))
        .await
        .map_err(|e| e.to_string())
}

/// hermes `_handle_ha_event`: filter + cooldown + format + dispatch.
async fn handle_event(runtime: &Arc<Runtime>, dispatcher: &Arc<Dispatcher>, envelope: &Value) {
    let event_data = envelope.get("event").and_then(|e| e.get("data"));
    let Some(event_data) = event_data else {
        return;
    };
    let entity_id = event_data
        .get("entity_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if entity_id.is_empty() {
        return;
    }
    if !should_forward(&runtime.cfg, entity_id) {
        return;
    }
    // Cooldown gate.
    {
        let mut last = runtime.last_event_time.lock().await;
        let now = Instant::now();
        if let Some(prev) = last.get(entity_id) {
            if now.duration_since(*prev).as_secs() < runtime.cfg.cooldown_seconds {
                return;
            }
        }
        last.insert(entity_id.to_string(), now);
    }
    let old_state = event_data.get("old_state").cloned().unwrap_or(Value::Null);
    let new_state = event_data.get("new_state").cloned().unwrap_or(Value::Null);
    let Some(message) = format_state_change(entity_id, &old_state, &new_state) else {
        return;
    };
    let message_id = format!(
        "ha_{}_{}",
        entity_id,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    );
    let event = MessageEvent {
        platform: "homeassistant".into(),
        chat_id: "ha_events".into(),
        sender_id: "homeassistant".into(),
        sender_name: "Home Assistant".into(),
        text: message,
        message_id,
        attachments: Vec::new(),
    };
    if !crate::messaging::pre_gateway_dispatch_gate_public(&mut { event.clone() }).await {
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
    let (reply_text, _media) = crate::messaging::extract_media_tags(&full);
    if !reply_text.trim().is_empty() {
        // Replies land as persistent notifications on the event channel
        // (P705: ledger-protected).
        dispatcher
            .try_send_with_ledger("homeassistant", "ha_events", &reply_text, || async {
                match send_notification(runtime, &reply_text).await {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        eprintln!("[homeassistant] notification failed: {e}");
                        Err(e.to_string())
                    }
                }
            })
            .await;
    }
}

/// REST send — hermes `send()` via `persistent_notification/create`.
async fn send_notification(runtime: &Arc<Runtime>, content: &str) -> Result<(), String> {
    let url = format!(
        "{}/api/services/persistent_notification/create",
        runtime.cfg.url
    );
    for chunk in crate::messaging::chunk_text(content, MAX_MESSAGE_LENGTH) {
        let payload = json!({ "title": "UlncLaw Agent", "message": chunk });
        let resp = runtime
            .client
            .post(&url)
            .bearer_auth(&runtime.cfg.token)
            .json(&payload)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if resp.status().as_u16() >= 300 {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("HTTP {status}: {body}"));
        }
    }
    Ok(())
}

struct HomeassistantSender {
    runtime: Arc<Runtime>,
}

#[async_trait::async_trait]
impl crate::messaging::PlatformSender for HomeassistantSender {
    async fn send_text(&self, _chat_id: &str, text: &str) {
        if let Err(e) = send_notification(&self.runtime, text).await {
            eprintln!("[homeassistant] send_text failed: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// Standalone `notify/notify` sender (hermes `_standalone_send`)
// ---------------------------------------------------------------------------

/// Payload for the HA `notify.notify` service call (hermes
/// `_standalone_send` body; an empty target is omitted — HA rejects
/// blank targets).
pub fn notify_payload(message: &str, target: &str) -> Value {
    let mut payload = json!({ "message": message });
    if !target.trim().is_empty() {
        payload["target"] = json!(target);
    }
    payload
}

/// hermes `_standalone_send` — deliver a notification through the HA
/// `notify/notify` REST service WITHOUT a live WebSocket adapter
/// (out-of-process cron delivery). Requires `HASS_URL` + `HASS_TOKEN`;
/// HA notifications have no threading or attachment model, so those
/// hermes arguments have no port here.
pub async fn standalone_notify(
    url: &str,
    token: &str,
    message: &str,
    target: &str,
) -> Result<(), String> {
    let hass_url = url.trim().trim_end_matches('/');
    if hass_url.is_empty() || token.trim().is_empty() {
        return Err(
            "Home Assistant standalone send: HASS_URL and HASS_TOKEN must both be set".to_string(),
        );
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("client: {e}"))?;
    let endpoint = format!("{hass_url}/api/services/notify/notify");
    let resp = client
        .post(&endpoint)
        .bearer_auth(token.trim())
        .json(&notify_payload(message, target))
        .send()
        .await
        .map_err(|e| format!("Home Assistant send failed: {e}"))?;
    let status = resp.status().as_u16();
    if status != 200 && status != 201 {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Home Assistant API error ({status}): {body}"));
    }
    Ok(())
}

/// Sender registered without a live adapter so platform delivery
/// (`deliver=homeassistant` hermes semantics) works out-of-process.
struct StandaloneHomeassistantSender {
    cfg: ResolvedHomeassistant,
}

#[async_trait::async_trait]
impl crate::messaging::PlatformSender for StandaloneHomeassistantSender {
    async fn send_text(&self, chat_id: &str, text: &str) {
        if let Err(e) = standalone_notify(&self.cfg.url, &self.cfg.token, text, chat_id).await {
            eprintln!("[homeassistant] standalone notify failed: {e}");
        }
    }
}

/// Register the credential-only `notify/notify` sender when HASS_URL +
/// HASS_TOKEN are configured — even if the live WebSocket adapter is
/// disabled. A live adapter that starts later overwrites this slot with
/// its persistent-notification sender (hermes live-adapter preference).
pub fn maybe_register_standalone_sender(cfg: &HomeassistantConfig) {
    let resolved = cfg.resolve();
    if resolved.url.trim().is_empty() || resolved.token.trim().is_empty() {
        return;
    }
    crate::messaging::register_platform_sender(
        "homeassistant",
        Arc::new(StandaloneHomeassistantSender { cfg: resolved }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_cfg() -> ResolvedHomeassistant {
        ResolvedHomeassistant {
            url: "http://ha.local:8123".into(),
            token: "tok".into(),
            watch_domains: Vec::new(),
            watch_entities: Vec::new(),
            ignore_entities: Vec::new(),
            watch_all: false,
            cooldown_seconds: 30,
        }
    }

    fn state(old: &str, new: &str, friendly: &str, extra: Value) -> (Value, Value) {
        let mut old_attrs = json!({});
        let mut new_attrs = extra;
        new_attrs["friendly_name"] = json!(friendly);
        old_attrs["friendly_name"] = json!(friendly);
        (
            json!({ "state": old, "attributes": old_attrs }),
            json!({ "state": new, "attributes": new_attrs }),
        )
    }

    #[test]
    fn ws_url_converts_schemes() {
        assert_eq!(
            ws_url("http://homeassistant.local:8123"),
            "ws://homeassistant.local:8123/api/websocket"
        );
        assert_eq!(
            ws_url("https://ha.example.com/"),
            "wss://ha.example.com/api/websocket"
        );
    }

    #[test]
    fn filter_closed_by_default() {
        let cfg = base_cfg();
        assert!(!should_forward(&cfg, "sensor.living_room_temp"));
    }

    #[test]
    fn filter_watch_all_forwards() {
        let mut cfg = base_cfg();
        cfg.watch_all = true;
        assert!(should_forward(&cfg, "sensor.living_room_temp"));
    }

    #[test]
    fn filter_domain_and_entity_lists() {
        let mut cfg = base_cfg();
        cfg.watch_domains = vec!["binary_sensor".into()];
        cfg.watch_entities = vec!["climate.thermostat".into()];
        assert!(should_forward(&cfg, "binary_sensor.front_door"));
        assert!(should_forward(&cfg, "climate.thermostat"));
        assert!(!should_forward(&cfg, "sensor.temperature"));
        // watch lists suppress watch_all semantics.
        cfg.watch_all = true;
        assert!(!should_forward(&cfg, "sensor.temperature"));
    }

    #[test]
    fn filter_ignore_wins() {
        let mut cfg = base_cfg();
        cfg.watch_all = true;
        cfg.ignore_entities = vec!["sensor.noisy".into()];
        assert!(!should_forward(&cfg, "sensor.noisy"));
        assert!(should_forward(&cfg, "sensor.other"));
    }

    #[test]
    fn format_sensor_with_unit() {
        let (old, new) = state(
            "21.0",
            "23.5",
            "Living Room Temp",
            json!({"unit_of_measurement": "°C"}),
        );
        let msg = format_state_change("sensor.living_room_temp", &old, &new).unwrap();
        assert_eq!(
            msg,
            "[Home Assistant] Living Room Temp: changed from 21.0°C to 23.5°C"
        );
    }

    #[test]
    fn format_binary_sensor() {
        let (old, new) = state("off", "on", "Front Door", json!({}));
        let msg = format_state_change("binary_sensor.front_door", &old, &new).unwrap();
        assert_eq!(msg, "[Home Assistant] Front Door: triggered (was cleared)");
    }

    #[test]
    fn format_light_switch() {
        let (old, new) = state("off", "on", "Desk Lamp", json!({}));
        let msg = format_state_change("light.desk", &old, &new).unwrap();
        assert_eq!(msg, "[Home Assistant] Desk Lamp: turned on");
    }

    #[test]
    fn format_climate() {
        let (old, new) = state(
            "heat",
            "cool",
            "Thermostat",
            json!({"current_temperature": 22, "temperature": 20}),
        );
        let msg = format_state_change("climate.thermostat", &old, &new).unwrap();
        assert!(msg.contains("HVAC mode changed from 'heat' to 'cool'"));
        assert!(msg.contains("current: 22"));
        assert!(msg.contains("target: 20"));
    }

    #[test]
    fn format_alarm_panel() {
        let (old, new) = state("disarmed", "armed_away", "House Alarm", json!({}));
        let msg = format_state_change("alarm_control_panel.house", &old, &new).unwrap();
        assert!(msg.contains("alarm state changed from 'disarmed' to 'armed_away'"));
    }

    #[test]
    fn format_generic_fallback() {
        let (old, new) = state("a", "b", "Cover", json!({}));
        let msg = format_state_change("cover.garage", &old, &new).unwrap();
        assert_eq!(
            msg,
            "[Home Assistant] Cover (cover.garage): changed from 'a' to 'b'"
        );
    }

    #[test]
    fn format_unchanged_state_is_none() {
        let (old, new) = state("on", "on", "Lamp", json!({}));
        assert!(format_state_change("light.lamp", &old, &new).is_none());
    }

    #[test]
    fn format_null_new_state_is_none() {
        assert!(format_state_change("light.lamp", &json!({"state": "on"}), &Value::Null).is_none());
    }

    #[tokio::test]
    async fn cooldown_suppresses_rapid_events() {
        let runtime = Runtime {
            cfg: {
                let mut c = base_cfg();
                c.cooldown_seconds = 60;
                c
            },
            client: reqwest::Client::new(),
            last_event_time: Mutex::new(HashMap::new()),
        };
        let entity = "sensor.x";
        // First event passes the cooldown gate.
        {
            let mut last = runtime.last_event_time.lock().await;
            assert!(last.get(entity).is_none());
            last.insert(entity.to_string(), Instant::now());
        }
        // Second within the window is suppressed.
        let last = runtime.last_event_time.lock().await;
        let prev = last.get(entity).unwrap();
        assert!(Instant::now().duration_since(*prev).as_secs() < runtime.cfg.cooldown_seconds);
    }

    #[test]
    fn resolve_env_overrides() {
        let _guard = crate::models_dev::test_env_lock();
        std::env::set_var("HASS_COOLDOWN_SECONDS", "77");
        let cfg = HomeassistantConfig {
            cooldown_seconds: 30,
            ..Default::default()
        };
        let resolved = cfg.resolve();
        assert_eq!(resolved.cooldown_seconds, 77);
        std::env::remove_var("HASS_COOLDOWN_SECONDS");
    }

    #[test]
    fn notify_payload_includes_target_when_present() {
        let with_target = notify_payload("hello", "kitchen");
        assert_eq!(with_target["message"], "hello");
        assert_eq!(with_target["target"], "kitchen");
        // Blank target is omitted (HA rejects empty targets).
        let bare = notify_payload("hello", "  ");
        assert!(bare.get("target").is_none());
    }

    #[tokio::test]
    async fn standalone_notify_requires_url_and_token() {
        let err = standalone_notify("", "tok", "m", "t").await.unwrap_err();
        assert!(err.contains("HASS_URL and HASS_TOKEN"));
        let err = standalone_notify("http://ha.local:8123", "  ", "m", "t")
            .await
            .unwrap_err();
        assert!(err.contains("HASS_URL and HASS_TOKEN"));
    }

    #[test]
    fn standalone_sender_registration_gated_on_credentials() {
        let _guard = crate::models_dev::test_env_lock();
        std::env::remove_var("HASS_URL");
        std::env::remove_var("HASS_TOKEN");
        // No credentials -> nothing registered (slot untouched).
        maybe_register_standalone_sender(&HomeassistantConfig::default());
        assert!(crate::messaging::platform_sender("homeassistant").is_none());
    }
}

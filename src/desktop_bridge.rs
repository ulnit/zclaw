//! Gateway transport for the desktop UI bridge (P231).
//!
//! The desktop affordance tools (`close_terminal`, `read_terminal`,
//! `focus_pane`, `desktop_preview`, `react_to_message`) reach the renderer
//! through [`crate::desktop`]'s emitter + blocking read callback. When
//! the gateway runs as a desktop shell's child process
//! (`ULNCLAW_DESKTOP=1`), the renderer is a webview speaking HTTP, so
//! this module installs:
//!
//! - an **emitter** that fans every `(ui_session_id, event, payload)`
//!   out to a process-wide broadcast bus consumed by the
//!   `GET /api/desktop/events` SSE endpoint;
//! - a **read_terminal callback** that performs a blocking
//!   request/response round-trip: emit a `terminal.read.request` event
//!   carrying a unique id, then wait (20 s cap) for the webview's answer
//!   (`POST /api/desktop/read-response`, or the WS `terminal.read.respond`
//!   method — both resolve the same pending id).
//! - a **set_theme callback** (`theme.set`, 320 s cap — a picker waits for a
//!   human) with its own pending map and `theme.set.respond` method.
//!
//! 🔴 Event names are a cross-process contract with the vendored renderer and
//! the WS mirror passes them through verbatim, so every name here is pinned by a
//! test — an untested string is exactly how `terminal.read` drifted from
//! `terminal.read.request` and silently broke the round-trip.
//!
//! Install is automatic at gateway serve time when `ULNCLAW_DESKTOP`
//! is truthy; everywhere else this module stays inert.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::mpsc::{channel, Sender};
use std::sync::{Mutex, OnceLock};
use tokio::sync::broadcast;

/// How long a blocking terminal read waits for the webview answer.
pub const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Event name the renderer listens for to answer a `read_terminal` call
/// (`gateway-event.ts:1001`).
pub const EVENT_TERMINAL_READ: &str = "terminal.read.request";

/// Event name carrying a `set_theme` request to the renderer (P2b).
///
/// 🔴 As of this commit the vendored renderer has **no** `theme.set` listener —
/// its 40 `event.type` literals do not include it — so this round-trip cannot
/// complete until either the renderer learns the name or the accent leg is
/// re-routed onto the existing `skin.changed` pipeline (`gateway-event.ts:303`,
/// which calls `ingestBackendSkin(payload, { apply: true })`). Kept as a named
/// constant so the pending wiring is one edit, and so the test below fails
/// loudly if anyone renames it without deciding that.
pub const EVENT_THEME_SET: &str = "theme.set";

/// One desktop event envelope on the bus.
#[derive(Debug, Clone)]
pub struct DesktopEvent {
    pub session_id: String,
    pub event: String,
    pub payload: Value,
}

fn bus_sender() -> &'static broadcast::Sender<DesktopEvent> {
    static BUS: OnceLock<broadcast::Sender<DesktopEvent>> = OnceLock::new();
    BUS.get_or_init(|| broadcast::channel(256).0)
}

/// Subscribe to the desktop event bus (gateway SSE endpoint).
pub fn subscribe() -> broadcast::Receiver<DesktopEvent> {
    bus_sender().subscribe()
}

/// Publish an envelope on the bus.
pub fn publish(session_id: &str, event: &str, payload: &Value) {
    // A lagged or empty bus is fine — events are best-effort.
    let _ = bus_sender().send(DesktopEvent {
        session_id: session_id.to_string(),
        event: event.to_string(),
        payload: payload.clone(),
    });
}

// ── terminal.read round-trip ─────────────────────────────────────────────

fn pending_reads() -> &'static Mutex<HashMap<String, Sender<Result<String, String>>>> {
    static PENDING: OnceLock<Mutex<HashMap<String, Sender<Result<String, String>>>>> =
        OnceLock::new();
    PENDING.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Blocking `read_terminal` callback: emit a `terminal.read` request and
/// wait for the webview's answer (hermes' desktop read is likewise a
/// blocking renderer round-trip). Times out with an error after
/// [`READ_TIMEOUT`].
fn read_terminal_roundtrip(start_line: Option<u64>, count: Option<u64>) -> Result<String, String> {
    let id = uuid::Uuid::new_v4().simple().to_string()[..12].to_string();
    let (tx, rx) = channel();
    pending_reads().lock().unwrap().insert(id.clone(), tx);
    // 🔴 The renderer matches `terminal.read.request`
    // (desktop-electron/src/app/session/hooks/use-message-stream/gateway-event.ts:1001),
    // and the gateway WS mirror passes `event` through verbatim as the RpcEvent
    // `type` (gateway_ws.rs:98). Publishing `terminal.read` here meant the request
    // reached the wire under a name nothing listened for, so the renderer never
    // answered and every `read_terminal` call blocked for READ_TIMEOUT and then
    // failed. `.request`/`.respond` also matches the existing convention in the WS
    // dispatch table (approval./clarify./sudo./secret.request + .respond).
    publish(
        "",
        EVENT_TERMINAL_READ,
        &json!({
            "id": id,
            "start_line": start_line,
            "count": count,
        }),
    );
    match rx.recv_timeout(READ_TIMEOUT) {
        Ok(answer) => answer,
        Err(_) => {
            pending_reads().lock().unwrap().remove(&id);
            Err(format!(
                "desktop terminal read timed out after {}s (no renderer answer)",
                READ_TIMEOUT.as_secs()
            ))
        }
    }
}

/// Resolve a pending `terminal.read` (gateway `POST
/// /api/desktop/read-response`). Returns `false` when the id is unknown
/// or already resolved.
pub fn resolve_read(id: &str, answer: Result<String, String>) -> bool {
    let sender = pending_reads().lock().unwrap().remove(id);
    match sender {
        Some(tx) => tx.send(answer).is_ok(),
        None => false,
    }
}

// ── theme.set round-trip ────────────────────────────────────────────────

/// How long a blocking `set_theme` waits for the webview answer.
///
/// 🔴 Deliberately much longer than [`READ_TIMEOUT`]: a `picker` request shows a
/// card and waits for the *user* to choose, which upstream bounds at 5 minutes
/// ("wait for the user (up to 5 minutes)"). 20 s would fail every picker before a
/// human could click. The slack over 300 s is for the renderer to apply and answer.
pub const THEME_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(320);

fn pending_themes() -> &'static Mutex<HashMap<String, Sender<Result<String, String>>>> {
    static PENDING: OnceLock<Mutex<HashMap<String, Sender<Result<String, String>>>>> =
        OnceLock::new();
    PENDING.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Blocking `set_theme` callback: emit a `theme.set` request and wait for the
/// webview's answer. The renderer applies the values — or shows the picker card
/// and waits for the user — then reports **what it actually applied**, which is
/// the whole point: the model must describe what the user sees, so an unknown
/// layout preset or an off-screen session comes back as an error rather than a
/// silent success.
fn set_theme_roundtrip(session_id: &str, payload: Value) -> Result<String, String> {
    let id = uuid::Uuid::new_v4().simple().to_string()[..12].to_string();
    let (tx, rx) = channel();
    pending_themes().lock().unwrap().insert(id.clone(), tx);
    // 🔴 The session id rides on the envelope (not inside the payload) so the SSE
    // fan-out can route this to the window that owns the turn — a picker must not
    // pop up in an unrelated window.
    publish(
        session_id,
        EVENT_THEME_SET,
        &json!({ "id": id, "payload": payload }),
    );
    match rx.recv_timeout(THEME_TIMEOUT) {
        Ok(answer) => answer,
        Err(_) => {
            pending_themes().lock().unwrap().remove(&id);
            Err(format!(
                "desktop set_theme timed out after {}s (no renderer answer)",
                THEME_TIMEOUT.as_secs()
            ))
        }
    }
}

/// Resolve a pending `theme.set` (gateway `theme.set.respond`). Returns `false`
/// when the id is unknown or already resolved.
pub fn resolve_theme(id: &str, answer: Result<String, String>) -> bool {
    let sender = pending_themes().lock().unwrap().remove(id);
    match sender {
        Some(tx) => tx.send(answer).is_ok(),
        None => false,
    }
}

/// Install the bridge when running under a desktop host
/// (`ULNCLAW_DESKTOP` truthy). No-op otherwise. Called once at gateway
/// serve time.
pub fn install() {
    if !crate::desktop::desktop_env_enabled() {
        return;
    }
    crate::desktop::set_emitter(Some(Box::new(|session_id, event, payload| {
        publish(session_id, event, payload);
    })));
    crate::desktop::set_read_terminal_callback(Some(Box::new(read_terminal_roundtrip)));
    crate::desktop::set_theme_callback(Some(Box::new(set_theme_roundtrip)));
    tracing::info!("desktop bridge active: events on /api/desktop/events");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_reaches_subscribers() {
        let mut rx = subscribe();
        publish("sess-1", "pane.reveal", &json!({"pane": "chat"}));
        let envelope = rx.try_recv().expect("envelope");
        assert_eq!(envelope.session_id, "sess-1");
        assert_eq!(envelope.event, "pane.reveal");
        assert_eq!(envelope.payload["pane"], json!("chat"));
    }

    #[test]
    fn resolve_read_delivers_answer() {
        // Simulate the tool side: register a pending read, then resolve
        // it the way POST /api/desktop/read-response would.
        let id = "test-read-1";
        let (tx, rx) = channel();
        pending_reads().lock().unwrap().insert(id.to_string(), tx);
        assert!(resolve_read(id, Ok("line one\nline two".into())));
        assert_eq!(rx.recv().unwrap(), Ok("line one\nline two".to_string()));
        // Second resolve: unknown id now.
        assert!(!resolve_read(id, Ok("late".into())));
    }

    #[test]
    fn resolve_unknown_id_is_false() {
        assert!(!resolve_read("no-such-read", Err("x".into())));
    }

    /// 🔴 Cross-process contract test: the event names this bridge publishes must
    /// be the exact strings the vendored renderer listens for. The WS mirror
    /// passes `event` through verbatim (`gateway_ws.rs:98`), so a one-character
    /// drift here silently breaks the round-trip — the request goes out, nothing
    /// answers, and the tool blocks for the full timeout before failing. That is
    /// precisely how `terminal.read` (published) vs `terminal.read.request`
    /// (listened) broke `read_terminal`.
    ///
    /// Reads the renderer source directly so BOTH sides are pinned by one test.
    /// Skips (does not fail) when the desktop tree is absent, e.g. an
    /// engine-only checkout.
    #[test]
    fn published_event_names_match_the_renderer_listeners() {
        let manifest = env!("CARGO_MANIFEST_DIR");
        let renderer = std::path::Path::new(manifest)
            .join("desktop-electron/src/app/session/hooks/use-message-stream/gateway-event.ts");
        let Ok(source) = std::fs::read_to_string(&renderer) else {
            eprintln!(
                "skipping: renderer source not found at {} (engine-only checkout)",
                renderer.display()
            );
            return;
        };
        // The renderer's switch matches `event.type === '<name>'`.
        let listens = |name: &str| source.contains(&format!("event.type === '{name}'"));

        assert!(
            listens(EVENT_TERMINAL_READ),
            "renderer must listen for {EVENT_TERMINAL_READ:?} — the bridge publishes it and \
             read_terminal blocks until the renderer answers"
        );

        // 🔴 `theme.set` has NO renderer listener yet (P2b's remaining work), so it
        // is asserted the other way round: if this ever starts failing, the
        // renderer learned the name and the round-trip is now live — update this
        // test rather than deleting it.
        assert!(
            !listens(EVENT_THEME_SET),
            "the renderer now listens for {EVENT_THEME_SET:?} — the set_theme round-trip is \
             live; delete this stale expectation and add a positive assertion"
        );
        // …and the names it DOES listen for that this bridge drives must stay in sync.
        for name in ["terminal.close", "pane.reveal", "message.reaction"] {
            assert!(
                listens(name),
                "renderer listens for {name:?} — keep the publisher aligned"
            );
        }
    }

    /// 🔴 P2b `set_theme`: the theme round-trip has its own pending map and its
    /// own resolve path, and — unlike the terminal read — must deliver a real
    /// failure, because the renderer is the authority on preset ids.
    #[test]
    fn resolve_theme_delivers_answer_and_error() {
        let id = "test-theme-1";
        let (tx, rx) = channel();
        pending_themes().lock().unwrap().insert(id.to_string(), tx);
        let applied = r#"{"success": true, "applied": {"accent": "nous"}}"#;
        assert!(resolve_theme(id, Ok(applied.into())));
        assert_eq!(rx.recv().unwrap(), Ok(applied.to_string()));
        // Consumed: a second resolve no longer finds it.
        assert!(!resolve_theme(id, Ok("late".into())));

        // The failure channel round-trips too.
        let id2 = "test-theme-2";
        let (tx2, rx2) = channel();
        pending_themes()
            .lock()
            .unwrap()
            .insert(id2.to_string(), tx2);
        assert!(resolve_theme(
            id2,
            Err("unknown layout preset 'nope'".into())
        ));
        assert_eq!(
            rx2.recv().unwrap(),
            Err("unknown layout preset 'nope'".to_string())
        );
    }

    /// Theme and terminal pending maps must not cross-talk: a `theme.set.respond`
    /// for an unknown id is false, and a read id never resolves as a theme.
    #[test]
    fn theme_and_read_pending_maps_are_separate() {
        assert!(!resolve_theme("no-such-theme", Ok("x".into())));
        let read_id = "test-read-sep";
        let (tx, _rx) = channel();
        pending_reads()
            .lock()
            .unwrap()
            .insert(read_id.to_string(), tx);
        // Same id string, wrong map → not resolved, and the read stays pending.
        assert!(!resolve_theme(read_id, Ok("x".into())));
        assert!(pending_reads().lock().unwrap().contains_key(read_id));
        assert!(resolve_read(read_id, Ok("screen".into())));
    }

    /// 🔴 A picker waits for a human, so the theme timeout must be far longer
    /// than the terminal read's — 20 s would fail every picker before a click.
    #[test]
    fn theme_timeout_outlasts_the_terminal_read_timeout() {
        assert!(THEME_TIMEOUT > READ_TIMEOUT);
        // Upstream bounds the picker wait at 5 minutes.
        assert!(THEME_TIMEOUT.as_secs() >= 300);
    }
}

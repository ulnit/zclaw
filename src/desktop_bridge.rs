//! Gateway transport for the desktop UI bridge (P231).
//!
//! The desktop affordance tools (`close_terminal`, `read_terminal`,
//! `focus_pane`, `open_preview`, `react_to_message`) reach the renderer
//! through [`crate::desktop`]'s emitter + blocking read callback. When
//! the gateway runs as a desktop shell's child process
//! (`ULNCLAW_DESKTOP=1`), the renderer is a webview speaking HTTP, so
//! this module installs:
//!
//! - an **emitter** that fans every `(ui_session_id, event, payload)`
//!   out to a process-wide broadcast bus consumed by the
//!   `GET /api/desktop/events` SSE endpoint;
//! - a **read_terminal callback** that performs a blocking
//!   request/response round-trip: emit a `terminal.read` request event
//!   carrying a unique id, then wait (20 s cap) for the webview's
//!   `POST /api/desktop/read-response` answer.
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
    publish(
        "",
        "terminal.read",
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
}

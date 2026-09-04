//! Desktop UI bridge — port of hermes' `tools/desktop_ui.py`.
//!
//! The desktop affordances (`close_terminal`, `read_terminal`,
//! `focus_pane`, `open_preview`) live in a renderer process, so the
//! desktop-gated tools reach them through an emitter a host application
//! installs at startup via [`set_emitter`]. Everywhere else the emitter
//! stays `None` and the tools report "desktop only".
//!
//! Events carry the UI session id of the turn that emitted them so the
//! host can route the event to the window that owns the turn (hermes
//! routes off `HERMES_UI_SESSION_ID`).
//!
//! Wiring (mirrors hermes' desktop `tui_gateway`):
//! ```ignore
//! ulnclaw::desktop::set_emitter(Some(Box::new(|session_id, event, payload| {
//!     my_renderer.send(session_id, event, payload);
//! })));
//! ```
//! Installing an emitter also wires the terminal process registry's
//! close sink so `close_terminal` emits `terminal.close`.

use serde_json::{json, Value};
use std::sync::{Arc, Mutex, OnceLock};

/// Renderer-event sink: `(ui_session_id, event, payload)`.
pub type UiEmitter = Box<dyn Fn(&str, &str, &Value) + Send + Sync>;

/// Blocking read of the in-app terminal window: `(start_line, count)` →
/// raw renderer answer (JSON object or plain text).
pub type ReadTerminalFn =
    Box<dyn Fn(Option<u64>, Option<u64>) -> Result<String, String> + Send + Sync>;

fn emitter_slot() -> &'static Mutex<Option<Arc<dyn Fn(&str, &str, &Value) + Send + Sync>>> {
    static EMITTER: OnceLock<Mutex<Option<Arc<dyn Fn(&str, &str, &Value) + Send + Sync>>>> =
        OnceLock::new();
    EMITTER.get_or_init(|| Mutex::new(None))
}

fn read_terminal_slot() -> &'static Mutex<Option<ReadTerminalFn>> {
    static READER: OnceLock<Mutex<Option<ReadTerminalFn>>> = OnceLock::new();
    READER.get_or_init(|| Mutex::new(None))
}

/// Install (or clear) the renderer-event sink. Called by a desktop host.
///
/// Also wires the terminal process registry's close sink: while an
/// emitter is installed, `close_terminal` emits a `terminal.close` event
/// carrying the process id and whether the process is still running.
pub fn set_emitter(emitter: Option<UiEmitter>) {
    let shared: Option<Arc<dyn Fn(&str, &str, &Value) + Send + Sync>> =
        emitter.map(|e| Arc::from(e) as Arc<dyn Fn(&str, &str, &Value) + Send + Sync>);
    *emitter_slot().lock().unwrap() = shared.clone();
    // Wire (or clear) the close sink alongside the emitter.
    let sink = shared.map(|emit| {
        Arc::new(move |ui_session: &str, running: bool, process_id: &str| {
            emit(
                ui_session,
                "terminal.close",
                &json!({ "process_id": process_id, "running": running }),
            );
        }) as crate::tools::builtin::terminal::CloseSink
    });
    crate::tools::builtin::terminal::set_close_sink(sink);
}

/// Install (or clear) the blocking in-app terminal reader.
pub fn set_read_terminal_callback(callback: Option<ReadTerminalFn>) {
    *read_terminal_slot().lock().unwrap() = callback;
}

/// True when running under a desktop host (an emitter is wired).
pub fn available() -> bool {
    emitter_slot().lock().map(|g| g.is_some()).unwrap_or(false)
}

/// True when the `ULNCLAW_DESKTOP` env var enables desktop tool
/// registration (hermes: `HERMES_DESKTOP`).
pub fn desktop_env_enabled() -> bool {
    crate::config::get_env_value("ULNCLAW_DESKTOP")
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

/// Route `event` to the window that owns `ui_session_id`.
///
/// Returns `false` when no emitter is wired (i.e. not a desktop host).
pub fn emit(ui_session_id: &str, event: &str, payload: &Value) -> bool {
    let emit_fn = emitter_slot().lock().unwrap().clone();
    match emit_fn {
        Some(f) => {
            f(ui_session_id, event, payload);
            true
        }
        None => false,
    }
}

/// Dispatch a blocking in-app terminal read to the host callback.
///
/// `None` means no reader is wired (not a desktop host). The call runs
/// while holding the reader lock — only one terminal read can be in
/// flight per process, which matches the single-focused-window model.
pub fn read_terminal_window(
    start_line: Option<u64>,
    count: Option<u64>,
) -> Option<Result<String, String>> {
    let guard = read_terminal_slot().lock().unwrap();
    guard.as_ref().map(|f| f(start_line, count))
}

/// Coax a bare host/domain into a fetchable URL; leave paths + schemes
/// alone (port of hermes `open_preview_tool._normalize_target`).
///
/// `www.cnn.com` → `https://www.cnn.com`; `localhost:3000` →
/// `http://localhost:3000`. File paths and explicit schemes pass through
/// for the renderer's preview normalizer to classify.
pub fn normalize_preview_target(raw: &str) -> String {
    let v = raw.trim().trim_matches('`').trim();
    if v.is_empty()
        || v.contains("://")
        || v.starts_with('/')
        || v.starts_with("./")
        || v.starts_with("../")
        || v.starts_with('~')
        || v.starts_with("file:")
    {
        return v.to_string();
    }
    let localhost =
        regex::Regex::new(r"^(?i)(localhost|127\.0\.0\.1|0\.0\.0\.0|\[::1\])(:\d+)?(/|$)")
            .expect("static regex");
    if localhost.is_match(v) {
        return format!("http://{v}");
    }
    let domain = regex::Regex::new(r"(?i)^[\w.-]+\.[a-z]{2,}(:\d+)?(/.*)?$").expect("static regex");
    if domain.is_match(v) {
        return format!("https://{v}");
    }
    v.to_string()
}

/// Serialize tests that mutate the process-global bridge/env state.
#[cfg(test)]
pub(crate) static BRIDGE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn teardown() {
        set_emitter(None);
        set_read_terminal_callback(None);
        std::env::remove_var("ULNCLAW_DESKTOP");
    }

    #[test]
    fn emit_without_emitter_is_false() {
        let _guard = BRIDGE_TEST_LOCK.lock().unwrap();
        teardown();
        assert!(!available());
        assert!(!emit("s1", "pane.reveal", &json!({"pane": "chat"})));
    }

    #[test]
    fn emitter_receives_events_with_session() {
        let _guard = BRIDGE_TEST_LOCK.lock().unwrap();
        teardown();
        static COUNT: AtomicUsize = AtomicUsize::new(0);
        let captured = Arc::new(Mutex::new(Vec::<(String, String, Value)>::new()));
        let cap = captured.clone();
        set_emitter(Some(Box::new(move |sid, event, payload| {
            COUNT.fetch_add(1, Ordering::SeqCst);
            cap.lock()
                .unwrap()
                .push((sid.to_string(), event.to_string(), payload.clone()));
        })));
        assert!(available());
        assert!(emit("win-7", "preview.open", &json!({"url": "https://x"})));
        // close_terminal wiring: the registry sink now emits terminal.close
        let result =
            crate::tools::builtin::terminal::request_close_terminal("win-7", "bg-deadbeef");
        assert_eq!(result["status"], json!("ok"));
        let events = captured.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].1, "preview.open");
        assert_eq!(events[1].1, "terminal.close");
        assert_eq!(events[1].2["process_id"], json!("bg-deadbeef"));
        teardown();
        assert!(!available());
        // sink cleared with the emitter
        let result = crate::tools::builtin::terminal::request_close_terminal("win-7", "bg-x");
        assert_eq!(result["status"], json!("error"));
    }

    #[test]
    fn read_terminal_callback_dispatch() {
        let _guard = BRIDGE_TEST_LOCK.lock().unwrap();
        teardown();
        assert!(read_terminal_window(None, None).is_none());
        set_read_terminal_callback(Some(Box::new(|start, _count| {
            Ok(format!(
                "{{\"total_lines\": 100, \"start\": {}, \"text\": \"hello\"}}",
                start.unwrap_or(0)
            ))
        })));
        let out = read_terminal_window(Some(5), None).unwrap().unwrap();
        assert!(out.contains("\"start\": 5"), "got: {out}");
        teardown();
    }

    #[test]
    fn normalize_targets() {
        assert_eq!(
            normalize_preview_target("www.cnn.com"),
            "https://www.cnn.com"
        );
        assert_eq!(
            normalize_preview_target("localhost:3000"),
            "http://localhost:3000"
        );
        assert_eq!(
            normalize_preview_target("127.0.0.1:8080/x"),
            "http://127.0.0.1:8080/x"
        );
        assert_eq!(
            normalize_preview_target("example.org:9000/a/b"),
            "https://example.org:9000/a/b"
        );
        assert_eq!(normalize_preview_target("https://x.y"), "https://x.y");
        assert_eq!(
            normalize_preview_target("./out/report.html"),
            "./out/report.html"
        );
        assert_eq!(normalize_preview_target("~/notes.md"), "~/notes.md");
        assert_eq!(normalize_preview_target("`cnn.com`"), "https://cnn.com");
        assert_eq!(normalize_preview_target("  "), "");
    }

    #[test]
    fn desktop_env_gate() {
        let _guard = BRIDGE_TEST_LOCK.lock().unwrap();
        teardown();
        assert!(!desktop_env_enabled());
        std::env::set_var("ULNCLAW_DESKTOP", "1");
        assert!(desktop_env_enabled());
        std::env::set_var("ULNCLAW_DESKTOP", "off");
        assert!(!desktop_env_enabled());
        teardown();
    }
}

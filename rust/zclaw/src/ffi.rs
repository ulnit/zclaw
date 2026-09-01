//! C FFI (v0.2 — streaming via polling). Exact contract of the shipped
//! libzclaw.so, as consumed by zclaw_napi.cpp on HarmonyOS:
//!
//! - zclaw_init(config_json) -> i32            (0=success, -1=error)
//! - zclaw_chat(message) -> i32                (0=accepted, -1=error, -2=busy)
//! - zclaw_poll_chunks() -> *const c_char      (JSON array of chunks)
//! - zclaw_is_running() -> i32                 (1=running, 0=idle)
//! - zclaw_cancel() -> i32                     (0=success, -1=no chat running)
//! - zclaw_get_sessions() -> *const c_char
//! - zclaw_get_messages(session_id) -> *const c_char
//! - zclaw_free(ptr)
//! - zclaw_version() -> *const c_char          ("0.2.0-mobile")

use crate::agent::dispatcher::{Chunk, Dispatcher};
use crate::config::Config;
use crate::memory::MemoryStore;
use std::ffi::{c_char, CStr, CString};
use std::os::raw::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

struct State {
    runtime: tokio::runtime::Runtime,
    dispatcher: Dispatcher,
    chunk_queue: Mutex<Vec<Chunk>>,
    running: AtomicBool,
    cancelled: Arc<AtomicBool>,
    current_session: Mutex<String>,
}

static STATE: OnceLock<Mutex<Option<State>>> = OnceLock::new();

fn state() -> &'static Mutex<Option<State>> {
    STATE.get_or_init(|| Mutex::new(None))
}

fn to_cstring(s: &str) -> *const c_char {
    CString::new(s).unwrap_or_else(|_| CString::new("").unwrap()).into_raw() as *const c_char
}

/// Parse the wrapper-provided config. Unknown fields (security, memory, ...)
/// are tolerated — serde ignores them by default.
fn parse_config(json: &str) -> anyhow::Result<Config> {
    let mut cfg: Config = serde_json::from_str(json)?;
    if cfg.api_url.is_empty() {
        cfg.api_url = "https://ai.ulnit.com/v1".to_string();
    }
    if cfg.workspace_dir.is_empty() {
        cfg.workspace_dir = ".".to_string();
    }
    Ok(cfg)
}

#[no_mangle]
pub extern "C" fn zclaw_init(config_json: *const c_char) -> i32 {
    if config_json.is_null() { return -1; }
    let json = unsafe { CStr::from_ptr(config_json) }.to_string_lossy().to_string();
    let cfg = match parse_config(&json) {
        Ok(c) => c,
        Err(_) => return -1,
    };

    let memory = match MemoryStore::open(&cfg.workspace_dir) {
        Ok(m) => Arc::new(m),
        Err(_) => return -1,
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .build();
    let runtime = match runtime {
        Ok(r) => r,
        Err(_) => return -1,
    };

    let cancelled = Arc::new(AtomicBool::new(false));
    let dispatcher = Dispatcher::new(cfg, memory, cancelled.clone());

    let st = State {
        runtime,
        dispatcher,
        chunk_queue: Mutex::new(Vec::new()),
        running: AtomicBool::new(false),
        cancelled,
        current_session: Mutex::new(String::new()),
    };

    let guard = state().lock().unwrap();
    drop(guard);
    *state().lock().unwrap() = Some(st);
    0
}

#[no_mangle]
pub extern "C" fn zclaw_chat(message: *const c_char) -> i32 {
    if message.is_null() { return -1; }
    let text = unsafe { CStr::from_ptr(message) }.to_string_lossy().to_string();
    if text.trim().is_empty() { return -1; }

    let guard = state().lock().unwrap();
    let Some(st) = guard.as_ref() else { return -1 };
    if st.running.load(Ordering::SeqCst) { return -2; } // busy

    // session: use the id set via zclaw_set_session, else a rolling default
    let session_id = {
        let mut s = st.current_session.lock().unwrap();
        if s.is_empty() {
            *s = uuid::Uuid::new_v4().to_string();
        }
        s.clone()
    };
    st.dispatcher.memory.touch_session(&session_id, &text, &st.dispatcher.config.default_model);

    st.running.store(true, Ordering::SeqCst);
    st.cancelled.store(false, Ordering::SeqCst);
    st.chunk_queue.lock().unwrap().clear();

    // move what the task needs into owned handles
    let dispatcher_ptr = &st.dispatcher as *const Dispatcher as usize;
    let queue_ptr = &st.chunk_queue as *const Mutex<Vec<Chunk>> as usize;
    let running_ptr = &st.running as *const AtomicBool as usize;

    st.runtime.spawn(async move {
        // SAFETY: State lives in a static Mutex<Option<State>> and is only
        // replaced by a subsequent zclaw_init while no chat is running.
        let dispatcher = unsafe { &*(dispatcher_ptr as *const Dispatcher) };
        let queue = unsafe { &*(queue_ptr as *const Mutex<Vec<Chunk>>) };
        let running = unsafe { &*(running_ptr as *const AtomicBool) };

        let emit = move |chunk: Chunk| {
            queue.lock().unwrap().push(chunk);
        };
        dispatcher.run_turn(&session_id, &text, &emit).await;
        running.store(false, Ordering::SeqCst);
    });

    0
}

/// Extension (v0.2+): select which session the next zclaw_chat runs in.
/// Not present in the original OHOS .so; mobile apps (Android/iOS) need it
/// for multi-session UX. HarmonyOS wrapper keeps its ArkTS-side sessions.
#[no_mangle]
pub extern "C" fn zclaw_set_session(session_id: *const c_char) -> i32 {
    if session_id.is_null() { return -1; }
    let id = unsafe { CStr::from_ptr(session_id) }.to_string_lossy().to_string();
    if id.trim().is_empty() { return -1; }
    let guard = state().lock().unwrap();
    let Some(st) = guard.as_ref() else { return -1 };
    *st.current_session.lock().unwrap() = id;
    0
}

#[no_mangle]
pub extern "C" fn zclaw_poll_chunks() -> *const c_char {
    let guard = state().lock().unwrap();
    let Some(st) = guard.as_ref() else { return to_cstring("[]") };
    let chunks: Vec<Chunk> = std::mem::take(&mut *st.chunk_queue.lock().unwrap());
    let json = serde_json::to_string(&chunks).unwrap_or_else(|_| "[]".to_string());
    to_cstring(&json)
}

#[no_mangle]
pub extern "C" fn zclaw_is_running() -> i32 {
    let guard = state().lock().unwrap();
    match guard.as_ref() {
        Some(st) if st.running.load(Ordering::SeqCst) => 1,
        _ => 0,
    }
}

#[no_mangle]
pub extern "C" fn zclaw_cancel() -> i32 {
    let guard = state().lock().unwrap();
    match guard.as_ref() {
        Some(st) if st.running.load(Ordering::SeqCst) => {
            st.cancelled.store(true, Ordering::SeqCst);
            0
        }
        _ => -1,
    }
}

#[no_mangle]
pub extern "C" fn zclaw_get_sessions() -> *const c_char {
    let guard = state().lock().unwrap();
    let Some(st) = guard.as_ref() else { return to_cstring("[]") };
    let sessions = st.dispatcher.memory.list_sessions();
    let json = serde_json::to_string(&sessions).unwrap_or_else(|_| "[]".to_string());
    to_cstring(&json)
}

#[no_mangle]
pub extern "C" fn zclaw_get_messages(session_id: *const c_char) -> *const c_char {
    if session_id.is_null() { return to_cstring("[]"); }
    let id = unsafe { CStr::from_ptr(session_id) }.to_string_lossy().to_string();
    let guard = state().lock().unwrap();
    let Some(st) = guard.as_ref() else { return to_cstring("[]") };
    let msgs = st.dispatcher.memory.list_messages(&id);
    let json = serde_json::to_string(&msgs).unwrap_or_else(|_| "[]".to_string());
    to_cstring(&json)
}

#[no_mangle]
pub extern "C" fn zclaw_free(ptr: *const c_char) {
    if ptr.is_null() { return; }
    unsafe { drop(CString::from_raw(ptr as *mut c_char)); }
}

#[no_mangle]
pub extern "C" fn zclaw_version() -> *const c_char {
    to_cstring("0.2.0-mobile")
}

// Silence unused-import lint for c_void (kept for ABI clarity).
#[allow(dead_code)]
fn _abi_anchor(_: *mut c_void) {}

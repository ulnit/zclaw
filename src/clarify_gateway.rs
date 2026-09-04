//! Gateway clarify registry — port of hermes `tools/clarify_gateway.py`.
//!
//! Pending clarify prompts raised by the `clarify` tool inside messaging
//! sessions. The tool registers an entry, the platform adapter renders it
//! (WhatsApp interactive buttons, numbered text elsewhere), and the
//! inbound pipeline resolves the waiter either through a button tap
//! (`resolve_gateway_clarify`) or the next plain text message in the
//! session (text intercept, hermes `_maybe_intercept_clarify_text`).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// hermes: state dicts are bounded so stale taps after long uptime cannot
/// grow memory without limit.
const STATE_CAP: usize = 128;

struct ClarifyEntry {
    clarify_id: String,
    session_key: String,
    #[allow(dead_code)]
    question: String,
    choices: Vec<String>,
    #[allow(dead_code)]
    multi_select: bool,
    /// True when the answer arrives as the next plain text message in the
    /// session (open-ended prompts from the start, or after tapping
    /// "Other" on a choice prompt — hermes `mark_awaiting_text`).
    awaiting_text: bool,
    created_at: u64,
    tx: Option<tokio::sync::oneshot::Sender<String>>,
}

fn entries() -> &'static Mutex<HashMap<String, ClarifyEntry>> {
    static ENTRIES: std::sync::OnceLock<Mutex<HashMap<String, ClarifyEntry>>> =
        std::sync::OnceLock::new();
    ENTRIES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Handle returned to the `clarify` tool: the prompt id rendered into
/// button payloads plus the receiver the tool awaits.
pub struct ClarifyHandle {
    pub clarify_id: String,
    pub rx: tokio::sync::oneshot::Receiver<String>,
}

/// Register a pending clarify for a messaging session (hermes
/// `register_gateway_clarify`). Evicts the oldest entry past the cap.
pub fn register(
    session_key: &str,
    question: &str,
    choices: &[String],
    multi_select: bool,
) -> ClarifyHandle {
    let clarify_id = uuid::Uuid::new_v4().simple().to_string()[..12].to_string();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let mut entries = entries().lock().unwrap();
    if entries.len() >= STATE_CAP {
        let oldest = entries
            .values()
            .min_by_key(|e| e.created_at)
            .map(|e| e.clarify_id.clone());
        if let Some(id) = oldest {
            entries.remove(&id);
        }
    }
    entries.insert(
        clarify_id.clone(),
        ClarifyEntry {
            clarify_id: clarify_id.clone(),
            session_key: session_key.to_string(),
            question: question.to_string(),
            choices: choices.to_vec(),
            multi_select,
            // Open-ended prompts capture the next text message directly
            // (hermes send_clarify 0-choices branch).
            awaiting_text: choices.is_empty(),
            created_at: now_secs(),
            tx: Some(tx),
        },
    );
    ClarifyHandle { clarify_id, rx }
}

/// Resolve a pending clarify with the user's answer (hermes
/// `resolve_gateway_clarify`). Returns false when no waiter exists
/// (stale tap / timeout / restart) so the caller can fall back to text
/// dispatch.
pub fn resolve(clarify_id: &str, response: &str) -> bool {
    let tx = {
        let mut entries = entries().lock().unwrap();
        entries.remove(clarify_id).and_then(|e| e.tx)
    };
    match tx {
        Some(tx) => tx.send(response.to_string()).is_ok(),
        None => false,
    }
}

/// Resolve an interactive tap (hermes WhatsApp clarify dispatch): maps
/// the tapped index to the stored choice text; out-of-range taps fall
/// back to the button title, then the raw index string. Returns false
/// when no entry matches (stale tap → caller falls back to text).
pub fn resolve_tap(clarify_id: &str, choice: &str, title: &str) -> bool {
    let response = {
        let entries = entries().lock().unwrap();
        let Some(entry) = entries.get(clarify_id) else {
            return false;
        };
        match choice.parse::<usize>() {
            Ok(idx) if idx < entry.choices.len() => entry.choices[idx].clone(),
            _ => {
                let title = title.trim();
                if title.is_empty() {
                    choice.to_string()
                } else {
                    title.to_string()
                }
            }
        }
    };
    resolve(clarify_id, &response)
}

/// Flip a choice prompt into text-capture mode (hermes
/// `mark_awaiting_text`): the user tapped "Other", so the next plain
/// message in the session resolves the clarify.
pub fn mark_awaiting_text(clarify_id: &str) -> bool {
    let mut entries = entries().lock().unwrap();
    match entries.get_mut(clarify_id) {
        Some(entry) => {
            entry.awaiting_text = true;
            true
        }
        None => false,
    }
}

/// True while an entry is still registered (hermes `_clarify_state`
/// membership check in the Telegram callback handler — distinguishes
/// "already resolved" taps from expired ones).
pub fn contains(clarify_id: &str) -> bool {
    entries().lock().unwrap().contains_key(clarify_id)
}

/// True when the entry is in text-capture mode (tapped "Other"). The
/// Discord prompt-expiry timer skips such prompts — a typed answer can
/// still arrive (see the discord `spawn_prompt_expiry` note).
pub fn is_awaiting_text(clarify_id: &str) -> bool {
    entries()
        .lock()
        .unwrap()
        .get(clarify_id)
        .map(|e| e.awaiting_text)
        .unwrap_or(false)
}

/// Look up a stored choice text without resolving the entry (hermes
/// Telegram callback handler reads `_entries` to map the tapped index
/// to the choice text before calling `resolve_gateway_clarify`).
/// Out-of-range or non-numeric indexes return None.
pub fn peek_choice(clarify_id: &str, idx_str: &str) -> Option<String> {
    let entries = entries().lock().unwrap();
    let entry = entries.get(clarify_id)?;
    let idx = idx_str.parse::<usize>().ok()?;
    entry.choices.get(idx).cloned()
}

/// Snapshot of a session's pending clarify for the text intercept.
#[derive(Debug, Clone)]
pub struct PendingClarify {
    pub clarify_id: String,
    pub session_key: String,
    pub awaiting_text: bool,
}

/// Return the pending clarify for a session, if any (hermes
/// `get_pending_for_session`).
pub fn pending_for_session(session_key: &str) -> Option<PendingClarify> {
    let entries = entries().lock().unwrap();
    entries
        .values()
        .filter(|e| e.session_key == session_key)
        .min_by_key(|e| e.created_at)
        .map(|e| PendingClarify {
            clarify_id: e.clarify_id.clone(),
            session_key: e.session_key.clone(),
            awaiting_text: e.awaiting_text,
        })
}

/// Drop all entries for a session (session end / `/new`).
pub fn clear_session(session_key: &str) {
    let mut entries = entries().lock().unwrap();
    entries.retain(|_, e| e.session_key != session_key);
}

#[cfg(test)]
pub fn reset_for_tests() {
    entries().lock().unwrap().clear();
}

/// Global lock serializing tests that touch the shared registry (tests
/// run in parallel threads).
#[cfg(test)]
pub fn test_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_resolve_roundtrip() {
        let _guard = test_lock().lock().unwrap();
        reset_for_tests();
        let handle = register(
            "platform-whatsapp_cloud-1",
            "Pick",
            &["a".into(), "b".into()],
            false,
        );
        assert!(pending_for_session("platform-whatsapp_cloud-1").is_some());
        assert!(resolve(&handle.clarify_id, "a"));
        // Second resolve: no waiter anymore.
        assert!(!resolve(&handle.clarify_id, "b"));
        assert!(pending_for_session("platform-whatsapp_cloud-1").is_none());
    }

    #[test]
    fn contains_and_peek_choice_mirror_callback_lookup() {
        let _guard = test_lock().lock().unwrap();
        reset_for_tests();
        let handle = register(
            "platform-telegram-1",
            "Pick",
            &["Alpha".into(), "Beta".into()],
            false,
        );
        assert!(contains(&handle.clarify_id));
        assert!(!is_awaiting_text(&handle.clarify_id));
        assert_eq!(
            peek_choice(&handle.clarify_id, "1").as_deref(),
            Some("Beta")
        );
        // Out-of-range and non-numeric lookups miss.
        assert_eq!(peek_choice(&handle.clarify_id, "9"), None);
        assert_eq!(peek_choice(&handle.clarify_id, "other"), None);
        // Other-tap flips text capture on.
        assert!(mark_awaiting_text(&handle.clarify_id));
        assert!(is_awaiting_text(&handle.clarify_id));
        assert!(resolve(&handle.clarify_id, "Beta"));
        // Resolved entry is gone: taps now report "already resolved".
        assert!(!contains(&handle.clarify_id));
        assert!(!is_awaiting_text(&handle.clarify_id));
        assert_eq!(peek_choice(&handle.clarify_id, "0"), None);
    }

    #[test]
    fn open_ended_awaits_text_from_start() {
        let _guard = test_lock().lock().unwrap();
        reset_for_tests();
        let handle = register("sess-1", "Tell me", &[], false);
        let pending = pending_for_session("sess-1").unwrap();
        assert_eq!(pending.clarify_id, handle.clarify_id);
        assert!(pending.awaiting_text);
    }

    #[test]
    fn choice_prompt_needs_other_flip() {
        let _guard = test_lock().lock().unwrap();
        reset_for_tests();
        let handle = register("sess-2", "Pick", &["x".into()], false);
        let pending = pending_for_session("sess-2").unwrap();
        assert!(!pending.awaiting_text);
        assert!(mark_awaiting_text(&handle.clarify_id));
        assert!(pending_for_session("sess-2").unwrap().awaiting_text);
        assert!(!mark_awaiting_text("nope"));
    }

    #[tokio::test]
    async fn resolve_delivers_answer() {
        let _guard = test_lock().lock().unwrap();
        reset_for_tests();
        let handle = register("sess-3", "Pick", &["a".into()], false);
        assert!(resolve(&handle.clarify_id, "the answer"));
        assert_eq!(handle.rx.await.unwrap(), "the answer");
    }

    #[test]
    fn clear_session_drops_entries() {
        let _guard = test_lock().lock().unwrap();
        reset_for_tests();
        register("sess-4", "q", &[], false);
        assert!(pending_for_session("sess-4").is_some());
        clear_session("sess-4");
        assert!(pending_for_session("sess-4").is_none());
    }

    #[test]
    fn cap_evicts_oldest() {
        let _guard = test_lock().lock().unwrap();
        reset_for_tests();
        let mut first = None;
        for i in 0..=STATE_CAP {
            let handle = register(&format!("s{}", i), "q", &[], false);
            if i == 0 {
                first = Some(handle.clarify_id);
            }
        }
        // The very first entry was evicted; the last one is pending.
        assert!(!resolve(&first.unwrap(), "x"));
        assert!(pending_for_session(&format!("s{}", STATE_CAP)).is_some());
    }
}

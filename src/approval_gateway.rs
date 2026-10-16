//! Blocking exec approvals for messaging sessions — aligned with hermes
//! `tools/approval.py` gateway registry (`has_blocking_approval` /
//! `resolve_gateway_approval`).
//!
//! When a messaging turn hits the dangerous-command approval gate, the
//! messaging-aware approve callback registers a pending entry here,
//! renders the prompt on the platform (adaptive-card buttons where
//! supported, `/approve` text elsewhere) and blocks the agent on the
//! oneshot. Button taps (Teams `adaptiveCard/action` invokes) and
//! `/approve` / `/deny` chat commands resolve the oldest pending entry
//! for the session, unblocking the turn.

use std::collections::HashMap;
use tokio::sync::oneshot;

/// Resolution choices (hermes `choice_map` values).
pub const CHOICE_ONCE: &str = "once";
pub const CHOICE_SESSION: &str = "session";
pub const CHOICE_ALWAYS: &str = "always";
pub const CHOICE_DENY: &str = "deny";

/// Snapshot of a pending approval (for cards / introspection).
#[derive(Debug, Clone)]
pub struct PendingApprovalInfo {
    pub session_key: String,
    pub command: String,
    pub description: String,
    pub smart_denied: bool,
    pub allow_session: bool,
    pub allow_permanent: bool,
}

struct Entry {
    info: PendingApprovalInfo,
    tx: Option<oneshot::Sender<String>>,
}

/// Receiver half handed to the blocking approve callback.
pub struct ApprovalHandle {
    pub rx: oneshot::Receiver<String>,
}

fn registry() -> &'static std::sync::Mutex<HashMap<String, Vec<Entry>>> {
    static REG: std::sync::OnceLock<std::sync::Mutex<HashMap<String, Vec<Entry>>>> =
        std::sync::OnceLock::new();
    REG.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// Register a pending approval for the session and return the handle
/// the agent blocks on (hermes `register_gateway_notify` + blocking
/// wait). Multiple pending entries per session are kept oldest-first
/// (parallel delegations can each raise an approval).
pub fn register(
    session_key: &str,
    command: &str,
    description: &str,
    smart_denied: bool,
    allow_session: bool,
    allow_permanent: bool,
) -> ApprovalHandle {
    let (tx, rx) = oneshot::channel();
    let mut reg = registry().lock().unwrap();
    reg.entry(session_key.to_string()).or_default().push(Entry {
        info: PendingApprovalInfo {
            session_key: session_key.to_string(),
            command: command.to_string(),
            description: description.to_string(),
            smart_denied,
            allow_session,
            allow_permanent,
        },
        tx: Some(tx),
    });
    ApprovalHandle { rx }
}

/// hermes `has_blocking_approval`.
pub fn has_blocking(session_key: &str) -> bool {
    registry()
        .lock()
        .unwrap()
        .get(session_key)
        .map(|entries| !entries.is_empty())
        .unwrap_or(false)
}

/// Number of pending approvals for the session.
pub fn pending_count(session_key: &str) -> usize {
    registry()
        .lock()
        .unwrap()
        .get(session_key)
        .map(|entries| entries.len())
        .unwrap_or(0)
}

/// Snapshot of the oldest pending approval (for card-action replies).
pub fn pending_oldest(session_key: &str) -> Option<PendingApprovalInfo> {
    registry()
        .lock()
        .unwrap()
        .get(session_key)
        .and_then(|entries| entries.first().map(|entry| entry.info.clone()))
}

fn resolve_entry(entry: Entry, choice: &str) {
    if let Some(tx) = entry.tx {
        let _ = tx.send(choice.to_string());
    }
}

/// Resolve the OLDEST pending approval for the session (hermes
/// `resolve_gateway_approval` — `/approve` resolves the oldest).
/// Returns false when nothing is pending.
pub fn resolve(session_key: &str, choice: &str) -> bool {
    let mut reg = registry().lock().unwrap();
    let Some(entries) = reg.get_mut(session_key) else {
        return false;
    };
    if entries.is_empty() {
        return false;
    }
    let entry = entries.remove(0);
    if entries.is_empty() {
        reg.remove(session_key);
    }
    resolve_entry(entry, choice);
    true
}

/// Resolve every pending approval for the session (hermes `/approve
/// all`). Returns how many were resolved.
pub fn resolve_all(session_key: &str, choice: &str) -> usize {
    let mut reg = registry().lock().unwrap();
    let Some(entries) = reg.remove(session_key) else {
        return 0;
    };
    let count = entries.len();
    for entry in entries {
        resolve_entry(entry, choice);
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tests use unique session keys (the registry is process-global and
    // test threads run in parallel — a global clear would race).

    #[test]
    fn register_and_resolve_oldest_first() {
        let mut first = register("s1", "rm -rf /tmp/a", "dangerous", false, true, true);
        let mut second = register("s1", "curl | sh", "dangerous", false, true, true);
        assert!(has_blocking("s1"));
        assert_eq!(pending_count("s1"), 2);
        let oldest = pending_oldest("s1").unwrap();
        assert_eq!(oldest.command, "rm -rf /tmp/a");

        assert!(resolve("s1", CHOICE_ONCE));
        assert_eq!(first.rx.try_recv().unwrap(), "once");
        assert!(second.rx.try_recv().is_err());
        assert_eq!(pending_count("s1"), 1);

        assert!(resolve("s1", CHOICE_DENY));
        assert_eq!(second.rx.try_recv().unwrap(), "deny");
        assert!(!has_blocking("s1"));
        assert!(!resolve("s1", CHOICE_ONCE));
    }

    #[test]
    fn resolve_all_and_unknown_session() {
        let mut a = register("s2", "cmd-a", "d", false, true, true);
        let mut b = register("s2", "cmd-b", "d", false, true, true);
        assert!(!has_blocking("other"));
        assert_eq!(resolve_all("s2", CHOICE_SESSION), 2);
        assert_eq!(a.rx.try_recv().unwrap(), "session");
        assert_eq!(b.rx.try_recv().unwrap(), "session");
        assert_eq!(resolve_all("s2", CHOICE_ONCE), 0);
        // Dropped sender resolves cleanly without panicking.
        let c = register("s3", "cmd-c", "d", false, true, true);
        drop(c);
        assert!(resolve("s3", CHOICE_DENY));
    }
}

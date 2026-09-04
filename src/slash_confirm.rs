//! Generic slash-command confirmation primitive (gateway-side) — port of
//! hermes' `tools/slash_confirm.py`.
//!
//! Slash commands with a non-destructive but expensive side effect worth
//! surfacing to the user (currently `/reload-mcp`, which invalidates the
//! provider prompt cache) route through this module.
//!
//! Two delivery paths (hermes):
//! 1. Button UI — adapters that can render inline buttons show
//!    Approve Once / Always Approve / Cancel; the button callback calls
//!    `resolve(session_key, confirm_id, choice)`.
//! 2. Text fallback — adapters without buttons prompt in plain text; users
//!    reply `/approve`, `/always`, or `/cancel`, which the gateway message
//!    intercept maps to `resolve()`.
//!
//! State is module-level (like the approval registry) so platform adapters
//! can resolve callbacks without a backreference to the gateway runner.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

/// Default timeout — a pending confirm older than this is discarded when
/// resolved or when `clear_if_stale` runs (hermes `DEFAULT_TIMEOUT_SECONDS`).
pub const DEFAULT_TIMEOUT_SECONDS: u64 = 300;

/// Confirmation choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmChoice {
    Once,
    Always,
    Cancel,
}

impl ConfirmChoice {
    /// Parse the wire values hermes uses (`"once"` / `"always"` /
    /// `"cancel"`).
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_lowercase().as_str() {
            "once" => Some(ConfirmChoice::Once),
            "always" => Some(ConfirmChoice::Always),
            "cancel" => Some(ConfirmChoice::Cancel),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            ConfirmChoice::Once => "once",
            ConfirmChoice::Always => "always",
            ConfirmChoice::Cancel => "cancel",
        }
    }
}

/// Handler run when a confirm resolves: receives the choice, returns an
/// optional follow-up message for the platform.
pub type ConfirmHandler = Box<
    dyn Fn(ConfirmChoice) -> Pin<Box<dyn Future<Output = Option<String>> + Send>> + Send + Sync,
>;

struct PendingEntry {
    confirm_id: String,
    command: String,
    handler: ConfirmHandler,
    created_at: SystemTime,
}

/// Pending confirmations keyed by gateway session key (hermes `_pending`).
static PENDING: LazyLock<Mutex<HashMap<String, PendingEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn pending() -> MutexGuard<'static, HashMap<String, PendingEntry>> {
    PENDING.lock().unwrap_or_else(|e| e.into_inner())
}

/// Register a pending slash-command confirmation. Overwrites any prior
/// pending confirm for the same session — a new confirmable command
/// supersedes the stale one (hermes `register`).
pub fn register(session_key: &str, confirm_id: &str, command: &str, handler: ConfirmHandler) {
    pending().insert(
        session_key.to_string(),
        PendingEntry {
            confirm_id: confirm_id.to_string(),
            command: command.to_string(),
            handler,
            created_at: SystemTime::now(),
        },
    );
}

/// Summary of the pending confirm for a session, if any (hermes
/// `get_pending` — returns a copy so callers don't hold the lock).
pub fn get_pending(session_key: &str) -> Option<PendingInfo> {
    pending().get(session_key).map(|entry| PendingInfo {
        confirm_id: entry.confirm_id.clone(),
        command: entry.command.clone(),
        created_at: entry.created_at,
    })
}

/// Copy of the pending-confirm metadata (no handler).
#[derive(Debug, Clone)]
pub struct PendingInfo {
    pub confirm_id: String,
    pub command: String,
    pub created_at: SystemTime,
}

/// Drop the pending confirm without running it (hermes `clear`).
pub fn clear(session_key: &str) {
    pending().remove(session_key);
}

/// Drop the pending confirm if older than `timeout` (hermes
/// `clear_if_stale`). Returns true when an entry was dropped.
pub fn clear_if_stale(session_key: &str, timeout: Duration) -> bool {
    let mut map = pending();
    let Some(entry) = map.get(session_key) else {
        return false;
    };
    let age = entry.created_at.elapsed().unwrap_or(Duration::ZERO);
    if age > timeout {
        map.remove(session_key);
        return true;
    }
    false
}

/// Resolve a pending confirm (hermes `resolve`).
///
/// Returns the handler's follow-up message, or `None` when the confirm was
/// stale, already resolved, or the `confirm_id` doesn't match (superseded
/// by a newer prompt on the same session). The entry is popped BEFORE the
/// handler runs so duplicate callbacks (button double-clicks) cannot run
/// it twice.
pub async fn resolve(
    session_key: &str,
    confirm_id: &str,
    choice: ConfirmChoice,
    timeout: Duration,
) -> Option<String> {
    let entry = {
        let mut map = pending();
        // A mismatched confirm_id means a NEWER prompt superseded this one
        // — return None WITHOUT popping so the fresh entry stays pending
        // (hermes `resolve`).
        if map.get(session_key).map(|e| e.confirm_id != confirm_id) == Some(true) {
            return None;
        }
        let entry = map.remove(session_key)?;
        let age = entry.created_at.elapsed().unwrap_or(Duration::ZERO);
        if age > timeout {
            return None;
        }
        entry
    };
    let result = (entry.handler)(choice).await;
    result
}

/// Drop ALL pending confirms — tests only.
#[cfg(test)]
pub fn clear_all_for_tests() {
    pending().clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// The pending-confirmation registry is process-global; serialize the
    /// tests that mutate it (parallel clear_all/register otherwise race).
    fn confirm_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn echo_handler(counter: Arc<AtomicUsize>) -> ConfirmHandler {
        Box::new(move |choice| {
            let counter = counter.clone();
            Box::pin(async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Some(format!("handled:{}", choice.as_str()))
            })
        })
    }

    #[tokio::test]
    async fn register_and_resolve_once() {
        let _guard = confirm_test_lock();
        clear_all_for_tests();
        let counter = Arc::new(AtomicUsize::new(0));
        register("sess1", "c1", "reload-mcp", echo_handler(counter.clone()));

        let info = get_pending("sess1").expect("pending registered");
        assert_eq!(info.confirm_id, "c1");
        assert_eq!(info.command, "reload-mcp");

        let out = resolve("sess1", "c1", ConfirmChoice::Once, Duration::from_secs(300))
            .await
            .unwrap();
        assert_eq!(out, "handled:once");
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        assert!(get_pending("sess1").is_none(), "entry popped on resolve");
    }

    #[tokio::test]
    async fn double_resolve_runs_handler_once() {
        let _guard = confirm_test_lock();
        clear_all_for_tests();
        let counter = Arc::new(AtomicUsize::new(0));
        register("sess2", "c1", "reload-mcp", echo_handler(counter.clone()));
        let first = resolve("sess2", "c1", ConfirmChoice::Once, Duration::from_secs(300)).await;
        let second = resolve("sess2", "c1", ConfirmChoice::Once, Duration::from_secs(300)).await;
        assert!(first.is_some());
        assert!(second.is_none(), "second resolve must miss");
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn mismatched_confirm_id_is_ignored() {
        let _guard = confirm_test_lock();
        clear_all_for_tests();
        let counter = Arc::new(AtomicUsize::new(0));
        register(
            "sess3",
            "newer",
            "reload-mcp",
            echo_handler(counter.clone()),
        );
        // Stale button from a superseded prompt.
        let out = resolve(
            "sess3",
            "older",
            ConfirmChoice::Once,
            Duration::from_secs(300),
        )
        .await;
        assert!(out.is_none());
        assert_eq!(counter.load(Ordering::SeqCst), 0);
        // But the mismatch consumed nothing? hermes returns None WITHOUT
        // popping on id mismatch — the fresh entry stays pending.
        assert!(get_pending("sess3").is_some());
    }

    #[tokio::test]
    async fn stale_entry_is_rejected() {
        let _guard = confirm_test_lock();
        clear_all_for_tests();
        let counter = Arc::new(AtomicUsize::new(0));
        register("sess4", "c1", "reload-mcp", echo_handler(counter.clone()));
        let out = resolve("sess4", "c1", ConfirmChoice::Cancel, Duration::from_secs(0)).await;
        assert!(out.is_none(), "zero timeout = immediately stale");
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn newer_registration_supersedes() {
        let _guard = confirm_test_lock();
        clear_all_for_tests();
        let counter = Arc::new(AtomicUsize::new(0));
        register("sess5", "c1", "reload-mcp", echo_handler(counter.clone()));
        register("sess5", "c2", "reload-mcp", echo_handler(counter.clone()));
        let info = get_pending("sess5").unwrap();
        assert_eq!(info.confirm_id, "c2");
        let out = resolve(
            "sess5",
            "c2",
            ConfirmChoice::Always,
            Duration::from_secs(300),
        )
        .await
        .unwrap();
        assert_eq!(out, "handled:always");
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn clear_and_clear_if_stale() {
        let _guard = confirm_test_lock();
        clear_all_for_tests();
        let counter = Arc::new(AtomicUsize::new(0));
        register("sess6", "c1", "reload-mcp", echo_handler(counter.clone()));
        clear("sess6");
        assert!(get_pending("sess6").is_none());

        register("sess6", "c1", "reload-mcp", echo_handler(counter.clone()));
        assert!(!clear_if_stale("sess6", Duration::from_secs(300)));
        assert!(get_pending("sess6").is_some());
        assert!(clear_if_stale("sess6", Duration::from_secs(0)));
        assert!(get_pending("sess6").is_none());
    }

    #[test]
    fn choice_parsing() {
        assert_eq!(ConfirmChoice::parse("once"), Some(ConfirmChoice::Once));
        assert_eq!(
            ConfirmChoice::parse(" ALWAYS "),
            Some(ConfirmChoice::Always)
        );
        assert_eq!(ConfirmChoice::parse("cancel"), Some(ConfirmChoice::Cancel));
        assert_eq!(ConfirmChoice::parse("maybe"), None);
    }
}

//! Shared session activity observation contract — port of hermes
//! `agent/session_activity.py` (#72016 / #72039).
//!
//! Observation-only: timestamp + bounded description + provenance.
//! Notification, timeout, kill, and retry policy stay in their own
//! components — [`crate::session_stall`] owns the stall notify-once
//! policy. Consumers distinguish work (API / tool / streaming /
//! stalled) from the description text itself; there is no separate
//! phase enum.
//!
//! Provenance is a small set of *noun* sources (where the stamp came
//! from). Generic turn-boundary stamps use `gateway.turn`; mid-turn
//! agent progress stamps `agent.progress`; the default clock stamps
//! `unknown` unless a caller passes an explicit provenance.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use serde::Serialize;

/// Shared budget for free-form activity text (hermes
/// `ACTIVITY_DESCRIPTION_MAX`).
pub const ACTIVITY_DESCRIPTION_MAX: usize = 120;

/// Clamp free-form activity text to the shared description budget.
pub fn bound_activity_description(description: &str) -> String {
    let text = description.trim();
    if text.chars().count() <= ACTIVITY_DESCRIPTION_MAX {
        return text.to_string();
    }
    let truncated: String = text.chars().take(ACTIVITY_DESCRIPTION_MAX - 1).collect();
    format!("{truncated}\u{2026}")
}

/// One observation snapshot (hermes `build_activity_snapshot`).
#[derive(Debug, Clone, Serialize)]
pub struct ActivitySnapshot {
    pub last_activity_at: f64,
    pub description: String,
    pub provenance: String,
}

fn registry() -> &'static Mutex<HashMap<String, ActivitySnapshot>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, ActivitySnapshot>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn now_epoch() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Stamp activity for a session/chat key (hermes `_touch_activity`).
pub fn touch(key: &str, description: &str, provenance: &str) {
    touch_at(key, description, provenance, now_epoch());
}

/// Deterministic-clock variant for tests.
pub fn touch_at(key: &str, description: &str, provenance: &str, now: f64) {
    let snapshot = ActivitySnapshot {
        last_activity_at: now,
        description: bound_activity_description(description),
        provenance: if provenance.trim().is_empty() {
            "unknown".to_string()
        } else {
            provenance.trim().to_string()
        },
    };
    registry().lock().unwrap().insert(key.to_string(), snapshot);
}

/// Latest snapshot for a key, if any.
pub fn snapshot(key: &str) -> Option<ActivitySnapshot> {
    registry().lock().unwrap().get(key).cloned()
}

/// Seconds since the last activity stamp, or `None` when the key has
/// no usable stamp — callers must not fall back to turn-start or
/// pending-inbound clocks (hermes #72039 single progress source).
pub fn seconds_since(key: &str, now: f64) -> Option<f64> {
    let at = registry()
        .lock()
        .unwrap()
        .get(key)
        .map(|s| s.last_activity_at)?;
    if !at.is_finite() {
        return None;
    }
    Some((now - at).max(0.0))
}

/// Drop a key's stamps (session teardown / tests).
pub fn remove(key: &str) {
    registry().lock().unwrap().remove(key);
}

/// Every live stamp, newest first (watcher + diagnostics).
pub fn snapshot_all() -> Vec<(String, ActivitySnapshot)> {
    let mut rows: Vec<(String, ActivitySnapshot)> = registry()
        .lock()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    rows.sort_by(|a, b| {
        b.1.last_activity_at
            .partial_cmp(&a.1.last_activity_at)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    rows
}

/// Test-only full clear.
#[doc(hidden)]
pub fn clear_for_tests() {
    registry().lock().unwrap().clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_descriptions_to_the_shared_budget() {
        assert_eq!(
            bound_activity_description("  tool call: read_file  "),
            "tool call: read_file"
        );
        let long = "x".repeat(300);
        let bounded = bound_activity_description(&long);
        assert_eq!(bounded.chars().count(), ACTIVITY_DESCRIPTION_MAX);
        assert!(bounded.ends_with('\u{2026}'));
    }

    #[test]
    fn touch_and_snapshot_roundtrip() {
        let _guard = crate::models_dev::test_env_lock();
        clear_for_tests();
        assert!(snapshot("platform-test-chat-1").is_none());
        touch_at(
            "platform-test-chat-1",
            "turn started",
            "gateway.turn",
            1000.0,
        );
        let snap = snapshot("platform-test-chat-1").unwrap();
        assert_eq!(snap.last_activity_at, 1000.0);
        assert_eq!(snap.description, "turn started");
        assert_eq!(snap.provenance, "gateway.turn");
        assert_eq!(seconds_since("platform-test-chat-1", 1042.5), Some(42.5));
        // Future stamps clamp at zero; unknown keys stay unknown.
        assert_eq!(seconds_since("platform-test-chat-1", 999.0), Some(0.0));
        assert_eq!(seconds_since("platform-test-ghost", 1000.0), None);
        // Blank provenance normalizes to unknown.
        touch_at("platform-test-chat-1", "x", "  ", 1001.0);
        assert_eq!(
            snapshot("platform-test-chat-1").unwrap().provenance,
            "unknown"
        );
        remove("platform-test-chat-1");
        assert!(snapshot("platform-test-chat-1").is_none());
        clear_for_tests();
    }

    #[test]
    fn snapshot_all_sorts_newest_first() {
        let _guard = crate::models_dev::test_env_lock();
        clear_for_tests();
        touch_at("k-old", "a", "unknown", 100.0);
        touch_at("k-new", "b", "unknown", 300.0);
        touch_at("k-mid", "c", "unknown", 200.0);
        let keys: Vec<String> = snapshot_all().into_iter().map(|(k, _)| k).collect();
        assert_eq!(keys, vec!["k-new", "k-mid", "k-old"]);
        clear_for_tests();
    }
}

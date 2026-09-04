//! Restart-loop circuit breaker (hermes `gateway/restart_loop_guard.py`
//! parity, defense-3 of #30719).
//!
//! A stale pidfile at boot means the previous gateway died without a
//! clean shutdown (crash / SIGKILL / host reboot). Each such
//! crash-restart boot is recorded in `<home>/gateway/restart_loop.json`;
//! once too many crash boots land inside a short window the loop is
//! "tripped" so operators (and the health surface) can see the gateway
//! is crash-looping instead of guessing.
//!
//! State is intentionally tiny and best-effort: any read/write failure
//! fails OPEN (no false trip), because a broken breaker must never wedge
//! a healthy gateway.

use serde_json::json;
use std::path::{Path, PathBuf};

/// A legitimate operator restart (or two) never trips the breaker, but a
/// tight supervisor respawn loop does within a few cycles (hermes
/// defaults).
pub const DEFAULT_MAX_RESTARTS: usize = 3;
pub const DEFAULT_WINDOW_SECONDS: u64 = 60;

/// Persisted boot log location (profile-scoped under the home dir).
pub fn state_path(home: &Path) -> PathBuf {
    home.join("gateway").join("restart_loop.json")
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn load_boots(home: &Path) -> Vec<f64> {
    let Ok(raw) = std::fs::read_to_string(state_path(home)) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return Vec::new();
    };
    value
        .get("boots")
        .and_then(|b| b.as_array())
        .map(|boots| boots.iter().filter_map(|t| t.as_f64()).collect())
        .unwrap_or_default()
}

fn save_boots(home: &Path, boots: &[f64]) {
    let path = state_path(home);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, json!({ "boots": boots }).to_string());
}

/// Record a crash-restart boot: prune boots older than the window,
/// append the current time, persist (best-effort), and return the
/// pruned+appended list (most recent last).
pub fn record_crash_boot(home: &Path, window_seconds: u64, now: Option<f64>) -> Vec<f64> {
    let ts = now.unwrap_or_else(now_secs);
    let cutoff = ts - window_seconds.max(1) as f64;
    let mut boots: Vec<f64> = load_boots(home)
        .into_iter()
        .filter(|t| *t >= cutoff)
        .collect();
    boots.push(ts);
    save_boots(home, &boots);
    boots
}

/// True when at least `max_restarts` crash boots sit inside the window.
/// Fails OPEN on any read problem — a broken breaker never wedges a
/// healthy gateway.
pub fn is_restart_loop_tripped(
    home: &Path,
    max_restarts: usize,
    window_seconds: u64,
    now: Option<f64>,
) -> bool {
    if max_restarts == 0 {
        return false;
    }
    let ts = now.unwrap_or_else(now_secs);
    let cutoff = ts - window_seconds.max(1) as f64;
    load_boots(home).iter().filter(|t| **t >= cutoff).count() >= max_restarts
}

/// The single entry point the gateway boot path calls: record this
/// crash-restart boot, then report whether the loop is now tripped.
pub fn check_and_record(home: &Path, now: Option<f64>) -> bool {
    let boots = record_crash_boot(home, DEFAULT_WINDOW_SECONDS, now);
    let tripped = boots.len() >= DEFAULT_MAX_RESTARTS;
    if tripped {
        tracing::warn!(
            "restart-loop breaker TRIPPED: {} crash-restart gateway boots within {}s \
             (threshold {}). The previous instance(s) died without a clean shutdown; \
             check the logs for the crash cause. Delete {} to reset the breaker.",
            boots.len(),
            DEFAULT_WINDOW_SECONDS,
            DEFAULT_MAX_RESTARTS,
            state_path(home).display(),
        );
    }
    tripped
}

/// Remove the persisted boot log (clean shutdown / tests).
pub fn clear(home: &Path) {
    let _ = std::fs::remove_file(state_path(home));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_prunes_outside_window() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let boots = record_crash_boot(home, 60, Some(1000.0));
        assert_eq!(boots, vec![1000.0]);
        // Second boot inside the window keeps both.
        let boots = record_crash_boot(home, 60, Some(1030.0));
        assert_eq!(boots, vec![1000.0, 1030.0]);
        // A boot past the window prunes the first entry.
        let boots = record_crash_boot(home, 60, Some(1070.0));
        assert_eq!(boots, vec![1030.0, 1070.0]);
    }

    #[test]
    fn trips_at_threshold_and_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        assert!(!check_and_record(home, Some(1000.0)));
        assert!(!check_and_record(home, Some(1010.0)));
        assert!(check_and_record(home, Some(1020.0)));
        assert!(is_restart_loop_tripped(home, 3, 60, Some(1030.0)));
        // Once the boots age out of the window the breaker recovers.
        assert!(!is_restart_loop_tripped(home, 3, 60, Some(1090.0)));
    }

    #[test]
    fn clear_resets_state() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        check_and_record(home, Some(1000.0));
        assert!(state_path(home).exists());
        clear(home);
        assert!(!state_path(home).exists());
        assert!(!is_restart_loop_tripped(home, 1, 60, Some(1001.0)));
    }

    #[test]
    fn corrupt_state_fails_open() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        std::fs::create_dir_all(home.join("gateway")).unwrap();
        std::fs::write(state_path(home), "{ not json").unwrap();
        assert!(!is_restart_loop_tripped(home, 1, 60, None));
        // Recording still works over the corrupt file.
        let boots = record_crash_boot(home, 60, Some(2000.0));
        assert_eq!(boots, vec![2000.0]);
    }
}

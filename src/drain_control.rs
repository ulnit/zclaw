//! External drain-control marker contract (dashboard → gateway) — port
//! of hermes `gateway/drain_control.py`.
//!
//! The dashboard has no way to call into a running gateway — there is
//! no HTTP control channel into the gateway process (guardrails:
//! "there is NO external control channel into a running gateway").
//! Restart/drain is driven only by the gateway reacting to its own
//! inputs: slash commands, process signals, and file markers it writes
//! itself.
//!
//! So a begin/cancel-drain dashboard surface communicates with the
//! running gateway the same way: it writes (or removes) a marker file,
//! and a gateway background watcher reacts to it. This module owns
//! that marker contract so both sides — the writer and the gateway
//! watcher (reader) — share one definition and can never disagree.
//!
//! Contract (presence-based):
//!
//! * begin-drain  → write `<home>/.drain_request.json` with
//!   `{"action": "drain", "requested_at": <iso>, "principal": <str>,
//!   "epoch": <instantiation-epoch>, "suppress_notification": <bool>}`.
//! * cancel-drain → remove the marker.
//! * The gateway watcher treats **presence of a marker stamped with
//!   the current instantiation epoch** as "external drain active":
//!   flip the drain flag and stop accepting new turns. Absence (or a
//!   marker from a *prior* instantiation) means "not draining"
//!   (revert to running if the watcher had flipped it).
//!
//! Why the epoch: the ulnclaw home is a **durable** store. A
//! begin-drain marker written there *survives a machine restart*. But
//! the disruptive lifecycle actions a drain protects all restart the
//! machine, which is exactly the signal that the drain is over.
//! Without the epoch, a freshly-restarted gateway re-reads the
//! orphaned marker on boot and parks itself right back in draining
//! forever. Stamping the marker with an identity of *this*
//! instantiation, and ignoring a marker whose epoch doesn't match,
//! makes "a deliberate restart clears the drain" true by construction
//! — while a marker written during the *current* instantiation still
//! matches.
//!
//! Reading the marker never panics: a malformed/half-written file
//! reads as "present but contentless", which the watcher still treats
//! as drain-active (fail-safe toward quiescing — a corrupt begin
//! marker must not be ignored). The epoch check is deliberately
//! **lenient**: it ignores a marker only on a *definite* epoch
//! mismatch. A marker with no epoch (legacy/corrupt/contentless), or
//! an environment where the epoch cannot be computed (non-Linux, no
//! `/proc`), both degrade to the original presence-only behaviour —
//! never fail-closed.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde_json::{json, Value};

const DRAIN_REQUEST_FILENAME: &str = ".drain_request.json";

/// Default drain-marker watcher cadence in seconds.
pub const WATCHER_INTERVAL_SECONDS: u64 = 2;

/// Identity of THIS container/VM instantiation (hermes
/// `current_instantiation_epoch`).
///
/// Stable for the life of the PID-1 init process — so a respawn of
/// just the gateway keeps the same epoch and an in-flight drain is
/// honoured — but changes when the machine/container is recreated.
/// Composed from two `/proc` facts:
///
/// * the kernel **boot id** (`/proc/sys/kernel/random/boot_id`) —
///   changes on a VM/microVM reboot;
/// * **PID 1's start time** (field 22 of `/proc/1/stat`) — changes on
///   a plain container restart (the host kernel, hence boot_id, is
///   unchanged, but `/init` is a brand-new process).
///
/// Returns `""` when neither identity source is readable (non-Linux,
/// no `/proc`). An empty epoch disables the staleness check
/// downstream, degrading to presence-only behaviour — never
/// fail-closed. Memoised: the epoch is constant for the life of the
/// process.
pub fn current_instantiation_epoch() -> &'static str {
    static EPOCH: OnceLock<String> = OnceLock::new();
    EPOCH.get_or_init(|| {
        let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let mut pid1_start = String::new();
        if let Ok(stat) = std::fs::read_to_string("/proc/1/stat") {
            // /proc/1/stat: "<pid> (<comm>) <state> ... <starttime@field22>".
            // comm can contain spaces and parens, so split on the LAST
            // ')' and index into the whitespace-delimited tail.
            // starttime is field 22 (1-indexed); after the comm the
            // tail starts at field 3, so it is the tail's index 19.
            if let Some(tail) = stat.rsplit(')').next() {
                let fields: Vec<&str> = tail.split_whitespace().collect();
                if let Some(start) = fields.get(19) {
                    pid1_start = start.to_string();
                }
            }
        }
        if boot_id.is_empty() && pid1_start.is_empty() {
            String::new()
        } else {
            format!("{boot_id}:{pid1_start}")
        }
    })
}

/// Absolute path to the drain-request marker, respecting the ulnclaw
/// home (hermes `drain_request_path`).
pub fn drain_request_path(home: Option<&Path>) -> PathBuf {
    let base = home
        .map(PathBuf::from)
        .unwrap_or_else(crate::config::ulnclaw_home);
    base.join(DRAIN_REQUEST_FILENAME)
}

/// Write the begin-drain marker; returns the payload written (hermes
/// `write_drain_request`).
///
/// Atomic write so the gateway watcher never reads a half-written
/// file. Idempotent: re-writing while a drain is already in progress
/// just refreshes `requested_at` (harmless — the watcher keys off
/// presence, not content).
///
/// Stamps the marker with [`current_instantiation_epoch`] so a marker
/// that later survives a machine restart can be recognised as stale
/// and ignored.
///
/// `suppress_notification` is a generic "be quiet on the shutdown
/// that ends this drain" flag: the gateway's shutdown path reads it
/// via [`drain_notification_suppressed`] and skips the home-channel
/// "gateway shutting down" broadcast. It defaults false so
/// operator drains behave exactly as before.
pub fn write_drain_request(
    principal: &str,
    suppress_notification: bool,
    home: Option<&Path>,
) -> Result<Value, String> {
    let payload = json!({
        "action": "drain",
        "requested_at": chrono::Utc::now().to_rfc3339(),
        "principal": principal,
        "epoch": current_instantiation_epoch(),
        "suppress_notification": suppress_notification,
    });
    atomic_json_write(&drain_request_path(home), &payload)?;
    Ok(payload)
}

/// Remove the drain marker (cancel-drain); true if one existed (hermes
/// `clear_drain_request`). Best-effort: a missing file is not an
/// error (cancel is idempotent).
pub fn clear_drain_request(home: Option<&Path>) -> bool {
    match std::fs::remove_file(drain_request_path(home)) {
        Ok(()) => true,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(err) => {
            tracing::warn!("drain-control: failed to remove marker: {err}");
            false
        }
    }
}

/// Return the marker payload, or `None` if absent (hermes
/// `read_drain_request`).
///
/// A present-but-unparseable marker returns `{}` (truthy-presence
/// preserved via [`drain_requested`]; callers that need the body get
/// an empty object rather than an error). Never fails.
pub fn read_drain_request(home: Option<&Path>) -> Option<Value> {
    let raw = match std::fs::read_to_string(drain_request_path(home)) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Err(err) => {
            tracing::warn!("drain-control: failed to read marker: {err}");
            return None;
        }
    };
    match serde_json::from_str::<Value>(&raw) {
        Ok(value) if value.is_object() => Some(value),
        _ => Some(json!({})),
    }
}

/// True iff `body`'s epoch is a *definite* mismatch with this process
/// (hermes `_marker_epoch_is_stale`).
///
/// Lenient by design — returns false (i.e. "not stale, honour it")
/// whenever it can't be sure: the current epoch can't be computed
/// ("" fallback, no /proc), OR the marker carries no epoch (legacy
/// marker, or a corrupt/contentless body). Only a marker whose epoch
/// is present AND differs from the current instantiation epoch is
/// considered stale.
/// Public staleness probe for ops surfaces (P728 `/api/drain`).
pub fn marker_is_stale(body: &Value) -> bool {
    marker_epoch_is_stale(body)
}

fn marker_epoch_is_stale(body: &Value) -> bool {
    let current = current_instantiation_epoch();
    if current.is_empty() {
        return false;
    }
    let Some(marker_epoch) = body.get("epoch").and_then(Value::as_str) else {
        return false;
    };
    if marker_epoch.is_empty() {
        return false;
    }
    marker_epoch != current
}

/// True iff a begin-drain marker for THIS instantiation is present
/// (hermes `drain_requested`).
///
/// A marker whose `epoch` does not match the current instantiation
/// epoch is treated as absent: it survived a restart and the
/// lifecycle action that triggered the drain has already completed —
/// honouring it would wedge the freshly-restarted gateway in
/// draining.
pub fn drain_requested(home: Option<&Path>) -> bool {
    let Some(body) = read_drain_request(home) else {
        return false;
    };
    !marker_epoch_is_stale(&body)
}

/// True iff an ACTIVE drain marker asks to suppress the shutdown
/// broadcast (hermes `drain_notification_suppressed`).
///
/// "Active" means exactly what [`drain_requested`] means — a stale
/// (other-epoch) marker is ignored here just as it is for drain
/// state: an orphaned marker's flag must never silence a *fresh*
/// gateway's legitimate shutdown broadcast.
///
/// Only honours the flag when it is explicitly truthy in the marker
/// body. A legacy marker without the field, a corrupt/contentless
/// body, or an absent marker all read as "not suppressed" — fail
/// toward the louder, more-visible behaviour.
pub fn drain_notification_suppressed(home: Option<&Path>) -> bool {
    let Some(body) = read_drain_request(home) else {
        return false;
    };
    if marker_epoch_is_stale(&body) {
        return false;
    }
    body.get("suppress_notification")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn atomic_json_write(path: &Path, value: &Value) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    std::fs::write(
        &tmp,
        serde_json::to_string_pretty(value).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).ok();
    }
    Ok(())
}

/// Background watcher: presence of `.drain_request.json` stamped with
/// the current instantiation epoch flips the gateway drain flag;
/// removal reverts it when the watcher had flipped it (hermes
/// drain-watcher contract — "revert to running if we had flipped
/// it"). New runs are refused while the flag is set.
pub async fn run_drain_watcher(
    state: std::sync::Arc<crate::gateway::GatewayState>,
    interval: std::time::Duration,
) {
    let mut flipped_by_marker = false;
    loop {
        tokio::time::sleep(interval).await;
        if drain_requested(None) {
            if !state.restart.load(std::sync::atomic::Ordering::SeqCst) {
                tracing::warn!("[drain] external drain marker active — refusing new runs");
                state
                    .restart
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
            flipped_by_marker = true;
        } else if flipped_by_marker {
            if state.restart.load(std::sync::atomic::Ordering::SeqCst) {
                tracing::info!("[drain] drain marker removed — accepting runs again");
                state
                    .restart
                    .store(false, std::sync::atomic::Ordering::SeqCst);
            }
            flipped_by_marker = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_is_stable_across_calls() {
        let first = current_instantiation_epoch();
        let second = current_instantiation_epoch();
        assert_eq!(first, second);
        // On Linux CI/dev hosts both /proc facts are readable.
        if cfg!(target_os = "linux") {
            assert!(!first.is_empty());
            assert!(first.contains(':'));
        }
    }

    #[test]
    fn marker_roundtrip_and_clear() {
        let _lock = crate::models_dev::test_env_lock();
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();

        assert!(read_drain_request(Some(home)).is_none());
        assert!(!drain_requested(Some(home)));
        assert!(!clear_drain_request(Some(home)));

        let payload = write_drain_request("test-principal", false, Some(home)).unwrap();
        assert_eq!(payload["action"], "drain");
        assert_eq!(payload["principal"], "test-principal");
        assert_eq!(payload["epoch"], current_instantiation_epoch());
        assert_eq!(payload["suppress_notification"], false);
        assert!(payload["requested_at"].as_str().unwrap().contains('T'));

        assert!(drain_requested(Some(home)));
        assert!(!drain_notification_suppressed(Some(home)));

        // Idempotent re-write refreshes the payload.
        let payload2 = write_drain_request("test-principal", true, Some(home)).unwrap();
        assert_eq!(payload2["suppress_notification"], true);
        assert!(drain_notification_suppressed(Some(home)));

        assert!(clear_drain_request(Some(home)));
        assert!(!drain_requested(Some(home)));
        assert!(!drain_notification_suppressed(Some(home)));
    }

    #[test]
    fn corrupt_marker_fails_safe_toward_drain() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        std::fs::write(drain_request_path(Some(home)), "not json").unwrap();
        // Present-but-unparseable reads as {} …
        assert_eq!(read_drain_request(Some(home)), Some(json!({})));
        // … which is still drain-active (fail-safe toward quiescing) …
        assert!(drain_requested(Some(home)));
        // … but never suppresses the shutdown broadcast.
        assert!(!drain_notification_suppressed(Some(home)));
    }

    #[test]
    fn stale_epoch_marker_is_ignored() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        if current_instantiation_epoch().is_empty() {
            return; // non-/proc environment: presence-only semantics.
        }
        std::fs::write(
            drain_request_path(Some(home)),
            r#"{"action": "drain", "epoch": "bogus-boot:999"}"#,
        )
        .unwrap();
        assert!(!drain_requested(Some(home)));
        assert!(!drain_notification_suppressed(Some(home)));
        // Legacy marker with no epoch is honoured (lenient check).
        std::fs::write(drain_request_path(Some(home)), r#"{"action": "drain"}"#).unwrap();
        assert!(drain_requested(Some(home)));
    }

    #[tokio::test]
    async fn watcher_flips_drain_flag_with_marker() {
        let _lock = crate::models_dev::test_env_lock();
        let temp = tempfile::tempdir().unwrap();
        let prev = std::env::var("ULNCLAW_HOME").ok();
        std::env::set_var("ULNCLAW_HOME", temp.path());

        let state = test_gateway_state();
        assert!(!state.restart.load(std::sync::atomic::Ordering::SeqCst));

        let watcher = tokio::spawn(run_drain_watcher(
            state.clone(),
            std::time::Duration::from_millis(25),
        ));

        // Marker appears → drain flag flips.
        write_drain_request("watcher-test", false, None).unwrap();
        wait_for_flag(&state, true).await;
        assert!(state.restart.load(std::sync::atomic::Ordering::SeqCst));

        // Marker removed → flag reverts (watcher had flipped it).
        clear_drain_request(None);
        wait_for_flag(&state, false).await;
        assert!(!state.restart.load(std::sync::atomic::Ordering::SeqCst));

        watcher.abort();
        match prev {
            Some(home) => std::env::set_var("ULNCLAW_HOME", home),
            None => std::env::remove_var("ULNCLAW_HOME"),
        }
    }

    async fn wait_for_flag(state: &crate::gateway::GatewayState, expected: bool) {
        for _ in 0..200 {
            if state.restart.load(std::sync::atomic::Ordering::SeqCst) == expected {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("drain flag never reached {expected}");
    }

    fn test_gateway_state() -> std::sync::Arc<crate::gateway::GatewayState> {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = std::sync::Arc::new(
            crate::SqliteSessionStore::open(temp.path().join("state.db")).expect("store opens"),
        );
        std::mem::forget(temp);
        let provider = std::sync::Arc::new(
            crate::provider::openai::OpenAiProvider::builder()
                .endpoint("http://127.0.0.1:9/v1")
                .model("test-model")
                .name("test")
                .build()
                .expect("provider builds"),
        );
        let agent =
            crate::agent::Agent::new(provider, crate::tools::ToolRegistry::new()).with_store(store);
        crate::gateway::GatewayState::new(
            std::sync::Arc::new(agent),
            "test-model".into(),
            "test".into(),
            Some("sekret".into()),
            crate::gateway::ApprovalRouter::new(),
        )
        .expect("state builds")
    }
}

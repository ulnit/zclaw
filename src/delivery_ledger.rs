//! Durable delivery-obligation ledger for gateway final responses
//! (hermes `gateway/delivery_ledger.py` parity).
//!
//! A final agent response that was generated but not yet delivered to
//! the platform is the one artifact the gateway can lose without a
//! trace: the turn already burned its tokens, the text exists only in
//! memory, and a crash or planned restart between finalize and send
//! drops it silently. The ledger records a small durable row per
//! outbound response in `state.db` and writes checkpoints around the
//! send:
//!
//! ```text
//! record_obligation()  state='pending'     before any send attempt
//! mark_attempting()    state='attempting'  immediately before the send
//! mark_delivered()     state='delivered'   once the send returns
//! mark_failed()        state='failed'      on a definitive rejection
//! ```
//!
//! On startup [`sweep_and_redeliver`] claims rows whose owning process
//! is dead and redelivers them: `pending` rows plainly (the send never
//! started), `attempting`/`failed` rows with [`RECOVERED_MARKER`] so an
//! ambiguous send is honest at-least-once instead of a silent duplicate.
//! Rows past the attempts cap or the stale cutoff flip to `abandoned`.
//!
//! Everything here is best-effort by design: ledger failures must never
//! block or delay an actual send.

use sha2::{Digest, Sha256};
use std::sync::Arc;

use crate::session::SqliteSessionStore;

/// Redelivery budget per obligation (hermes `MAX_ATTEMPTS`).
pub const MAX_ATTEMPTS: i64 = 3;
/// Obligations older than this are abandoned, not retried (hermes
/// `STALE_AFTER_SECONDS`).
pub const STALE_AFTER_SECONDS: f64 = 24.0 * 60.0 * 60.0;
/// Delivered/abandoned rows are pruned after this window (hermes
/// `_RETENTION_SECONDS`).
pub const RETENTION_SECONDS: f64 = 7.0 * 24.0 * 60.0 * 60.0;
/// Hard row cap for the ledger table (hermes `_MAX_ROWS`).
pub const MAX_ROWS: usize = 500;

/// Visible prefix for redeliveries that might duplicate an
/// already-received message (crash mid-send / post-rejection retry) —
/// honest at-least-once (hermes `RECOVERED_MARKER`).
pub const RECOVERED_MARKER: &str =
    "\u{267b}\u{fe0f} Recovered reply \u{2014} the gateway restarted during delivery, \
     so this may be a duplicate:\n\n";

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Stable obligation id: sha256 over the routing + content + a nonce
/// (hermes hashes the same fields; the nonce lets identical repeat
/// replies coexist).
pub fn obligation_id(platform: &str, chat_id: &str, content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(platform.as_bytes());
    hasher.update(b"|");
    hasher.update(chat_id.as_bytes());
    hasher.update(b"|");
    hasher.update(content.as_bytes());
    hasher.update(b"|");
    hasher.update(uuid::Uuid::new_v4().as_bytes());
    format!("{:x}", hasher.finalize())[..32].to_string()
}

/// This process's ownership stamp: pid + process start time (hermes
/// `_owner_stamp`) so a recycled pid never looks alive.
pub fn owner_stamp() -> (u32, Option<u64>) {
    let pid = std::process::id();
    (pid, crate::gateway_pidfile::process_start_time(pid))
}

/// Whether the owner stamp of a ledger row still points at a live
/// process (hermes `_owner_alive`): pid alive AND start time matches
/// when both sides recorded one.
pub fn owner_alive(pid: Option<i64>, started: Option<i64>) -> bool {
    let Some(pid) = pid.filter(|pid| *pid > 0) else {
        // No owner recorded: treat as orphaned (dead) so the sweep can
        // claim it.
        return false;
    };
    if !crate::gateway_pidfile::is_alive(pid as u32) {
        return false;
    }
    match (
        started,
        crate::gateway_pidfile::process_start_time(pid as u32),
    ) {
        (Some(recorded), Some(actual)) => recorded as u64 == actual,
        _ => true, // start time unavailable on either side: pid liveness alone
    }
}

/// Record a pending obligation before the first send attempt. Returns
/// the obligation id; ledger failures degrade to None (send proceeds
/// without protection — never blocked).
pub fn record_obligation(
    store: &SqliteSessionStore,
    session_key: &str,
    platform: &str,
    chat_id: &str,
    thread_id: Option<&str>,
    content: &str,
) -> Option<String> {
    let id = obligation_id(platform, chat_id, content);
    let (pid, started) = owner_stamp();
    store
        .record_obligation(
            &id,
            session_key,
            platform,
            chat_id,
            thread_id,
            content,
            pid,
            started,
            now_secs(),
        )
        .ok()?;
    Some(id)
}

pub fn mark_attempting(store: &SqliteSessionStore, obligation_id: &str) {
    let _ = store.set_obligation_state(obligation_id, "attempting", None, now_secs());
}

pub fn mark_delivered(store: &SqliteSessionStore, obligation_id: &str) {
    let _ = store.set_obligation_state(obligation_id, "delivered", None, now_secs());
}

pub fn mark_failed(store: &SqliteSessionStore, obligation_id: &str, error: &str) {
    let _ = store.set_obligation_state(obligation_id, "failed", Some(error), now_secs());
}

/// Abandon an obligation with a reason (P707 dead-target skip path;
/// hermes abandons rows it must never retry).
fn mark_failed_abandoned(store: &SqliteSessionStore, obligation_id: &str, reason: &str) {
    let _ = store.set_obligation_state(obligation_id, "abandoned", Some(reason), now_secs());
}

/// Startup recovery (hermes `sweep_recoverable` + redelivery loop):
/// claim obligations orphaned by a dead gateway for platforms that
/// have a live sender this boot, redeliver them (marker on ambiguous
/// states), and prune retained rows. Returns the number redelivered.
pub async fn sweep_and_redeliver(store: &Arc<SqliteSessionStore>) -> usize {
    let platforms = crate::messaging::platform_sender_names();
    let (pid, started) = owner_stamp();
    let claimed = store.sweep_obligations(
        &platforms,
        pid,
        started,
        &|pid, started| owner_alive(pid, started),
        MAX_ATTEMPTS,
        STALE_AFTER_SECONDS,
        now_secs(),
    );
    let mut redelivered = 0usize;
    for (obligation, platform, chat_id, _thread_id, content, needs_marker, attempts) in claimed {
        // P707: a target already proven permanently unreachable (dead
        // chat / kicked bot) must not burn redelivery attempts on
        // every boot — abandon the row; a successful send to the
        // target later revives it (hermes dead-target registry).
        if crate::dead_targets::is_dead(&platform, &chat_id) {
            tracing::info!(
                "[delivery_ledger] abandoning orphaned reply for known-dead target {platform}/{chat_id}"
            );
            mark_failed_abandoned(store, &obligation, "target marked dead");
            continue;
        }
        let Some(sender) = crate::messaging::platform_sender(&platform) else {
            continue;
        };
        let text = if needs_marker {
            format!("{RECOVERED_MARKER}{content}")
        } else {
            content
        };
        tracing::info!(
            "[delivery_ledger] redelivering orphaned reply to {platform}/{chat_id} \
             (attempt {attempts}, marker={needs_marker})"
        );
        sender.send_text(&chat_id, &text).await;
        mark_delivered(store, &obligation);
        redelivered += 1;
    }
    let _ = store.prune_obligations(RETENTION_SECONDS, MAX_ROWS, now_secs());
    redelivered
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (tempfile::TempDir, Arc<SqliteSessionStore>) {
        let dir = tempfile::tempdir().unwrap();
        let store =
            Arc::new(SqliteSessionStore::open(dir.path().join("state.db")).expect("store opens"));
        (dir, store)
    }

    #[test]
    fn obligation_ids_are_distinct_for_identical_content() {
        let a = obligation_id("telegram", "1", "hello");
        let b = obligation_id("telegram", "1", "hello");
        assert_ne!(a, b);
        assert_eq!(a.len(), 32);
    }

    #[test]
    fn lifecycle_transitions_land_in_the_table() {
        let (_dir, store) = temp_store();
        let id =
            record_obligation(&store, "sess-1", "telegram", "42", None, "final answer").unwrap();
        let rows = store.obligation_rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].2, "pending");

        mark_attempting(&store, &id);
        assert_eq!(store.obligation_rows()[0].2, "attempting");
        mark_delivered(&store, &id);
        assert_eq!(store.obligation_rows()[0].2, "delivered");
    }

    #[test]
    fn failed_records_the_error_state() {
        let (_dir, store) = temp_store();
        let id = record_obligation(&store, "sess-1", "discord", "c1", None, "reply").unwrap();
        mark_failed(&store, &id, "rejected by platform");
        assert_eq!(store.obligation_rows()[0].2, "failed");
    }

    #[test]
    fn sweep_claims_dead_owner_rows_and_bumps_attempts() {
        let (_dir, store) = temp_store();
        let id = record_obligation(&store, "sess-1", "telegram", "42", None, "answer").unwrap();
        // The recorded owner is THIS (alive) process, so a sweep that
        // trusts real liveness claims nothing.
        let claimed = store.sweep_obligations(
            &["telegram".to_string()],
            std::process::id(),
            crate::gateway_pidfile::process_start_time(std::process::id()),
            &|pid, started| owner_alive(pid, started),
            MAX_ATTEMPTS,
            STALE_AFTER_SECONDS,
            now_secs(),
        );
        assert!(claimed.is_empty(), "live owner rows stay put");

        // A sweep whose liveness check reports dead claims the row,
        // bumps attempts, and marks needs_marker=false for pending.
        let claimed = store.sweep_obligations(
            &["telegram".to_string()],
            std::process::id(),
            None,
            &|_, _| false,
            MAX_ATTEMPTS,
            STALE_AFTER_SECONDS,
            now_secs(),
        );
        assert_eq!(claimed.len(), 1);
        let (oid, platform, chat_id, _thread, content, needs_marker, attempts) = &claimed[0];
        assert_eq!(oid, &id);
        assert_eq!(platform, "telegram");
        assert_eq!(chat_id, "42");
        assert_eq!(content, "answer");
        assert!(!needs_marker);
        assert_eq!(*attempts, 1);
        // The row is now owned by this process (alive) again.
        assert_eq!(store.obligation_rows()[0].3, 1);
    }

    #[test]
    fn sweep_abandons_over_budget_and_stale_rows() {
        let (_dir, store) = temp_store();
        let id = record_obligation(&store, "sess-1", "telegram", "42", None, "answer").unwrap();
        // Exhaust the attempts budget directly.
        for _ in 0..MAX_ATTEMPTS {
            store.sweep_obligations(
                &["telegram".to_string()],
                std::process::id(),
                None,
                &|_, _| false,
                MAX_ATTEMPTS,
                STALE_AFTER_SECONDS,
                now_secs(),
            );
        }
        // Next sweep: attempts at cap → abandoned, nothing claimed.
        let claimed = store.sweep_obligations(
            &["telegram".to_string()],
            std::process::id(),
            None,
            &|_, _| false,
            MAX_ATTEMPTS,
            STALE_AFTER_SECONDS,
            now_secs(),
        );
        assert!(claimed.is_empty());
        assert_eq!(store.obligation_rows()[0].2, "abandoned");
        let _ = id;
    }

    #[test]
    fn sweep_skips_platforms_without_senders() {
        let (_dir, store) = temp_store();
        record_obligation(&store, "sess-1", "matrix", "r1", None, "answer").unwrap();
        let claimed = store.sweep_obligations(
            &["telegram".to_string()], // matrix not deliverable this boot
            std::process::id(),
            None,
            &|_, _| false,
            MAX_ATTEMPTS,
            STALE_AFTER_SECONDS,
            now_secs(),
        );
        assert!(claimed.is_empty());
        // Untouched: still pending with zero attempts.
        let row = store.obligation_rows().remove(0);
        assert_eq!(row.2, "pending");
        assert_eq!(row.3, 0);
    }

    #[test]
    fn prune_removes_old_terminal_rows() {
        let (_dir, store) = temp_store();
        let id = record_obligation(&store, "sess-1", "telegram", "42", None, "answer").unwrap();
        mark_delivered(&store, &id);
        // Far-future prune: the delivered row is past retention.
        store
            .prune_obligations(
                RETENTION_SECONDS,
                MAX_ROWS,
                now_secs() + RETENTION_SECONDS + 10.0,
            )
            .unwrap();
        assert!(store.obligation_rows().is_empty());
    }

    #[tokio::test]
    async fn sweep_and_redeliver_skips_unknown_platforms() {
        // No senders registered for "telegram" in this process, so the
        // sweep leaves the row untouched (attempts unspent).
        let (_dir, store) = temp_store();
        record_obligation(&store, "sess-1", "nosuchplatform", "42", None, "answer").unwrap();
        let count = sweep_and_redeliver(&store).await;
        assert_eq!(count, 0);
        let row = store.obligation_rows().remove(0);
        assert_eq!(row.2, "pending");
    }
}

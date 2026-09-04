//! Gateway session stall notification policy — port of hermes
//! `gateway/session_stall.py` (#72016 item 2).
//!
//! Consumes the shared activity observation contract
//! ([`crate::session_activity`]) as the **single progress source**.
//! This module owns only the notify-once policy for "pending inbound
//! + stale progress"; it does not invent a parallel progress clock
//! from turn-start or inbound event timestamps.
//!
//! Boundaries (keep separate):
//! - outbound delivery obligations → [`crate::delivery_ledger`]
//! - dead-target short-circuits → [`crate::dead_targets`]
//! - pending inbound here is a stall *policy gate* (a queued follow-up
//!   exists), not an outbound obligation and not a progress timestamp.

use std::collections::HashMap;
use std::time::Duration;

/// Send bound for one stall notice (hermes
/// `_STALL_NOTIFY_SEND_TIMEOUT_SECONDS`): a wedged platform transport
/// must not block the whole watcher pass — sibling candidates would
/// never be evaluated and the watcher itself would stop ticking.
pub const STALL_NOTIFY_SEND_TIMEOUT_SECONDS: f64 = 15.0;

/// Default watcher cadence (hermes `_session_stall_watcher(interval=30)`).
pub const DEFAULT_WATCHER_INTERVAL_SECONDS: f64 = 30.0;

/// Resolve the effective stall timeout: the `ULNCLAW_SESSION_STALL_TIMEOUT`
/// env wins when it parses (hermes `_float_env` per-tick re-read),
/// otherwise the `[gateway] session_stall_timeout_secs` file value.
/// Zero or negative disables the watchdog.
pub fn resolve_stall_timeout(configured: f64) -> f64 {
    if let Ok(raw) = std::env::var("ULNCLAW_SESSION_STALL_TIMEOUT") {
        if let Ok(value) = raw.trim().parse::<f64>() {
            return value;
        }
    }
    configured
}

fn now_epoch() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Return true when a stall warning should be sent for this session.
pub fn should_emit_session_stall_notification(
    timeout_seconds: f64,
    idle_seconds: Option<f64>,
    has_pending_inbound: bool,
    already_notified: bool,
) -> bool {
    if timeout_seconds <= 0.0 {
        return false;
    }
    if !has_pending_inbound {
        return false;
    }
    if already_notified {
        return false;
    }
    match idle_seconds {
        Some(idle) => idle >= timeout_seconds,
        None => false,
    }
}

/// Return true when a prior stall notice may be cleared (episode ended).
pub fn should_clear_session_stall_notification(
    timeout_seconds: f64,
    idle_seconds: Option<f64>,
    has_pending_inbound: bool,
) -> bool {
    if !has_pending_inbound {
        return true;
    }
    if timeout_seconds <= 0.0 {
        return true;
    }
    match idle_seconds {
        // Unknown progress: hold the latch. Do not treat observation
        // gaps as recovery.
        None => false,
        Some(idle) => idle < timeout_seconds,
    }
}

/// User-facing stall warning (ASCII minutes; matches hermes copy).
pub fn format_session_stall_notification(idle_seconds: f64) -> String {
    let mins = ((idle_seconds / 60.0).floor() as i64).max(1);
    format!(
        "\u{26a0}\u{fe0f} Agent session appears stalled (last activity {mins} min ago). \
         Try /new to reset."
    )
}

/// Idle seconds from a shared activity snapshot only (hermes #72039
/// contract). Returns `None` when there is no usable progress
/// timestamp — callers must not fall back to turn-start or
/// pending-inbound clocks.
pub fn resolve_session_idle_seconds_from_activity(
    activity: Option<&crate::session_activity::ActivitySnapshot>,
    now: f64,
) -> Option<f64> {
    let at = activity?.last_activity_at;
    if !at.is_finite() {
        return None;
    }
    Some((now - at).max(0.0))
}

/// One watcher pass over the pending-inbound directory (hermes
/// `_check_session_stalls`). Returns notifications sent this pass.
/// Latches live in `notified` (one notify per stall episode).
pub async fn check_session_stalls(
    timeout_seconds: f64,
    notified: &mut HashMap<String, bool>,
) -> usize {
    let candidates = crate::messaging::pending_inbound_snapshot();
    let now = now_epoch();
    let mut sent = 0usize;

    for row in &candidates {
        let activity = crate::session_activity::snapshot(&row.session_key);
        let idle_seconds = resolve_session_idle_seconds_from_activity(activity.as_ref(), now);
        if should_clear_session_stall_notification(
            timeout_seconds,
            idle_seconds,
            true, // directory rows are pending by construction
        ) {
            notified.remove(&row.session_key);
        }
        let already = notified.get(&row.session_key).copied().unwrap_or(false);
        if !should_emit_session_stall_notification(timeout_seconds, idle_seconds, true, already) {
            continue;
        }
        let Some(idle) = idle_seconds else { continue };

        // Re-read pending state + activity timestamp IMMEDIATELY before
        // delivery (hermes #76354 review S2): the snapshot above ages
        // while earlier candidates in this pass await their sends; an
        // agent that made progress (or drained its queue) in that
        // window must not receive a false stall notice. Abort and
        // leave the latch un-set so the next tick re-evaluates.
        let still_pending = crate::messaging::pending_inbound_contains(&row.session_key);
        let fresh_idle = resolve_session_idle_seconds_from_activity(
            crate::session_activity::snapshot(&row.session_key).as_ref(),
            now_epoch(),
        );
        if !still_pending || fresh_idle.is_some_and(|i| i < timeout_seconds) {
            notified.remove(&row.session_key);
            continue;
        }

        let Some(sender) = crate::messaging::platform_sender(&row.platform) else {
            continue;
        };
        tracing::warn!(
            "session stall detected: session={} idle={}s (timeout={}s, ~{} min); \
             pending inbound present | last_activity={:?} | provenance={}",
            row.session_key,
            idle as i64,
            timeout_seconds as i64,
            ((idle / 60.0).floor() as i64).max(1),
            activity.as_ref().map(|a| a.last_activity_at),
            activity
                .as_ref()
                .map(|a| a.provenance.as_str())
                .unwrap_or("unknown"),
        );
        let text = format_session_stall_notification(idle);
        let send = tokio::time::timeout(
            Duration::from_secs_f64(STALL_NOTIFY_SEND_TIMEOUT_SECONDS),
            sender.send_text(&row.chat_id, &text),
        )
        .await;
        match send {
            Ok(()) => {
                sent += 1;
                notified.insert(row.session_key.clone(), true);
            }
            Err(_) => {
                // Do not latch — retry next watcher tick.
                tracing::warn!(
                    "session stall notify send timed out after {}s for {}; will retry next tick",
                    STALL_NOTIFY_SEND_TIMEOUT_SECONDS,
                    row.session_key,
                );
            }
        }
    }

    // Drop latches for sessions that no longer appear in any pending map.
    notified.retain(|key, _| candidates.iter().any(|row| row.session_key == *key));
    sent
}

/// Periodic pending-inbound + stale-activity stall watchdog (hermes
/// `_session_stall_watcher`). Notify-only: does not kill the turn
/// (contrast gateway timeouts / the shutdown watchdog family).
pub async fn run_stall_watcher(timeout_seconds: f64, interval_seconds: f64) {
    // Short initial delay so startup reconnect noise does not false-fire.
    let initial = interval_seconds.clamp(1.0, 30.0);
    tokio::time::sleep(Duration::from_secs_f64(initial)).await;
    let mut notified: HashMap<String, bool> = HashMap::new();
    loop {
        let timeout = resolve_stall_timeout(timeout_seconds);
        if timeout > 0.0 {
            if tokio::time::timeout(
                Duration::from_secs_f64(interval_seconds.max(5.0)),
                check_session_stalls(timeout, &mut notified),
            )
            .await
            .is_err()
            {
                tracing::debug!("session stall watcher pass overran its budget");
            }
        }
        tokio::time::sleep(Duration::from_secs_f64(interval_seconds)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_activity::ActivitySnapshot;

    fn snap(at: f64) -> ActivitySnapshot {
        ActivitySnapshot {
            last_activity_at: at,
            description: "test".into(),
            provenance: "unknown".into(),
        }
    }

    #[test]
    fn emit_policy_matches_hermes() {
        // Disabled timeout, missing pending, already-notified, unknown
        // idle and fresh activity all suppress the notice.
        assert!(!should_emit_session_stall_notification(
            0.0,
            Some(999.0),
            true,
            false
        ));
        assert!(!should_emit_session_stall_notification(
            300.0,
            Some(999.0),
            false,
            false
        ));
        assert!(!should_emit_session_stall_notification(
            300.0,
            Some(999.0),
            true,
            true
        ));
        assert!(!should_emit_session_stall_notification(
            300.0, None, true, false
        ));
        assert!(!should_emit_session_stall_notification(
            300.0,
            Some(299.9),
            true,
            false
        ));
        // Stale + pending + un-notified fires.
        assert!(should_emit_session_stall_notification(
            300.0,
            Some(300.0),
            true,
            false
        ));
        assert!(should_emit_session_stall_notification(
            300.0,
            Some(9999.0),
            true,
            false
        ));
    }

    #[test]
    fn clear_policy_matches_hermes() {
        // Episode ends when the queue drains or the watchdog is off.
        assert!(should_clear_session_stall_notification(
            300.0,
            Some(9999.0),
            false
        ));
        assert!(should_clear_session_stall_notification(
            0.0,
            Some(9999.0),
            true
        ));
        // Unknown progress holds the latch; fresh activity clears it.
        assert!(!should_clear_session_stall_notification(300.0, None, true));
        assert!(should_clear_session_stall_notification(
            300.0,
            Some(1.0),
            true
        ));
        assert!(!should_clear_session_stall_notification(
            300.0,
            Some(300.0),
            true
        ));
    }

    #[test]
    fn format_uses_ascii_minutes_with_floor_of_one() {
        assert_eq!(
            format_session_stall_notification(5.0),
            "\u{26a0}\u{fe0f} Agent session appears stalled (last activity 1 min ago). Try /new to reset."
        );
        assert_eq!(
            format_session_stall_notification(600.0),
            "\u{26a0}\u{fe0f} Agent session appears stalled (last activity 10 min ago). Try /new to reset."
        );
    }

    #[test]
    fn resolve_idle_only_from_activity() {
        assert_eq!(
            resolve_session_idle_seconds_from_activity(None, 500.0),
            None
        );
        assert_eq!(
            resolve_session_idle_seconds_from_activity(Some(&snap(400.0)), 500.0),
            Some(100.0)
        );
        assert_eq!(
            resolve_session_idle_seconds_from_activity(Some(&snap(600.0)), 500.0),
            Some(0.0)
        );
        let non_finite = ActivitySnapshot {
            last_activity_at: f64::NAN,
            description: String::new(),
            provenance: "unknown".into(),
        };
        assert_eq!(
            resolve_session_idle_seconds_from_activity(Some(&non_finite), 500.0),
            None
        );
    }

    struct StallCapture {
        sends: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
    }

    #[async_trait::async_trait]
    impl crate::messaging::PlatformSender for StallCapture {
        async fn send_text(&self, chat_id: &str, text: &str) {
            self.sends
                .lock()
                .unwrap()
                .push((chat_id.to_string(), text.to_string()));
        }
    }

    #[tokio::test]
    async fn check_session_stalls_notifies_once_per_episode() {
        // Process-global registries (pending inbound directory,
        // platform senders, activity stamps) — serialize with other
        // global-state tests.
        let _env_guard = crate::models_dev::test_env_lock();
        let key = "platform-stallplat-chat-77".to_string();
        let sends = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        crate::messaging::register_platform_sender(
            "stallplat",
            std::sync::Arc::new(StallCapture {
                sends: sends.clone(),
            }),
        );
        crate::session_activity::clear_for_tests();
        crate::messaging::clear_pending_inbound_for_tests();

        // Pending inbound + stale activity stamp (10 minutes idle).
        crate::messaging::register_pending_inbound(&key, "stallplat", "chat-77");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        crate::session_activity::touch_at(&key, "tool call: shell", "agent.progress", now - 600.0);

        let mut notified: HashMap<String, bool> = HashMap::new();
        let sent = check_session_stalls(300.0, &mut notified).await;
        assert_eq!(sent, 1);
        assert_eq!(*notified.get(&key).unwrap(), true);
        let captured = sends.lock().unwrap().clone();
        assert_eq!(captured.len(), 1, "{captured:?}");
        assert_eq!(captured[0].0, "chat-77");
        assert!(captured[0].1.contains("appears stalled"), "{captured:?}");
        assert!(captured[0].1.contains("10 min ago"), "{captured:?}");

        // Second pass: latched, no re-notify.
        let sent = check_session_stalls(300.0, &mut notified).await;
        assert_eq!(sent, 0);
        assert_eq!(sends.lock().unwrap().len(), 1);

        // Fresh progress clears the latch; a later stall notifies again.
        crate::session_activity::touch_at(&key, "streaming", "agent.progress", now);
        let sent = check_session_stalls(300.0, &mut notified).await;
        assert_eq!(sent, 0);
        assert!(notified.get(&key).is_none(), "episode latch must clear");
        crate::session_activity::touch_at(&key, "tool call: shell", "agent.progress", now - 900.0);
        let sent = check_session_stalls(300.0, &mut notified).await;
        assert_eq!(sent, 1);
        assert_eq!(sends.lock().unwrap().len(), 2);

        // Queue drain ends the episode and drops the latch.
        crate::messaging::unregister_pending_inbound(&key);
        let sent = check_session_stalls(300.0, &mut notified).await;
        assert_eq!(sent, 0);
        assert!(notified.is_empty(), "{notified:?}");

        // Unknown progress (no activity stamp) never notifies.
        crate::messaging::register_pending_inbound(&key, "stallplat", "chat-77");
        crate::session_activity::remove(&key);
        let sent = check_session_stalls(300.0, &mut notified).await;
        assert_eq!(sent, 0);
        assert_eq!(sends.lock().unwrap().len(), 2);

        crate::messaging::clear_pending_inbound_for_tests();
        crate::session_activity::clear_for_tests();
        crate::messaging::unregister_platform_sender_for_tests("stallplat");
    }

    #[test]
    fn env_override_wins_when_parseable() {
        // SAFETY: mutates process env; guarded by the global test lock.
        let _env_guard = crate::models_dev::test_env_lock();
        std::env::set_var("ULNCLAW_SESSION_STALL_TIMEOUT", "42.5");
        assert_eq!(resolve_stall_timeout(300.0), 42.5);
        std::env::set_var("ULNCLAW_SESSION_STALL_TIMEOUT", "garbage");
        assert_eq!(resolve_stall_timeout(300.0), 300.0);
        std::env::remove_var("ULNCLAW_SESSION_STALL_TIMEOUT");
    }
}

//! Per-session turn lease — serializes the [load history → run →
//! flush] region (hermes `gateway/turn_lease.py` parity, #64934).
//!
//! The gateway's busy guards are keyed by ROUTING KEY (the chat key),
//! but the durable transcript is owned by SESSION_ID — and session
//! remapping (`/resume`, `/handoff`) makes the key→id mapping
//! many-to-one. Two routing keys mapped to one session_id run
//! concurrent turns on two different agent objects, so no per-key
//! guard ever sees the collision; the two turns then interleave
//! their flushes on one transcript (rows persist in completion order
//! instead of arrival order).
//!
//! The lease registry keeps one async lock per resolved session_id.
//! Semantics (all from hermes):
//!
//! - **Ownership-checked release.** Each acquire returns a token
//!   (routing key + generation); release only frees the lease when
//!   that exact token is the current holder — a stale unwind can
//!   never release a newer turn's lease. Release is idempotent.
//! - **Fail-open on timeout.** A stuck holder degrades to today's
//!   unserialized behavior with a loud warning after the configured
//!   wait — never a wedged session. A degraded token holds nothing
//!   and releases nothing.
//! - **Bounded registry.** The per-session lease map is size-capped;
//!   eviction only ever removes idle (unheld, uncontended) entries,
//!   never a live lease.
//! - **Rebind on mid-turn rotation** aliases a held lease onto a new
//!   session id so the serialization boundary follows the transcript.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Upper bound on tracked per-session leases. Idle entries (no
/// holder, no waiter) are evicted oldest-first once the cap is
/// reached; live leases are never evicted (hermes
/// `DEFAULT_MAX_LEASES`).
pub const DEFAULT_MAX_LEASES: usize = 512;

/// Fallback wait when the caller passes no timeout (hermes
/// `DEFAULT_LEASE_WAIT`): a stuck holder fails open on the same
/// clock a turn itself would be declared stuck on.
pub const DEFAULT_LEASE_WAIT: Duration = Duration::from_secs(1800);

/// Handle returned by [`SessionTurnLeaseRegistry::acquire`] (hermes
/// `TurnLeaseToken`). `degraded` means the acquire timed out and the
/// turn proceeds UNSERIALIZED (fail-open); such a token holds
/// nothing and its release is a no-op.
pub struct TurnLeaseToken {
    session_id: Mutex<String>,
    owner_key: String,
    generation: u64,
    degraded: bool,
    released: AtomicBool,
    /// The held lock guard (None for degraded tokens or after
    /// release). Dropping it frees the lease.
    guard: Mutex<Option<tokio::sync::OwnedMutexGuard<()>>>,
}

impl TurnLeaseToken {
    pub fn session_id(&self) -> String {
        self.session_id
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default()
    }

    pub fn owner_key(&self) -> &str {
        &self.owner_key
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn is_degraded(&self) -> bool {
        self.degraded
    }

    pub fn is_released(&self) -> bool {
        self.released.load(Ordering::SeqCst)
    }
}

struct SessionLease {
    lock: Arc<tokio::sync::Mutex<()>>,
    holder: Mutex<Option<Arc<TurnLeaseToken>>>,
    acquired_at: Mutex<Option<Instant>>,
    last_used: Mutex<Instant>,
}

impl SessionLease {
    fn new() -> Self {
        Self {
            lock: Arc::new(tokio::sync::Mutex::new(())),
            holder: Mutex::new(None),
            acquired_at: Mutex::new(None),
            last_used: Mutex::new(Instant::now()),
        }
    }

    /// True when this lease can be evicted: nobody holds or awaits it.
    fn idle(&self) -> bool {
        let unheld = self
            .holder
            .lock()
            .map(|holder| holder.is_none())
            .unwrap_or(true);
        unheld && self.lock.clone().try_lock_owned().is_ok()
    }

    fn touch(&self) {
        if let Ok(mut last_used) = self.last_used.lock() {
            *last_used = Instant::now();
        }
    }
}

/// Lease registry per resolved session_id, serializing transcript
/// turns (hermes `SessionTurnLeaseRegistry`). Process-local by
/// design — the same visibility scope as the routing-key busy guards
/// it extends.
#[derive(Default)]
pub struct SessionTurnLeaseRegistry {
    leases: Mutex<HashMap<String, Arc<SessionLease>>>,
    max_entries: usize,
}

impl SessionTurnLeaseRegistry {
    pub fn new(max_entries: usize) -> Self {
        Self {
            leases: Mutex::new(HashMap::new()),
            max_entries: max_entries.max(1),
        }
    }

    /// Number of tracked leases (diagnostics/tests).
    pub fn len(&self) -> usize {
        self.leases.lock().map(|m| m.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn get_or_create(&self, session_id: &str) -> Arc<SessionLease> {
        let Ok(mut leases) = self.leases.lock() else {
            return Arc::new(SessionLease::new());
        };
        let lease = match leases.get(session_id) {
            Some(lease) => lease.clone(),
            None => {
                Self::evict_idle(&mut leases, self.max_entries);
                let lease = Arc::new(SessionLease::new());
                leases.insert(session_id.to_string(), lease.clone());
                lease
            }
        };
        lease.touch();
        lease
    }

    /// Drop oldest idle entries so a new lease fits under the cap.
    /// Never evicts a held or contended lease — correctness beats the
    /// cap (hermes `_evict_idle`).
    fn evict_idle(leases: &mut HashMap<String, Arc<SessionLease>>, max_entries: usize) {
        let overflow = leases.len() as isize - max_entries as isize + 1;
        if overflow <= 0 {
            return;
        }
        let mut idle: Vec<(String, Instant)> = Vec::new();
        for (session_id, lease) in leases.iter() {
            if lease.idle() {
                let last_used = lease
                    .last_used
                    .lock()
                    .map(|t| *t)
                    .unwrap_or_else(|_| Instant::now());
                idle.push((session_id.clone(), last_used));
            }
        }
        idle.sort_by_key(|(_, last_used)| *last_used);
        for (session_id, _) in idle.into_iter().take(overflow as usize) {
            leases.remove(&session_id);
        }
    }

    /// Acquire the turn lease for `session_id`, waiting if held
    /// (hermes `acquire`). Returns a token — degraded when the wait
    /// timed out (fail-open: the caller proceeds unserialized), or
    /// None for an empty `session_id`.
    pub async fn acquire(
        &self,
        session_id: &str,
        owner_key: &str,
        generation: u64,
        timeout: Option<Duration>,
    ) -> Option<Arc<TurnLeaseToken>> {
        if session_id.trim().is_empty() {
            return None;
        }
        let wait = timeout
            .filter(|t| *t > Duration::ZERO)
            .unwrap_or(DEFAULT_LEASE_WAIT);
        let lease = self.get_or_create(session_id);

        if lease.lock.clone().try_lock_owned().is_err() {
            let holder = lease.holder.lock().ok().and_then(|h| h.clone());
            let held_secs = lease
                .acquired_at
                .lock()
                .ok()
                .and_then(|acquired| *acquired)
                .map(|at| at.elapsed().as_secs_f64())
                .unwrap_or(-1.0);
            tracing::warn!(
                "[turn_lease] contention on session {session_id}: routing key {owner_key} \
                 (gen {generation}) waiting behind in-flight turn held by routing key {} \
                 (gen {}, held {held_secs:.0}s) — two routing keys are mapped to one \
                 session_id; serializing this turn behind the previous turn's flush",
                holder.as_ref().map(|h| h.owner_key.as_str()).unwrap_or("?"),
                holder.as_ref().map(|h| h.generation).unwrap_or(0),
            );
        }

        let lock = lease.lock.clone();
        let guard = match tokio::time::timeout(wait, lock.lock_owned()).await {
            Ok(guard) => guard,
            Err(_) => {
                let holder = lease.holder.lock().ok().and_then(|h| h.clone());
                tracing::error!(
                    "[turn_lease] wait timed out after {wait:?} on session {session_id} \
                     (waiter: routing key {owner_key} gen {generation}; holder: routing key {} \
                     gen {}) — failing open: this turn runs UNSERIALIZED against the stuck \
                     holder rather than wedging the session; transcript writes may interleave",
                    holder.as_ref().map(|h| h.owner_key.as_str()).unwrap_or("?"),
                    holder.as_ref().map(|h| h.generation).unwrap_or(0),
                );
                return Some(Arc::new(TurnLeaseToken {
                    session_id: Mutex::new(session_id.to_string()),
                    owner_key: owner_key.to_string(),
                    generation,
                    degraded: true,
                    released: AtomicBool::new(false),
                    guard: Mutex::new(None),
                }));
            }
        };

        let token = Arc::new(TurnLeaseToken {
            session_id: Mutex::new(session_id.to_string()),
            owner_key: owner_key.to_string(),
            generation,
            degraded: false,
            released: AtomicBool::new(false),
            guard: Mutex::new(Some(guard)),
        });
        if let Ok(mut holder) = lease.holder.lock() {
            *holder = Some(token.clone());
        }
        if let Ok(mut acquired_at) = lease.acquired_at.lock() {
            *acquired_at = Some(Instant::now());
        }
        lease.touch();
        Some(token)
    }

    /// Alias a HELD lease onto `new_session_id` after mid-turn
    /// rotation (hermes `rebind`): the SAME lease is registered under
    /// the new id so acquirers on either id serialize against one
    /// lock. Only the current holder can rebind; if the new id
    /// already has a live lease of its own the rebind is refused
    /// (fail-open, never deadlock).
    pub fn rebind(&self, token: &Arc<TurnLeaseToken>, new_session_id: &str) -> bool {
        let old_id = token.session_id();
        if token.degraded
            || token.is_released()
            || new_session_id.trim().is_empty()
            || new_session_id == old_id
        {
            return false;
        }
        let Ok(mut leases) = self.leases.lock() else {
            return false;
        };
        let Some(lease) = leases.get(&old_id).cloned() else {
            return false;
        };
        {
            let Ok(holder) = lease.holder.lock() else {
                return false;
            };
            match holder.as_ref() {
                Some(current) if Arc::ptr_eq(current, token) => {}
                _ => return false,
            }
        }
        if let Some(existing) = leases.get(new_session_id) {
            if !Arc::ptr_eq(existing, &lease) && !existing.idle() {
                let holder = existing.holder.lock().ok().and_then(|h| h.clone());
                tracing::warn!(
                    "[turn_lease] rebind blocked: session {old_id} rotated to {new_session_id} \
                     mid-turn (holder: routing key {} gen {}) but the target session's lease \
                     is already live (holder: routing key {} gen {}) — keeping the lease on \
                     the old id; transcript writes on {new_session_id} may interleave",
                    token.owner_key,
                    token.generation,
                    holder.as_ref().map(|h| h.owner_key.as_str()).unwrap_or("?"),
                    holder.as_ref().map(|h| h.generation).unwrap_or(0),
                );
                return false;
            }
        }
        leases.insert(new_session_id.to_string(), lease.clone());
        lease.touch();
        if let Ok(mut session_id) = token.session_id.lock() {
            *session_id = new_session_id.to_string();
        }
        true
    }

    /// Release `token`'s lease. Idempotent; ownership-checked (hermes
    /// `release`). Returns true only when this exact token was the
    /// current holder and the lock was freed.
    pub fn release(&self, token: &Arc<TurnLeaseToken>) -> bool {
        if token.degraded || token.released.swap(true, Ordering::SeqCst) {
            return false;
        }
        let session_id = token.session_id();
        let Some(lease) = self
            .leases
            .lock()
            .ok()
            .and_then(|leases| leases.get(&session_id).cloned())
        else {
            return false;
        };
        {
            let Ok(mut holder) = lease.holder.lock() else {
                return false;
            };
            match holder.as_ref() {
                Some(current) if Arc::ptr_eq(current, token) => {
                    *holder = None;
                }
                _ => {
                    // Not the current holder: undo the released flag
                    // flip? No — hermes marks released then returns
                    // false; keep released=true (idempotent no-op).
                    tracing::debug!(
                        "[turn_lease] release skipped on session {session_id}: token \
                         (key {} gen {}) is not the current holder",
                        token.owner_key,
                        token.generation
                    );
                    return false;
                }
            }
        }
        if let Ok(mut acquired_at) = lease.acquired_at.lock() {
            *acquired_at = None;
        }
        lease.touch();
        // Drop the guard last: frees the lock for the next waiter.
        if let Ok(mut guard) = token.guard.lock() {
            guard.take();
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> SessionTurnLeaseRegistry {
        SessionTurnLeaseRegistry::new(DEFAULT_MAX_LEASES)
    }

    #[tokio::test]
    async fn acquire_release_roundtrip() {
        let reg = registry();
        let token = reg.acquire("sess-1", "key-a", 1, None).await.unwrap();
        assert!(!token.is_degraded());
        assert!(!token.is_released());
        assert_eq!(token.session_id(), "sess-1");
        assert!(reg.release(&token));
        assert!(token.is_released());
        // Idempotent second release.
        assert!(!reg.release(&token));
    }

    #[tokio::test]
    async fn empty_session_id_acquires_nothing() {
        let reg = registry();
        assert!(reg.acquire("", "key", 1, None).await.is_none());
        assert!(reg.acquire("   ", "key", 1, None).await.is_none());
    }

    #[tokio::test]
    async fn same_session_serializes_distinct_sessions_do_not() {
        let reg = Arc::new(registry());

        // Hold the lease on sess-1.
        let holder = reg.acquire("sess-1", "key-a", 1, None).await.unwrap();

        // A waiter on the SAME session blocks until release.
        let reg2 = reg.clone();
        let waiter = tokio::spawn(async move {
            let token = reg2.acquire("sess-1", "key-b", 2, None).await.unwrap();
            token.session_id()
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiter.is_finished(), "waiter must block behind the holder");
        reg.release(&holder);
        assert_eq!(waiter.await.unwrap(), "sess-1");

        // A different session is never blocked by sess-1.
        let other = reg
            .acquire("sess-2", "key-c", 1, Some(Duration::from_millis(100)))
            .await
            .unwrap();
        assert!(!other.is_degraded());
    }

    #[tokio::test]
    async fn timeout_degrades_and_fails_open() {
        let reg = registry();
        let holder = reg.acquire("sess-1", "key-a", 1, None).await.unwrap();
        let token = reg
            .acquire("sess-1", "key-b", 2, Some(Duration::from_millis(50)))
            .await
            .unwrap();
        assert!(token.is_degraded());
        // Degraded tokens hold nothing and release nothing.
        assert!(!reg.release(&token));
        assert!(!token.is_released());
        assert!(!reg.release(&token));
        // The real holder still owns the lease.
        assert!(reg.release(&holder));
    }

    #[tokio::test]
    async fn stale_token_cannot_release_newer_holder() {
        let reg = registry();
        let first = reg.acquire("sess-1", "key-a", 1, None).await.unwrap();
        reg.release(&first);
        let second = reg.acquire("sess-1", "key-b", 2, None).await.unwrap();
        // A stale re-release of the first token must not free the
        // second turn's lease.
        assert!(!reg.release(&first));
        assert!(!second.is_released());
        assert!(reg.release(&second));
    }

    #[tokio::test]
    async fn rebind_aliases_lease_to_rotated_session() {
        let reg = Arc::new(registry());
        let holder = reg.acquire("sess-old", "key-a", 1, None).await.unwrap();
        assert!(reg.rebind(&holder, "sess-new"));
        assert_eq!(holder.session_id(), "sess-new");

        // An acquirer on the NEW id serializes behind the rebound holder.
        let reg2 = reg.clone();
        let waiter = tokio::spawn(async move {
            let token = reg2.acquire("sess-new", "key-b", 2, None).await.unwrap();
            token.session_id()
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiter.is_finished(), "rebind must serialize the new id");
        assert!(reg.release(&holder));
        assert_eq!(waiter.await.unwrap(), "sess-new");
    }

    #[tokio::test]
    async fn rebind_refused_when_target_lease_is_live() {
        let reg = registry();
        let holder = reg.acquire("sess-a", "key-a", 1, None).await.unwrap();
        let other = reg.acquire("sess-b", "key-b", 1, None).await.unwrap();
        // sess-b has a live lease of its own — cannot merge mid-wait.
        assert!(!reg.rebind(&holder, "sess-b"));
        assert_eq!(holder.session_id(), "sess-a");
        assert!(reg.release(&holder));
        assert!(reg.release(&other));
    }

    #[tokio::test]
    async fn rebind_requires_current_holder() {
        let reg = registry();
        let first = reg.acquire("sess-1", "key-a", 1, None).await.unwrap();
        reg.release(&first);
        let second = reg.acquire("sess-1", "key-b", 2, None).await.unwrap();
        // The stale first token is not the holder — rebind refused.
        assert!(!reg.rebind(&first, "sess-2"));
        assert!(reg.rebind(&second, "sess-2"));
        assert!(reg.release(&second));
    }

    #[tokio::test]
    async fn eviction_drops_idle_entries_but_never_live_ones() {
        let reg = SessionTurnLeaseRegistry::new(2);
        let live = reg.acquire("live", "key-a", 1, None).await.unwrap();
        let _idle1 = reg.acquire("idle-1", "key-b", 1, None).await.unwrap();
        reg.release(&_idle1);
        // Fill to the cap with a live lease + idle leases; the live
        // lease must survive evictions.
        let t2 = reg.acquire("idle-2", "key-c", 1, None).await.unwrap();
        reg.release(&t2);
        let t3 = reg.acquire("idle-3", "key-d", 1, None).await.unwrap();
        reg.release(&t3);
        assert!(reg.len() <= 3, "registry stays near the cap");
        // The live lease still serializes: acquire on 'live' returns
        // immediately only after release (it was never evicted).
        assert!(!live.is_released());
        assert!(reg.release(&live));
    }
}

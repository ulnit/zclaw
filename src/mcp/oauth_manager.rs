//! Central manager for per-server MCP OAuth state (hermes
//! `tools/mcp_oauth_manager.py`).
//!
//! One instance shared across the process. Coordinates:
//!
//! - **Cross-process token reload** via mtime-based disk watch: when an
//!   external process (cron job, another ulnclaw profile, the dashboard
//!   OAuth bridge) refreshes tokens on disk, the next request picks them
//!   up without a restart (hermes `invalidate_if_disk_changed`, design
//!   reference Claude Code `invalidateOAuthCacheIfDiskChanged`).
//! - **401 deduplication** via shared in-flight futures keyed by the
//!   failed access token: when N concurrent tool calls all 401 with the
//!   same token, only one recovery fires; the rest await the same result
//!   (hermes `handle_401` / `pending_401`, mirrors Claude Code's
//!   `pending401Handlers`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use futures::future::{FutureExt, Shared};

use crate::error::{AgentError, Result};

use super::oauth;

type RecoveryFuture = Shared<
    std::pin::Pin<
        Box<dyn std::future::Future<Output = std::result::Result<String, String>> + Send>,
    >,
>;

#[derive(Default)]
struct ServerEntry {
    /// Last-seen modification time of the on-disk tokens file, in
    /// nanoseconds since the epoch (0 = never seen).
    last_mtime_ns: u128,
    /// In-flight 401 recoveries keyed by the failed access token.
    pending_401: HashMap<String, RecoveryFuture>,
}

fn entry_key(home: &Path, server_name: &str) -> String {
    format!("{}::{}", home.display(), server_name)
}

fn tokens_file_mtime_ns(home: &Path, server_name: &str) -> Option<u128> {
    let path = oauth::token_dir(home).join(format!("{}.json", oauth::safe_filename(server_name)));
    let metadata = std::fs::metadata(path).ok()?;
    let modified = metadata.modified().ok()?;
    let duration = modified
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    Some(duration.as_nanos())
}

pub struct OAuthManager {
    entries: Mutex<HashMap<String, ServerEntry>>,
}

impl Default for OAuthManager {
    fn default() -> Self {
        Self::new()
    }
}

impl OAuthManager {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Record the tokens file's current mtime as "seen" (call after
    /// reading/writing tokens so later external changes stand out).
    pub fn note_mtime(&self, home: &Path, server_name: &str) {
        let Some(mtime_ns) = tokens_file_mtime_ns(home, server_name) else {
            return;
        };
        let mut entries = self.entries.lock().unwrap();
        entries
            .entry(entry_key(home, server_name))
            .or_default()
            .last_mtime_ns = mtime_ns;
    }

    /// If the tokens file on disk has a different mtime than last-seen,
    /// update the watermark and return true — the caller should reload
    /// tokens from disk (hermes `invalidate_if_disk_changed`).
    pub fn invalidate_if_disk_changed(&self, home: &Path, server_name: &str) -> bool {
        let Some(mtime_ns) = tokens_file_mtime_ns(home, server_name) else {
            return false;
        };
        let mut entries = self.entries.lock().unwrap();
        let entry = entries.entry(entry_key(home, server_name)).or_default();
        if mtime_ns != entry.last_mtime_ns {
            entry.last_mtime_ns = mtime_ns;
            true
        } else {
            false
        }
    }

    /// Handle a 401 from a request, deduplicated across concurrent
    /// callers by the failed access token (hermes `handle_401`).
    ///
    /// Returns the access token to retry with:
    /// 1. the on-disk token when the file changed since last seen (an
    ///    external process already refreshed it — no network recovery);
    /// 2. otherwise the outcome of `recover` (refresh grant / re-auth),
    ///    shared with every other caller that 401'd on the same token.
    ///
    /// The recovery runs on a spawned task (hermes `_inflight_tasks`) so
    /// it completes even if the first requester is cancelled.
    pub async fn handle_401<F, Fut>(
        &self,
        home: &Path,
        server_name: &str,
        failed_access_token: Option<&str>,
        recover: F,
    ) -> Result<String>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<String>> + Send + 'static,
    {
        let key = failed_access_token.unwrap_or("<unknown>").to_string();
        let cache_key = entry_key(home, server_name);

        let existing = {
            let mut entries = self.entries.lock().unwrap();
            entries
                .entry(cache_key.clone())
                .or_default()
                .pending_401
                .get(&key)
                .cloned()
        };
        if let Some(shared) = existing {
            return shared.await.map_err(AgentError::Tool);
        }

        // Step 1 (hermes): did disk change? Picks up external refresh
        // without spending a network round-trip.
        if self.invalidate_if_disk_changed(home, server_name) {
            if let Some(tokens) = oauth::load_tokens(home, server_name) {
                if tokens.is_valid() {
                    return Ok(tokens.access_token);
                }
            }
        }

        // Step 2: run the recovery, shared across concurrent callers.
        let home_owned: PathBuf = home.to_path_buf();
        let server_owned = server_name.to_string();
        let recovery = async move {
            recover()
                .await
                .map_err(|e| e.to_string())
                .inspect(|_token| {
                    // The recovery rewrote the tokens file — refresh the
                    // mtime watermark so the next disk-watch pass starts
                    // from here.
                    if let Some(mtime_ns) = tokens_file_mtime_ns(&home_owned, &server_owned) {
                        if let Some(manager) = MANAGERS.get() {
                            let mut entries = manager.entries.lock().unwrap();
                            if let Some(entry) =
                                entries.get_mut(&entry_key(&home_owned, &server_owned))
                            {
                                entry.last_mtime_ns = mtime_ns;
                            }
                        }
                    }
                })
        }
        .boxed();
        let shared: RecoveryFuture = recovery.shared();

        {
            let mut entries = self.entries.lock().unwrap();
            entries
                .entry(cache_key.clone())
                .or_default()
                .pending_401
                .insert(key.clone(), shared.clone());
        }

        // Drive the recovery to completion regardless of awaiters (hermes
        // `_inflight_tasks`), then drop the pending entry.
        let driver = shared.clone();
        let cleanup_key = key.clone();
        let cleanup_cache_key = cache_key.clone();
        tokio::spawn(async move {
            let _ = driver.await;
            if let Some(manager) = MANAGERS.get() {
                let mut entries = manager.entries.lock().unwrap();
                if let Some(entry) = entries.get_mut(&cleanup_cache_key) {
                    entry.pending_401.remove(&cleanup_key);
                }
            }
        });

        shared.await.map_err(AgentError::Tool)
    }
}

static MANAGERS: OnceLock<OAuthManager> = OnceLock::new();

/// The process-wide manager singleton (hermes `get_manager`).
pub fn manager() -> &'static OAuthManager {
    MANAGERS.get_or_init(OAuthManager::new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::oauth::{save_tokens, StoredTokens};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    fn write_valid_tokens(home: &Path, server: &str, token: &str) {
        let tokens = StoredTokens {
            access_token: token.to_string(),
            refresh_token: Some("refresh".into()),
            expires_at: crate::mcp::oauth::now_secs() + 3600,
        };
        save_tokens(home, server, &tokens).unwrap();
    }

    /// Bump a file's mtime deterministically (filesystem timestamp
    /// granularity can exceed test speed).
    fn touch(path: &Path, nanos: u128) {
        let mtime = SystemTime::UNIX_EPOCH + Duration::from_nanos(nanos as u64);
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
    }

    #[test]
    fn disk_change_detection_tracks_mtime() {
        let mgr = OAuthManager::new();
        let tmp = tempfile::tempdir().unwrap();
        write_valid_tokens(tmp.path(), "srv-a", "token-1");

        // Never seen → first look is a "change" (watermark set).
        assert!(mgr.invalidate_if_disk_changed(tmp.path(), "srv-a"));
        // Unchanged → no invalidation.
        assert!(!mgr.invalidate_if_disk_changed(tmp.path(), "srv-a"));

        // External refresh: mtime moves → detected exactly once.
        let file = oauth::token_dir(tmp.path()).join("srv-a.json");
        touch(&file, 4_000_000_000_000_000_000);
        assert!(mgr.invalidate_if_disk_changed(tmp.path(), "srv-a"));
        assert!(!mgr.invalidate_if_disk_changed(tmp.path(), "srv-a"));

        // note_mtime re-baselines without reporting a change.
        touch(&file, 5_000_000_000_000_000_000);
        mgr.note_mtime(tmp.path(), "srv-a");
        assert!(!mgr.invalidate_if_disk_changed(tmp.path(), "srv-a"));

        // Missing file → no invalidation.
        assert!(!mgr.invalidate_if_disk_changed(tmp.path(), "no-such-server"));
    }

    #[tokio::test]
    async fn handle_401_dedupes_concurrent_recoveries_by_failed_token() {
        // Ensure the singleton exists (the driver task looks it up).
        manager();
        let mgr = OAuthManager::new();
        // The singleton is what spawned drivers consult; mirror its state
        // by running through the real singleton with a unique home.
        let tmp = tempfile::tempdir().unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..8 {
            let attempts = attempts.clone();
            let home = tmp.path().to_path_buf();
            handles.push(tokio::spawn(async move {
                crate::mcp::oauth_manager::manager()
                    .handle_401(&home, "dedup-srv", Some("stale-token"), move || {
                        let attempts = attempts.clone();
                        async move {
                            attempts.fetch_add(1, Ordering::SeqCst);
                            tokio::time::sleep(Duration::from_millis(50)).await;
                            Ok("fresh-token".to_string())
                        }
                    })
                    .await
            }));
        }
        let mut results = Vec::new();
        for handle in handles {
            results.push(handle.await.unwrap().unwrap());
        }
        assert!(results.iter().all(|t| t == "fresh-token"), "{results:?}");
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            1,
            "one recovery for N concurrent 401s"
        );
        drop(mgr);
    }

    #[tokio::test]
    async fn handle_401_distinct_failed_tokens_recover_separately() {
        manager();
        let tmp = tempfile::tempdir().unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for i in 0..2 {
            let attempts = attempts.clone();
            let home = tmp.path().to_path_buf();
            handles.push(tokio::spawn(async move {
                crate::mcp::oauth_manager::manager()
                    .handle_401(
                        &home,
                        "dedup-srv-2",
                        Some(&format!("stale-{}", i)),
                        move || {
                            let attempts = attempts.clone();
                            async move {
                                attempts.fetch_add(1, Ordering::SeqCst);
                                tokio::time::sleep(Duration::from_millis(30)).await;
                                Ok(format!("fresh-{}", i))
                            }
                        },
                    )
                    .await
            }));
        }
        for handle in handles {
            handle.await.unwrap().unwrap();
        }
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn handle_401_external_refresh_short_circuits_recovery() {
        manager();
        let tmp = tempfile::tempdir().unwrap();
        // Baseline watermark at token-1.
        write_valid_tokens(tmp.path(), "ext-srv", "token-1");
        crate::mcp::oauth_manager::manager().note_mtime(tmp.path(), "ext-srv");

        // An external process refreshes the tokens on disk.
        write_valid_tokens(tmp.path(), "ext-srv", "token-external");
        let file = oauth::token_dir(tmp.path()).join("ext-srv.json");
        touch(&file, 6_000_000_000_000_000_000);

        let home = tmp.path().to_path_buf();
        let token = crate::mcp::oauth_manager::manager()
            .handle_401(&home, "ext-srv", Some("stale"), || async move {
                panic!("recovery must not run when disk already refreshed");
            })
            .await
            .unwrap();
        assert_eq!(token, "token-external");
    }

    #[tokio::test]
    async fn handle_401_error_propagates_to_all_waiters() {
        manager();
        let tmp = tempfile::tempdir().unwrap();
        let mut handles = Vec::new();
        for _ in 0..3 {
            let home = tmp.path().to_path_buf();
            handles.push(tokio::spawn(async move {
                crate::mcp::oauth_manager::manager()
                    .handle_401(&home, "err-srv", Some("stale"), || async move {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        Err(AgentError::Tool("refresh rejected".into()))
                    })
                    .await
            }));
        }
        for handle in handles {
            let err = handle.await.unwrap().unwrap_err();
            assert!(err.to_string().contains("refresh rejected"), "{err}");
        }
    }
}

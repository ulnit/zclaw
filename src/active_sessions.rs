//! Cross-process active chat session leases — port of
//! `hermes_cli/active_sessions.py`.
//!
//! The session database records persisted conversations; this registry
//! records currently-open chat surfaces (including idle CLI sessions that
//! have not written a transcript row yet) so a global
//! `[gateway] max_concurrent_sessions` cap can be enforced across processes.
//!
//! Storage: `<home>/runtime/active_sessions.json`, guarded by an exclusive
//! `flock` on `<home>/runtime/active_sessions.lock`. Stale leases are
//! reclaimed by PID liveness checks (paired with the process start time so a
//! recycled PID cannot keep a dead lease alive).

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::UlncLawConfig;

/// Resolve the configured cap: a positive integer, or `None` when disabled
/// (0 / unset). Hermes `coerce_max_concurrent_sessions` + `resolve_*`.
pub fn resolve_max_concurrent_sessions(config: &UlncLawConfig) -> Option<usize> {
    let raw = config.gateway.max_concurrent_sessions?;
    if raw == 0 {
        return None;
    }
    Some(raw as usize)
}

/// Compact age label: `5m`, `2h`, `1h30m` (hermes `format_age`).
pub fn format_age(seconds: f64) -> String {
    let minutes = ((seconds / 60.0).floor() as i64).max(0);
    if minutes < 60 {
        return format!("{minutes}m");
    }
    let hours = minutes / 60;
    let rem = minutes % 60;
    if rem == 0 {
        format!("{hours}h")
    } else {
        format!("{hours}h{rem}m")
    }
}

/// One registry entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEntry {
    pub lease_id: String,
    pub session_id: String,
    pub surface: String,
    pub pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_start_time: Option<f64>,
    pub started_at: f64,
    pub updated_at: f64,
}

/// Compact "who is holding the slots" phrase, e.g. `desktop x4, cli`
/// (hermes `summarize_holders`).
pub fn summarize_holders(entries: &[SessionEntry]) -> String {
    if entries.is_empty() {
        return String::new();
    }
    let mut counts: HashMap<String, usize> = HashMap::new();
    for entry in entries {
        *counts.entry(entry.surface.clone()).or_insert(0) += 1;
    }
    let now = current_time();
    let mut parts: Vec<String> = counts
        .iter()
        .map(|(surface, n)| {
            if *n > 1 {
                format!("{surface} x{n}")
            } else {
                surface.clone()
            }
        })
        .collect();
    // Sort by count desc, then name asc for deterministic output.
    parts.sort_by(|a, b| {
        let ca = counts
            .get(a.split(' ').next().unwrap_or(a))
            .copied()
            .unwrap_or(0);
        let cb = counts
            .get(b.split(' ').next().unwrap_or(b))
            .copied()
            .unwrap_or(0);
        cb.cmp(&ca).then_with(|| a.cmp(b))
    });
    let mut held = parts.join(", ");
    let oldest = entries
        .iter()
        .map(|e| e.started_at)
        .fold(f64::INFINITY, f64::min);
    if oldest.is_finite() {
        held.push_str(&format!(", oldest {} ago", format_age(now - oldest)));
    }
    held
}

/// The rejection message when the cap is hit (hermes
/// `active_session_limit_message`).
pub fn active_session_limit_message(
    active_count: usize,
    max_sessions: usize,
    entries: &[SessionEntry],
) -> String {
    let held = summarize_holders(entries);
    let detail = if held.is_empty() {
        String::new()
    } else {
        format!(" Held by: {held}.")
    };
    format!(
        "ulnclaw is at the active session limit ({active_count}/{max_sessions}).{detail} \
         Try again when another session finishes."
    )
}

fn current_time() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn runtime_dir() -> PathBuf {
    crate::config::ulnclaw_home().join("runtime")
}

fn state_path() -> PathBuf {
    runtime_dir().join("active_sessions.json")
}

fn lock_path() -> PathBuf {
    runtime_dir().join("active_sessions.lock")
}

/// Exclusive advisory file lock (hermes `_FileLock`, flock-based).
struct FileLock {
    file: File,
}

impl FileLock {
    fn acquire(path: &Path) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(path)?;
        // flock on unix, LockFileEx on Windows (hermes `_FileLock`).
        crate::process_ctl::lock_exclusive(&file)?;
        Ok(Self { file })
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = crate::process_ctl::unlock(&self.file);
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Registry {
    entries: Vec<SessionEntry>,
}

fn read_entries(path: &Path) -> Vec<SessionEntry> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    match serde_json::from_str::<Registry>(&text) {
        Ok(reg) => reg.entries,
        Err(_) => {
            tracing::warn!(
                "Ignoring corrupt active session registry at {}",
                path.display()
            );
            Vec::new()
        }
    }
}

fn write_entries(path: &Path, entries: &[SessionEntry]) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    let registry = Registry {
        entries: entries.to_vec(),
    };
    if let Ok(mut file) = File::create(&tmp) {
        if serde_json::to_writer(&mut file, &registry).is_ok() {
            let _ = file.flush();
            drop(file);
            let _ = fs::rename(&tmp, path);
            return;
        }
    }
    let _ = fs::remove_file(&tmp);
}

/// Process start time (seconds since boot) from `/proc/<pid>/stat` — a
/// stable per-incarnation value so a recycled PID cannot spoof liveness
/// (hermes `_process_start_time` via psutil create_time).
#[cfg(unix)]
fn process_start_time(pid: u32) -> Option<f64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Field 2 (comm) may contain spaces/parens; find the last ')' first.
    let after_comm = stat.rfind(')')? + 1;
    let fields: Vec<&str> = stat[after_comm..].split_whitespace().collect();
    // After ')' the next field is state (field 3); starttime is field 22,
    // i.e. index 22 - 3 = 19 in this slice.
    let ticks: f64 = fields.get(19)?.parse().ok()?;
    let clk_tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
    if clk_tck <= 0.0 {
        return None;
    }
    Some(ticks / clk_tck)
}

/// No `/proc` on this platform — start times unavailable.
#[cfg(not(unix))]
fn process_start_time(_pid: u32) -> Option<f64> {
    None
}

fn pid_alive(pid: u32, expected_start: Option<f64>) -> bool {
    if pid == 0 {
        return false;
    }
    if !crate::process_ctl::alive(pid) {
        return false;
    }
    let Some(expected) = expected_start else {
        return true;
    };
    let Some(current) = process_start_time(pid) else {
        return true;
    };
    (current - expected).abs() < 0.001
}

fn prune_dead(entries: Vec<SessionEntry>) -> Vec<SessionEntry> {
    entries
        .into_iter()
        .filter(|entry| pid_alive(entry.pid, entry.process_start_time))
        .collect()
}

/// An acquired (or no-op) active-session slot. Dropping the lease releases
/// the slot (hermes `ActiveSessionLease.release`).
pub struct ActiveSessionLease {
    pub lease_id: String,
    pub session_id: String,
    pub surface: String,
    pub enabled: bool,
    released: bool,
}

impl ActiveSessionLease {
    pub fn release(&mut self) {
        if self.released || !self.enabled {
            self.released = true;
            return;
        }
        release_active_session(self);
    }
}

impl Drop for ActiveSessionLease {
    fn drop(&mut self) {
        self.release();
    }
}

/// Acquire an active-session slot (hermes `try_acquire_active_session`).
/// Returns `(Some(lease), None)` on success; `(None, Some(message))` when
/// the cap is reached. With the cap disabled the lease is a no-op object.
pub fn try_acquire_active_session(
    session_id: &str,
    surface: &str,
    config: &UlncLawConfig,
) -> (Option<ActiveSessionLease>, Option<String>) {
    let lease_id = uuid::Uuid::new_v4().simple().to_string();
    let Some(max_sessions) = resolve_max_concurrent_sessions(config) else {
        return (
            Some(ActiveSessionLease {
                lease_id,
                session_id: session_id.to_string(),
                surface: surface.to_string(),
                enabled: false,
                released: false,
            }),
            None,
        );
    };

    let now = current_time();
    let pid = std::process::id();
    let entry = SessionEntry {
        lease_id: lease_id.clone(),
        session_id: session_id.to_string(),
        surface: surface.to_string(),
        pid,
        process_start_time: process_start_time(pid),
        started_at: now,
        updated_at: now,
    };

    let state = state_path();
    let _lock = match FileLock::acquire(&lock_path()) {
        Ok(lock) => lock,
        Err(e) => {
            tracing::warn!("active session lock unavailable: {e}");
            return (
                None,
                Some("active session file lock unavailable".to_string()),
            );
        }
    };
    let raw_entries = read_entries(&state);
    let entries = prune_dead(raw_entries.clone());
    let pruned = raw_entries.len() - entries.len();
    if pruned > 0 {
        tracing::info!("Pruned {pruned} stale active session lease(s)");
    }
    if entries.len() >= max_sessions {
        write_entries(&state, &entries);
        tracing::info!(
            "Active session limit reached: active={} max={} surface={surface}",
            entries.len(),
            max_sessions
        );
        return (
            None,
            Some(active_session_limit_message(
                entries.len(),
                max_sessions,
                &entries,
            )),
        );
    }
    let mut entries = entries;
    entries.push(entry);
    write_entries(&state, &entries);
    drop(_lock);

    (
        Some(ActiveSessionLease {
            lease_id,
            session_id: session_id.to_string(),
            surface: surface.to_string(),
            enabled: true,
            released: false,
        }),
        None,
    )
}

/// Release a lease (hermes `release_active_session`).
pub fn release_active_session(lease: &mut ActiveSessionLease) {
    let state = state_path();
    if let Ok(_lock) = FileLock::acquire(&lock_path()) {
        let entries = prune_dead(read_entries(&state));
        let kept: Vec<SessionEntry> = entries
            .into_iter()
            .filter(|entry| entry.lease_id != lease.lease_id)
            .collect();
        write_entries(&state, &kept);
    }
    lease.released = true;
}

/// Move an existing lease to a new session id without dropping the slot
/// (hermes `transfer_active_session`).
pub fn transfer_active_session(lease: &mut ActiveSessionLease, new_session_id: &str) -> bool {
    if new_session_id.is_empty() || lease.released {
        return false;
    }
    if !lease.enabled {
        lease.session_id = new_session_id.to_string();
        return true;
    }
    let state = state_path();
    let Ok(_lock) = FileLock::acquire(&lock_path()) else {
        return false;
    };
    let mut entries = prune_dead(read_entries(&state));
    let mut updated = false;
    for entry in entries.iter_mut() {
        if entry.lease_id != lease.lease_id {
            continue;
        }
        entry.session_id = new_session_id.to_string();
        entry.updated_at = current_time();
        updated = true;
        break;
    }
    if updated {
        write_entries(&state, &entries);
        lease.session_id = new_session_id.to_string();
    }
    updated
}

/// Drop this process's registry entries that no live session owns (hermes
/// `release_orphaned_leases`).
pub fn release_orphaned_leases(live_lease_ids: &std::collections::HashSet<String>) -> usize {
    let pid = std::process::id();
    let state = state_path();
    if !state.exists() {
        return 0;
    }
    let Ok(_lock) = FileLock::acquire(&lock_path()) else {
        return 0;
    };
    let entries = prune_dead(read_entries(&state));
    let total = entries.len();
    let kept: Vec<SessionEntry> = entries
        .into_iter()
        .filter(|entry| entry.pid != pid || live_lease_ids.contains(&entry.lease_id))
        .collect();
    let dropped = total - kept.len();
    if dropped > 0 {
        write_entries(&state, &kept);
    }
    dropped
}

/// Pruned registry snapshot for diagnostics/tests (hermes
/// `active_session_registry_snapshot`).
pub fn active_session_registry_snapshot() -> Vec<SessionEntry> {
    let state = state_path();
    let Ok(_lock) = FileLock::acquire(&lock_path()) else {
        return Vec::new();
    };
    let entries = prune_dead(read_entries(&state));
    write_entries(&state, &entries);
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_cap(cap: Option<u32>) -> UlncLawConfig {
        let mut config = UlncLawConfig::default();
        config.gateway.max_concurrent_sessions = cap;
        config
    }

    #[test]
    fn resolve_cap_zero_and_unset_disable() {
        assert_eq!(
            resolve_max_concurrent_sessions(&config_with_cap(None)),
            None
        );
        assert_eq!(
            resolve_max_concurrent_sessions(&config_with_cap(Some(0))),
            None
        );
        assert_eq!(
            resolve_max_concurrent_sessions(&config_with_cap(Some(3))),
            Some(3)
        );
    }

    #[test]
    fn format_age_labels() {
        assert_eq!(format_age(30.0), "0m");
        assert_eq!(format_age(300.0), "5m");
        assert_eq!(format_age(7200.0), "2h");
        assert_eq!(format_age(5400.0), "1h30m");
    }

    #[test]
    fn holders_summary_groups_surfaces() {
        let now = current_time();
        let entries = vec![
            SessionEntry {
                lease_id: "a".into(),
                session_id: "s1".into(),
                surface: "cli".into(),
                pid: 1,
                process_start_time: None,
                started_at: now - 3600.0,
                updated_at: now,
            },
            SessionEntry {
                lease_id: "b".into(),
                session_id: "s2".into(),
                surface: "gateway".into(),
                pid: 1,
                process_start_time: None,
                started_at: now - 60.0,
                updated_at: now,
            },
            SessionEntry {
                lease_id: "c".into(),
                session_id: "s3".into(),
                surface: "gateway".into(),
                pid: 1,
                process_start_time: None,
                started_at: now - 30.0,
                updated_at: now,
            },
        ];
        let summary = summarize_holders(&entries);
        assert!(summary.contains("gateway x2"), "{summary}");
        assert!(summary.contains("cli"), "{summary}");
        assert!(summary.contains("oldest 1h ago"), "{summary}");
        let message = active_session_limit_message(3, 3, &entries);
        assert!(message.contains("3/3"));
        assert!(message.contains("Held by"));
    }

    #[test]
    fn acquire_respects_cap_and_releases() {
        let _guard = crate::models_dev::test_env_lock();
        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var("ULNCLAW_HOME").ok();
        std::env::set_var("ULNCLAW_HOME", dir.path());

        let config = config_with_cap(Some(2));
        let (lease_a, err_a) = try_acquire_active_session("s1", "cli", &config);
        assert!(lease_a.is_some() && err_a.is_none());
        let (lease_b, err_b) = try_acquire_active_session("s2", "gateway", &config);
        assert!(lease_b.is_some() && err_b.is_none());

        let snapshot = active_session_registry_snapshot();
        assert_eq!(snapshot.len(), 2);

        let (lease_c, err_c) = try_acquire_active_session("s3", "cli", &config);
        assert!(lease_c.is_none());
        let message = err_c.unwrap();
        assert!(message.contains("2/2"), "{message}");
        assert!(message.contains("Held by"), "{message}");

        // Release one slot → the next acquire succeeds.
        let mut lease_a = lease_a.unwrap();
        lease_a.release();
        let (lease_d, err_d) = try_acquire_active_session("s4", "cli", &config);
        assert!(lease_d.is_some() && err_d.is_none());

        // Transfer the surviving lease to a new session id.
        let mut lease_b = lease_b.unwrap();
        assert!(transfer_active_session(&mut lease_b, "s2-renamed"));
        let snapshot = active_session_registry_snapshot();
        assert!(snapshot.iter().any(|e| e.session_id == "s2-renamed"));

        drop(lease_b);
        drop(lease_d);
        match prev {
            Some(v) => std::env::set_var("ULNCLAW_HOME", v),
            None => std::env::remove_var("ULNCLAW_HOME"),
        }
    }

    #[test]
    fn disabled_cap_yields_noop_lease() {
        let _guard = crate::models_dev::test_env_lock();
        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var("ULNCLAW_HOME").ok();
        std::env::set_var("ULNCLAW_HOME", dir.path());

        let config = config_with_cap(None);
        let (lease, err) = try_acquire_active_session("s1", "cli", &config);
        assert!(err.is_none());
        let lease = lease.unwrap();
        assert!(!lease.enabled);
        // Registry is never written when the cap is disabled.
        assert!(!state_path().exists());

        match prev {
            Some(v) => std::env::set_var("ULNCLAW_HOME", v),
            None => std::env::remove_var("ULNCLAW_HOME"),
        }
    }

    #[test]
    fn dead_pids_are_pruned() {
        let _guard = crate::models_dev::test_env_lock();
        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var("ULNCLAW_HOME").ok();
        std::env::set_var("ULNCLAW_HOME", dir.path());

        // Spawn and kill a child so we have a guaranteed-dead PID.
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("spawn sleep");
        let dead_pid = child.id();
        let _ = child.kill();
        let _ = child.wait();

        let now = current_time();
        let entries = vec![
            SessionEntry {
                lease_id: "dead".into(),
                session_id: "s-dead".into(),
                surface: "cli".into(),
                pid: dead_pid,
                process_start_time: None,
                started_at: now,
                updated_at: now,
            },
            SessionEntry {
                lease_id: "live".into(),
                session_id: "s-live".into(),
                surface: "cli".into(),
                pid: std::process::id(),
                process_start_time: process_start_time(std::process::id()),
                started_at: now,
                updated_at: now,
            },
        ];
        write_entries(&state_path(), &entries);

        let snapshot = active_session_registry_snapshot();
        assert_eq!(snapshot.len(), 1, "dead lease pruned");
        assert_eq!(snapshot[0].lease_id, "live");

        match prev {
            Some(v) => std::env::set_var("ULNCLAW_HOME", v),
            None => std::env::remove_var("ULNCLAW_HOME"),
        }
    }

    #[test]
    fn orphaned_leases_are_released() {
        let _guard = crate::models_dev::test_env_lock();
        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var("ULNCLAW_HOME").ok();
        std::env::set_var("ULNCLAW_HOME", dir.path());

        let config = config_with_cap(Some(5));
        let (lease_a, _) = try_acquire_active_session("s1", "cli", &config);
        let (lease_b, _) = try_acquire_active_session("s2", "cli", &config);
        let lease_a = lease_a.unwrap();
        let lease_b = lease_b.unwrap();

        // Only lease_a is live → lease_b's entry is orphaned.
        let mut live = std::collections::HashSet::new();
        live.insert(lease_a.lease_id.clone());
        let dropped = release_orphaned_leases(&live);
        assert_eq!(dropped, 1);
        let snapshot = active_session_registry_snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].lease_id, lease_a.lease_id);
        let _ = lease_b;

        match prev {
            Some(v) => std::env::set_var("ULNCLAW_HOME", v),
            None => std::env::remove_var("ULNCLAW_HOME"),
        }
    }
}

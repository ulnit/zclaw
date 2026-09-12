//! Cgroup orphan reaper — port of hermes `gateway/cgroup_cleanup.py`.
//!
//! SIGKILL any process left in this systemd unit's cgroup. hermes runs
//! the Python original as `ExecStopPost=` so it only fires after the
//! gateway's main process has exited; the gateway already reaps its own
//! tool subprocesses on a clean shutdown, and this is the safety net for
//! long-lived helpers it doesn't track (`adb`, platform bridges, etc.)
//! that would otherwise be orphaned in the cgroup and block
//! `Restart=always` (hermes issue #37454).
//!
//! We deliberately iterate `cgroup.procs` and send per-PID SIGKILLs
//! instead of writing `1` to `cgroup.kill`: the original failure mode in
//! #37454 was the kernel returning `EINVAL` on the cgroup-wide kill,
//! while per-PID signal delivery uses a separate code path that still
//! works.
//!
//! ⚠️ systemd-only: the hermes original is an `ExecStopPost=` hook, so it
//! runs *inside the unit's own cgroup* after the gateway exits. When the
//! gateway is spawned by anything else (a desktop Supervisor, Tauri shell,
//! plain shell), it *inherits the parent's cgroup* — reaping then
//! SIGKILLs the supervisor and every sibling process of the session
//! (observed: restarting one component killed the whole edge-runtime +
//! futu-gateway). Callers MUST gate on [`under_systemd_unit`].

/// True iff this process was started by systemd as a service unit.
///
/// systemd sets `INVOCATION_ID` for every unit-launched process (same
/// probe `shutdown_forensics` uses). Under a desktop supervisor / shell
/// the var is absent → reaping the shared cgroup would be fratricide.
pub fn under_systemd_unit() -> bool {
    std::env::var("INVOCATION_ID")
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
}

/// Parse the cgroup v2 path out of `/proc/self/cgroup` contents.
///
/// cgroup v2 unified hierarchy lines look like `0::<path>`; anything
/// else (legacy v1 controllers) is ignored.
pub fn parse_cgroup_v2_path(contents: &str) -> Option<String> {
    for line in contents.lines() {
        if let Some(rest) = line.strip_prefix("0::") {
            let path = rest.trim();
            if !path.is_empty() {
                return Some(path.to_string());
            }
        }
    }
    None
}

/// Parse PIDs out of a `cgroup.procs` file (one PID per line, blank or
/// junk lines skipped).
pub fn parse_procs(raw: &str) -> Vec<u32> {
    raw.lines()
        .filter_map(|line| line.trim().parse::<u32>().ok())
        .collect()
}

/// Return the cgroup v2 path for the calling process, or `None` when
/// `/proc/self/cgroup` is unreadable or has no unified-hierarchy entry.
#[cfg(target_os = "linux")]
pub fn own_cgroup_path() -> Option<String> {
    let text = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    parse_cgroup_v2_path(&text)
}

/// Read the PIDs currently in the cgroup at `/sys/fs/cgroup<path>`.
#[cfg(target_os = "linux")]
pub fn read_cgroup_pids(cgroup_path: &str) -> Vec<u32> {
    let procs_file = format!("/sys/fs/cgroup{cgroup_path}/cgroup.procs");
    match std::fs::read_to_string(procs_file) {
        Ok(raw) => parse_procs(&raw),
        Err(_) => Vec::new(),
    }
}

/// SIGKILL every PID in the cgroup other than the caller. Returns the
/// number of signals delivered. `ProcessLookupError` (already gone) and
/// `PermissionError` (not ours to kill) are skipped silently, matching
/// hermes.
#[cfg(target_os = "linux")]
pub fn reap_cgroup(cgroup_path: Option<&str>) -> u64 {
    let owned;
    let path = match cgroup_path {
        Some(p) => p,
        None => {
            // Auto-detect path is ONLY safe under a systemd unit: the
            // hermes original runs as `ExecStopPost=` inside the unit's
            // own cgroup. Spawned by a supervisor/Tauri/shell instead,
            // `/proc/self/cgroup` points at the *parent's* cgroup and
            // reaping would SIGKILL the supervisor and every sibling
            // (fratricide observed on the ClawQuant desktop edge-runtime).
            if !under_systemd_unit() {
                return 0;
            }
            owned = own_cgroup_path();
            match owned.as_deref() {
                Some(p) => p,
                None => return 0,
            }
        }
    };
    let own = std::process::id();
    let mut killed = 0u64;
    for pid in read_cgroup_pids(path) {
        if pid == own {
            continue;
        }
        // SAFETY: libc::kill with SIGKILL against a PID read from our
        // own cgroup; the signal path is the hermes-mandated per-PID
        // delivery. Errors are benign (process raced away / no perms).
        let rc = unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        if rc == 0 {
            killed += 1;
        }
    }
    killed
}

/// Non-Linux stub: the reaper reads `/proc` and `/sys/fs/cgroup` and
/// only makes sense under systemd, so other platforms report nothing.
#[cfg(not(target_os = "linux"))]
pub fn own_cgroup_path() -> Option<String> {
    None
}

/// Non-Linux stub — see [`own_cgroup_path`].
#[cfg(not(target_os = "linux"))]
pub fn read_cgroup_pids(_cgroup_path: &str) -> Vec<u32> {
    Vec::new()
}

/// Non-Linux stub — see [`own_cgroup_path`].
#[cfg(not(target_os = "linux"))]
pub fn reap_cgroup(_cgroup_path: Option<&str>) -> u64 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// INVOCATION_ID 是进程级环境变量；涉及它的测试必须互斥执行，
    /// 否则 reap 测试可能读到另一测试刚 set 的值 → 真的去 SIGKILL 共享 cgroup。
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn test_parse_cgroup_v2_path_unified() {
        let text = "12:pids:/user.slice\n11:memory:/user.slice\n0::/system.slice/ulnclaw-gateway.service\n";
        assert_eq!(
            parse_cgroup_v2_path(text),
            Some("/system.slice/ulnclaw-gateway.service".to_string())
        );
    }

    #[test]
    fn test_parse_cgroup_v2_path_root() {
        assert_eq!(parse_cgroup_v2_path("0::/\n"), Some("/".to_string()));
    }

    #[test]
    fn test_parse_cgroup_v2_path_legacy_only() {
        let text = "12:pids:/user.slice\n11:memory:/user.slice\n";
        assert_eq!(parse_cgroup_v2_path(text), None);
    }

    #[test]
    fn test_parse_cgroup_v2_path_empty_entry() {
        assert_eq!(parse_cgroup_v2_path("0::\n"), None);
    }

    #[test]
    fn test_parse_procs_mixed() {
        let raw = "101\n\n  202  \njunk\n303\n";
        assert_eq!(parse_procs(raw), vec![101, 202, 303]);
    }

    #[test]
    fn test_parse_procs_empty() {
        assert!(parse_procs("").is_empty());
    }

    #[test]
    fn test_under_systemd_unit_follows_invocation_id() {
        // 串行锁：INVOCATION_ID 是进程级 env，两测试并行会竞态 ——
        // 最坏情形 reap 测试恰读到另一测试设置的值 → 真去 SIGKILL
        // 共享 cgroup 里的 cargo test 进程。必须互斥。
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::remove_var("INVOCATION_ID") };
        assert!(
            !under_systemd_unit(),
            "no INVOCATION_ID → not a systemd unit"
        );
        unsafe { std::env::set_var("INVOCATION_ID", "  ") };
        assert!(
            !under_systemd_unit(),
            "blank INVOCATION_ID → not a systemd unit"
        );
        unsafe { std::env::set_var("INVOCATION_ID", "d3e9a1c2b4f6") };
        assert!(under_systemd_unit(), "set INVOCATION_ID → systemd unit");
        unsafe { std::env::remove_var("INVOCATION_ID") };
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_reap_cgroup_guarded_outside_systemd() {
        // 非 systemd 环境（Supervisor/Tauri/shell 拉起）自动路径必须 0 杀 ——
        // 防 fratricide（共享 cgroup 里的 supervisor 和兄弟进程）。
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::remove_var("INVOCATION_ID") };
        assert_eq!(reap_cgroup(None), 0, "must not reap outside a systemd unit");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_own_cgroup_path_readable() {
        // On any Linux host (container included) /proc/self/cgroup
        // exists; whether it carries a v2 entry is environment
        // dependent, so just make sure the call is panic-free.
        let _ = own_cgroup_path();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_read_cgroup_pids_missing_dir() {
        assert!(read_cgroup_pids("/nonexistent-ulnclaw-cgroup").is_empty());
    }
}

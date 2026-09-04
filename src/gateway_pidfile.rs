//! Gateway single-instance guard — port of the hermes
//! `gateway.status.get_running_pid` + `gateway run --replace` contract
//! (v2026.8.3).
//!
//! The running gateway writes `<home>/gateway.pid` carrying its pid and
//! a process-start token. A fresh start reads the record, verifies the
//! process is alive AND the start token matches (the PID-reuse guard —
//! hermes `start_time` check), then refuses (`--force` bypasses) or
//! terminates and replaces the old instance (`--replace`). Stale
//! records (dead pid / reused pid) are cleaned up best-effort, exactly
//! like hermes `cleanup_stale`.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Pidfile name under the ulnclaw home (hermes `_get_pid_path`).
pub const GATEWAY_PIDFILE_NAME: &str = "gateway.pid";

/// One pidfile entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PidRecord {
    pub pid: u32,
    /// Process-start token: Linux `/proc/<pid>/stat` starttime
    /// (clock ticks since boot). `None` on platforms without `/proc`,
    /// where the liveness check degrades to `kill(pid, 0)` only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<u64>,
}

/// Path of the gateway pidfile for a given home.
pub fn pidfile_path(home: &Path) -> PathBuf {
    home.join(GATEWAY_PIDFILE_NAME)
}

/// Start token for `pid` — field 22 (`starttime`) of
/// Process-start token used as the PID-reuse guard. Unix reads field 22
/// of `/proc/<pid>/stat` (the comm field is parenthesized and may
/// contain spaces, so parsing anchors on the LAST `)`); Windows reads
/// the process creation time via `GetProcessTimes`.
#[cfg(unix)]
pub fn process_start_time(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = stat.rfind(')')?;
    let rest = stat.get(close + 2..)?; // skip ") "
                                       // stat field 22 == whitespace token index 19 after the comm field
                                       // (token 0 is field 3, `state`).
    let field = rest.split_whitespace().nth(19)?;
    field.parse().ok()
}

/// Windows PID-reuse token: the process creation time (100-ns ticks
/// since the Windows epoch) — a recycled pid carries a different
/// creation time, so a stale pidfile pointing at a reused pid is
/// detected exactly like the `/proc` starttime on unix.
#[cfg(windows)]
pub fn process_start_time(pid: u32) -> Option<u64> {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    if pid == 0 {
        return None;
    }
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle == 0 {
            return None;
        }
        let mut creation: FILETIME = std::mem::zeroed();
        let mut exit: FILETIME = std::mem::zeroed();
        let mut kernel: FILETIME = std::mem::zeroed();
        let mut user: FILETIME = std::mem::zeroed();
        let ok = GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) != 0;
        CloseHandle(handle);
        if !ok {
            return None;
        }
        Some(((creation.dwHighDateTime as u64) << 32) | creation.dwLowDateTime as u64)
    }
}

/// `/proc/<pid>/stat` state character (`S`, `R`, `Z`, …), when
/// readable.
pub fn process_state(pid: u32) -> Option<char> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = stat.rfind(')')?;
    stat.get(close + 2..)?.chars().next()
}

/// Best-effort process image name (`ulnclaw`, `python`, …) used as a
/// PID-reuse fallback when a legacy pidfile carries no start token: a
/// live pid whose executable is not ulnclaw means the kernel recycled
/// the pid for an unrelated process (hermes start_time spirit).
#[cfg(unix)]
pub fn process_image_name(pid: u32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|comm| comm.trim().to_string())
        .filter(|name| !name.is_empty())
}

/// Windows image name via `QueryFullProcessImageNameW` (works with
/// `PROCESS_QUERY_LIMITED_INFORMATION`), reduced to the file stem.
#[cfg(windows)]
pub fn process_image_name(pid: u32) -> Option<String> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    if pid == 0 {
        return None;
    }
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle == 0 {
            return None;
        }
        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        let ok =
            QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len) != 0;
        CloseHandle(handle);
        if !ok {
            return None;
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        let stem = path
            .rsplit(['\\', '/'])
            .next()
            .unwrap_or(&path)
            .trim_end_matches(".exe")
            .trim_end_matches(".EXE");
        if stem.is_empty() {
            None
        } else {
            Some(stem.to_string())
        }
    }
}

/// True iff `pid` refers to a live process. Zombies count as dead —
/// `/proc` state wins when available; otherwise `kill(pid, 0)` probes
/// (EPERM means "exists but owned by someone else", i.e. alive).
pub fn is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    if let Some(state) = process_state(pid) {
        return state != 'Z' && state != 'X' && state != 'x';
    }
    crate::process_ctl::alive(pid)
}

/// Write the pidfile for the CURRENT process (best-effort caller
/// handles errors; a missing pidfile only weakens the guard).
pub fn write_pidfile(home: &Path) -> std::io::Result<()> {
    let pid = std::process::id();
    let record = PidRecord {
        pid,
        started_at: process_start_time(pid),
    };
    let json = serde_json::to_string(&record)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    std::fs::write(pidfile_path(home), json + "\n")
}

/// PID of the gateway recorded in the pidfile, if that process is
/// alive and the start token matches the record. Stale records are
/// removed best-effort (hermes `cleanup_stale`).
pub fn running_gateway_pid(home: &Path) -> Option<u32> {
    let path = pidfile_path(home);
    let data = std::fs::read_to_string(&path).ok()?;
    let record: PidRecord = serde_json::from_str(data.trim()).ok()?;
    if record.pid == 0 || !is_alive(record.pid) {
        let _ = std::fs::remove_file(&path);
        return None;
    }
    if let (Some(recorded), Some(current)) = (record.started_at, process_start_time(record.pid)) {
        if recorded != current {
            // The pid was recycled by the kernel for another process.
            let _ = std::fs::remove_file(&path);
            return None;
        }
    } else if let Some(name) = process_image_name(record.pid) {
        // Legacy record without a usable start token: only trust it
        // when the live process is actually ulnclaw. A reused pid
        // running python.exe / anything else is stale, not a gateway.
        if !name.to_ascii_lowercase().contains("ulnclaw") {
            let _ = std::fs::remove_file(&path);
            return None;
        }
    }
    Some(record.pid)
}

/// Terminate a running gateway so this start can take over (hermes
/// `gateway run --replace`): SIGTERM, poll until exit, escalate to
/// SIGKILL after `term_timeout`.
pub fn replace_running(pid: u32, term_timeout: Duration) -> Result<(), String> {
    if let Err(err) = crate::process_ctl::terminate(pid) {
        if err.kind() == std::io::ErrorKind::NotFound {
            return Ok(()); // already gone
        }
        return Err(format!("terminate pid {pid}: {err}"));
    }
    let deadline = std::time::Instant::now() + term_timeout;
    while std::time::Instant::now() < deadline {
        if !is_alive(pid) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = crate::process_ctl::kill_hard(pid);
    let kill_deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < kill_deadline {
        if !is_alive(pid) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(format!("gateway pid {pid} survived SIGTERM and SIGKILL"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn pidfile_roundtrip_detects_current_process() {
        let home = temp_home();
        write_pidfile(home.path()).unwrap();
        assert_eq!(running_gateway_pid(home.path()), Some(std::process::id()));
    }

    #[test]
    fn stale_pidfile_is_cleaned_up() {
        let home = temp_home();
        // Spawn and reap a child so its pid is definitely dead.
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn true");
        let dead_pid = child.id();
        child.wait().unwrap();
        let record = PidRecord {
            pid: dead_pid,
            started_at: None,
        };
        std::fs::write(
            pidfile_path(home.path()),
            serde_json::to_string(&record).unwrap(),
        )
        .unwrap();
        assert_eq!(running_gateway_pid(home.path()), None);
        assert!(!pidfile_path(home.path()).exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn legacy_record_pointing_at_foreign_process_is_stale() {
        let home = temp_home();
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .expect("spawn sleep");
        let foreign_pid = child.id();
        let record = PidRecord {
            pid: foreign_pid,
            started_at: None,
        };
        std::fs::write(
            pidfile_path(home.path()),
            serde_json::to_string(&record).unwrap(),
        )
        .unwrap();
        // alive but not ulnclaw -> recycled pid, record is stale
        assert_eq!(running_gateway_pid(home.path()), None);
        assert!(!pidfile_path(home.path()).exists());
        let _ = child.kill();
        let _ = child.wait();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn start_time_mismatch_means_pid_reuse() {
        let home = temp_home();
        let pid = std::process::id();
        let real = process_start_time(pid).expect("/proc starttime");
        let record = PidRecord {
            pid,
            started_at: Some(real.wrapping_add(12345)),
        };
        std::fs::write(
            pidfile_path(home.path()),
            serde_json::to_string(&record).unwrap(),
        )
        .unwrap();
        assert_eq!(running_gateway_pid(home.path()), None);
        assert!(!pidfile_path(home.path()).exists());
    }

    #[test]
    fn replace_terminates_running_process() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();
        assert!(is_alive(pid));
        replace_running(pid, Duration::from_secs(3)).unwrap();
        // Reap and confirm the process is gone.
        let status = child.wait().unwrap();
        assert!(!is_alive(pid));
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            assert!(status.signal().is_some());
        }
    }

    #[test]
    fn replace_missing_pid_is_ok() {
        // ESRCH path: replacing an already-dead pid is not an error.
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn true");
        let dead_pid = child.id();
        child.wait().unwrap();
        assert!(replace_running(dead_pid, Duration::from_secs(1)).is_ok());
    }
}

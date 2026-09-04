//! Shutdown forensics — capture context when the gateway receives
//! SIGTERM/SIGINT. Port of hermes `gateway/shutdown_forensics.py`.
//!
//! The gateway's shutdown path wants a durable record of who/what
//! triggered the shutdown so that "the gateway keeps dying" incidents
//! can be diagnosed after the fact.
//!
//! [`snapshot_shutdown_context`] is a fast (<10ms), non-blocking probe
//! returning a structured snapshot the signal handler can log
//! immediately; [`spawn_async_diagnostic`] is a fire-and-forget `ps`
//! walk run as a detached subprocess so it can't block teardown even
//! if `/proc` is wedged. Anything that needs to wait belongs in the
//! async helper, never in the synchronous probe.

use std::path::Path;

use serde_json::{json, Value};

/// Human-readable signal name (hermes `_signal_name`).
pub fn signal_name(sig: i32) -> String {
    let name = match sig {
        libc::SIGTERM => "SIGTERM",
        libc::SIGINT => "SIGINT",
        #[cfg(unix)]
        libc::SIGHUP => "SIGHUP",
        #[cfg(unix)]
        libc::SIGQUIT => "SIGQUIT",
        #[cfg(not(windows))]
        libc::SIGUSR1 => "SIGUSR1",
        #[cfg(not(windows))]
        libc::SIGUSR2 => "SIGUSR2",
        _ => return format!("signal#{sig}"),
    };
    name.to_string()
}

/// Read a single field from `/proc/<pid>/status` (Linux only; `None`
/// elsewhere — hermes `_read_proc_field`).
fn read_proc_field(pid: u32, key: &str) -> Option<String> {
    let content = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix(&format!("{key}:")) {
            return Some(rest.trim().to_string());
        }
    }
    None
}

/// Read `/proc/<pid>/cmdline` as a printable string (Linux only;
/// `None` elsewhere — hermes `_read_proc_cmdline`).
fn read_proc_cmdline(pid: u32) -> Option<String> {
    let data = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    if data.is_empty() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&data)
            .replace('\0', " ")
            .trim()
            .to_string(),
    )
}

/// Compact `/proc/<pid>` snapshot: pid, name, ppid, state, uid,
/// cmdline (hermes `_proc_summary`). Best-effort — missing fields are
/// simply omitted.
pub fn proc_summary(pid: u32) -> Value {
    let mut summary = json!({ "pid": pid });
    if pid == 0 {
        return summary;
    }
    let map = summary.as_object_mut().unwrap();
    if let Some(name) = read_proc_field(pid, "Name") {
        map.insert("name".into(), Value::String(name));
    }
    if let Some(state) = read_proc_field(pid, "State") {
        map.insert("state".into(), Value::String(state));
    }
    if let Some(ppid) = read_proc_field(pid, "PPid").and_then(|raw| raw.parse::<u32>().ok()) {
        map.insert("ppid".into(), json!(ppid));
    }
    if let Some(uid) = read_proc_field(pid, "Uid") {
        // "real effective saved fs" — keep the real uid.
        let real = uid.split_whitespace().next().unwrap_or("").to_string();
        map.insert("uid".into(), Value::String(real));
    }
    if let Some(cmdline) = read_proc_cmdline(pid) {
        // Truncate aggressively — these can be 4KB.
        let truncated: String = cmdline.chars().take(300).collect();
        map.insert("cmdline".into(), Value::String(truncated));
    }
    summary
}

/// Fast (<10ms) snapshot of who/what is asking us to shut down
/// (hermes `snapshot_shutdown_context`).
///
/// Captures the signal number/name, our own PID/ppid + parent process
/// info from `/proc`, whether systemd is our parent (`ppid==1` or
/// `INVOCATION_ID` set), `/proc` load average, and any attached
/// tracer (debugger/strace). Pure stdlib, never panics, never blocks
/// on subprocesses.
pub fn snapshot_shutdown_context(received_signal: Option<i32>) -> Value {
    let pid = std::process::id();
    let ppid = parent_pid().unwrap_or(0);
    let mut ctx = json!({
        "ts": chrono::Utc::now().to_rfc3339(),
        "signal": received_signal.map(signal_name).unwrap_or_else(|| "UNKNOWN".into()),
        "signal_num": received_signal,
        "pid": pid,
        "ppid": ppid,
        "parent": proc_summary(ppid),
        "self": proc_summary(pid),
    });
    let map = ctx.as_object_mut().unwrap();

    // systemd context: a unit-started process carries INVOCATION_ID;
    // ppid==1 (init) is also a strong signal that systemd reaped and
    // forwarded the SIGTERM.
    if let Ok(invocation_id) = std::env::var("INVOCATION_ID") {
        if !invocation_id.trim().is_empty() {
            map.insert("systemd_invocation_id".into(), Value::String(invocation_id));
        }
    }
    if let Ok(journal_stream) = std::env::var("JOURNAL_STREAM") {
        if !journal_stream.trim().is_empty() {
            map.insert(
                "systemd_journal_stream".into(),
                Value::String(journal_stream),
            );
        }
    }
    let under_systemd = std::env::var("INVOCATION_ID")
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
        || ppid == 1;
    map.insert("under_systemd".into(), Value::Bool(under_systemd));

    // Load average — high load points the finger at "something else
    // crushing the box" rather than "external killer".
    if let Some(load) = loadavg_1m() {
        map.insert("loadavg_1m".into(), json!(load));
    }

    // /proc/self/status TracerPid: nonzero means a debugger / strace
    // is attached. Useful when "phantom SIGKILL" turns out to be a
    // manual gdb session.
    if let Some(tracer) = read_proc_field(pid, "TracerPid") {
        if tracer != "0" {
            if let Ok(tracer_pid) = tracer.parse::<u32>() {
                map.insert("tracer_pid".into(), json!(tracer_pid));
                map.insert("tracer".into(), proc_summary(tracer_pid));
            }
        }
    }
    ctx
}

fn parent_pid() -> Option<u32> {
    read_proc_field(std::process::id(), "PPid").and_then(|raw| raw.parse().ok())
}

fn loadavg_1m() -> Option<f64> {
    let content = std::fs::read_to_string("/proc/loadavg").ok()?;
    content.split_whitespace().next()?.parse().ok()
}

/// Fire-and-forget `ps`-style snapshot written to `log_path` (hermes
/// `spawn_async_diagnostic`).
///
/// Runs as a detached subprocess so it can't block teardown or compete
/// with platform shutdown. The subprocess uses its own `timeout` so a
/// wedged `ps` still self-cleans. Returns the subprocess PID on
/// success, `None` on failure. Never panics.
///
/// Deliberately avoids running `ps aux` inside the signal path: on a
/// busy host with hundreds of processes the walk can take seconds,
/// during which teardown is frozen.
pub fn spawn_async_diagnostic(
    log_path: &Path,
    signal_label: &str,
    timeout_seconds: u64,
) -> Option<u32> {
    if cfg!(windows) {
        return None;
    }
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .append(true)
        .open(log_path)
        .ok()?;
    let script = format!(
        "echo '=== shutdown diagnostic @ {signal_label} ==='; \
         echo '--- date ---'; date -u +%Y-%m-%dT%H:%M:%SZ; \
         echo '--- ps aux (top 60 by cpu) ---'; \
         ps aux --sort=-pcpu 2>/dev/null | head -60; \
         echo '--- process tree of self ---'; \
         pstree -plau {pid} 2>/dev/null | head -40 || true; \
         echo '--- /proc/loadavg ---'; \
         cat /proc/loadavg 2>/dev/null || true; \
         echo '--- recent dmesg (oom/killed) ---'; \
         dmesg -T 2>/dev/null | tail -20 || true; \
         echo '=== end ==='",
        pid = std::process::id()
    );
    let child = std::process::Command::new("timeout")
        .arg(timeout_seconds.to_string())
        .arg("bash")
        .arg("-c")
        .arg(script)
        .stdout(file)
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null())
        .spawn()
        .ok()?;
    Some(child.id())
}

/// Default diagnostic log path: `<home>/logs/gateway-shutdown-diagnostic.log`.
pub fn diagnostic_log_path(home: Option<&Path>) -> std::path::PathBuf {
    let base = home
        .map(PathBuf::from)
        .unwrap_or_else(crate::config::ulnclaw_home);
    base.join("logs").join("gateway-shutdown-diagnostic.log")
}

use std::path::PathBuf;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_names_map() {
        assert_eq!(signal_name(libc::SIGTERM), "SIGTERM");
        assert_eq!(signal_name(libc::SIGINT), "SIGINT");
        assert_eq!(signal_name(libc::SIGHUP), "SIGHUP");
        assert_eq!(signal_name(9999), "signal#9999");
    }

    #[test]
    fn proc_summary_self_has_basics() {
        let summary = proc_summary(std::process::id());
        assert_eq!(summary["pid"], std::process::id());
        if cfg!(target_os = "linux") {
            assert!(summary.get("name").is_some(), "{summary}");
            assert!(summary.get("state").is_some(), "{summary}");
            assert!(summary.get("cmdline").is_some(), "{summary}");
        }
    }

    #[test]
    fn snapshot_context_shape() {
        let ctx = snapshot_shutdown_context(Some(libc::SIGTERM));
        assert_eq!(ctx["signal"], "SIGTERM");
        assert_eq!(ctx["signal_num"], libc::SIGTERM);
        assert_eq!(ctx["pid"], std::process::id());
        assert!(ctx.get("ppid").is_some());
        assert!(ctx.get("under_systemd").is_some());
        assert!(ctx.get("parent").is_some());
        assert!(ctx.get("self").is_some());
        if cfg!(target_os = "linux") {
            assert!(ctx.get("loadavg_1m").is_some(), "{ctx}");
        }

        let none = snapshot_shutdown_context(None);
        assert_eq!(none["signal"], "UNKNOWN");
        assert!(none["signal_num"].is_null());
    }

    #[test]
    fn async_diagnostic_writes_header() {
        if cfg!(windows) {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let log = temp.path().join("logs").join("diag.log");
        let pid = spawn_async_diagnostic(&log, "SIGTERM", 5).expect("spawn ok");
        assert!(pid > 0);
        // The header echo lands first; poll briefly for the detached
        // child to flush it.
        let mut content = String::new();
        for _ in 0..100 {
            content = std::fs::read_to_string(&log).unwrap_or_default();
            if content.contains("=== shutdown diagnostic @ SIGTERM ===") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(
            content.contains("=== shutdown diagnostic @ SIGTERM ==="),
            "{content}"
        );
    }

    #[test]
    fn diagnostic_path_follows_home() {
        let home = Path::new("/tmp/some-home");
        assert_eq!(
            diagnostic_log_path(Some(home)),
            PathBuf::from("/tmp/some-home/logs/gateway-shutdown-diagnostic.log")
        );
    }
}

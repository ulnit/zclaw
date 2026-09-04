//! Gateway lifecycle ledger — durable termination-reason evidence
//! (hermes `gateway/lifecycle_ledger.py` parity, NS-608).
//!
//! The gateway has graceful-shutdown paths, but nothing records an
//! **unclean death**: a SIGKILL, a kernel OOM kill, or the whole host
//! dying takes the process out before any handler runs, so the next
//! boot has no idea the previous life ended violently.
//!
//! A tiny state machine persists in `<home>/state/gateway.lifecycle.json`:
//!
//! * On startup, [`record_startup`] reads the sentinel left by the
//!   previous life. `phase == "running"` means that life never reached
//!   any exit path → it died uncleanly. The finding — including the
//!   last heartbeat's memory sample, the closest thing to a pre-death
//!   telemetry snapshot — is appended to `logs/gateway-exit-diag.log`
//!   and logged at WARNING. The sentinel is then rewritten
//!   `phase=running` for the new life.
//! * On every clean exit path, [`mark_exited`] rewrites the sentinel
//!   `phase=exited` with the exit code and a reason string.
//!
//! [`heartbeat`] refreshes `last_heartbeat_at` + a memory sample so an
//! unclean-death report carries "memory N seconds before death" and OOM
//! cycles become classifiable from the evidence alone. Everything here
//! is best-effort: forensics must never affect the lifecycle observed.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// OOM-suspicion thresholds for the last heartbeat's memory sample
/// (hermes `_LOW_MEM_AVAILABLE_*`): deliberately conservative — they
/// only annotate the report with a hint.
const LOW_MEM_AVAILABLE_KIB: u64 = 64 * 1024; // < 64 MiB available
const LOW_MEM_AVAILABLE_FRACTION: f64 = 0.05; // < 5% of MemTotal

/// Heartbeat cadence (hermes shutdown_watchdog loop heartbeat).
pub const HEARTBEAT_INTERVAL_SECONDS: u64 = 30;

pub fn sentinel_path(home: &Path) -> PathBuf {
    home.join("state").join("gateway.lifecycle.json")
}

pub fn exit_diag_path(home: &Path) -> PathBuf {
    home.join("logs").join("gateway-exit-diag.log")
}

fn now_epoch() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Cheap memory snapshot: own RSS + system availability + swap (hermes
/// `sample_memory`). Pure `/proc` reads, Linux-only (`{}` elsewhere),
/// never fails. Values in KiB to match the kernel's units.
pub fn sample_memory() -> Value {
    let mut sample = serde_json::Map::new();
    #[cfg(target_os = "linux")]
    {
        if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
            if let Some(line) = status.lines().find(|l| l.starts_with("VmRSS:")) {
                if let Some(kib) = line
                    .split_whitespace()
                    .nth(1)
                    .and_then(|v| v.parse::<u64>().ok())
                {
                    sample.insert("rss_kib".into(), json!(kib));
                }
            }
        }
        if let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") {
            let mut total = None;
            let mut available = None;
            let mut swap_total = None;
            let mut swap_free = None;
            for line in meminfo.lines() {
                let Some((key, rest)) = line.split_once(':') else {
                    continue;
                };
                let value = rest
                    .split_whitespace()
                    .next()
                    .and_then(|v| v.parse::<u64>().ok());
                match key {
                    "MemTotal" => total = value,
                    "MemAvailable" => available = value,
                    "SwapTotal" => swap_total = value,
                    "SwapFree" => swap_free = value,
                    _ => {}
                }
            }
            if let Some(v) = total {
                sample.insert("mem_total_kib".into(), json!(v));
            }
            if let Some(v) = available {
                sample.insert("mem_available_kib".into(), json!(v));
            }
            if let (Some(t), Some(f)) = (swap_total, swap_free) {
                sample.insert("swap_used_kib".into(), json!(t.saturating_sub(f)));
            }
        }
    }
    Value::Object(sample)
}

fn read_sentinel(home: &Path) -> Option<Value> {
    let raw = std::fs::read_to_string(sentinel_path(home)).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Public sentinel snapshot for ops surfaces (P729 `/api/lifecycle`).
pub fn sentinel_snapshot(home: &Path) -> Option<Value> {
    read_sentinel(home)
}

fn write_sentinel(home: &Path, payload: Value) {
    let path = sentinel_path(home);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, payload.to_string());
}

fn append_exit_diag(home: &Path, record: Value) {
    let path = exit_diag_path(home);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(file, "{}", record.to_string());
    }
}

/// True when `pid` is a live process matching `start_time` (hermes
/// `_pid_alive_with_start_time`): guards the takeover race — during
/// `--replace` the old gateway can still be mid-teardown when the new
/// one boots, and a live matching owner is a planned handover, not an
/// unclean death.
fn pid_alive_with_start_time(pid: Option<i64>, start_time: Option<u64>) -> bool {
    let Some(pid) = pid.filter(|pid| *pid > 0) else {
        return false;
    };
    if !crate::gateway_pidfile::is_alive(pid as u32) {
        return false;
    }
    match (
        start_time,
        crate::gateway_pidfile::process_start_time(pid as u32),
    ) {
        (Some(recorded), Some(actual)) => recorded == actual,
        // Alive but can't disambigate PID reuse — err on "alive".
        _ => true,
    }
}

fn suspected_oom(mem: &Value) -> bool {
    let Some(avail) = mem.get("mem_available_kib").and_then(Value::as_u64) else {
        return false;
    };
    if avail < LOW_MEM_AVAILABLE_KIB {
        return true;
    }
    match mem.get("mem_total_kib").and_then(Value::as_u64) {
        Some(total) if total > 0 => (avail as f64 / total as f64) < LOW_MEM_AVAILABLE_FRACTION,
        _ => false,
    }
}

/// Inspect the previous life's sentinel (read-only): evidence when it
/// died uncleanly, else None (hermes `detect_unclean_exit`).
pub fn detect_unclean_exit(home: &Path) -> Option<Value> {
    let sentinel = read_sentinel(home)?;
    if sentinel.get("phase").and_then(Value::as_str) != Some("running") {
        return None;
    }
    let pid = sentinel.get("pid").and_then(Value::as_i64);
    let start_time = sentinel.get("start_time").and_then(Value::as_u64);
    if pid_alive_with_start_time(pid, start_time) {
        return None; // live owner — planned takeover in flight
    }
    let mut evidence = serde_json::Map::new();
    evidence.insert(
        "prior_pid".into(),
        sentinel.get("pid").cloned().unwrap_or(Value::Null),
    );
    evidence.insert(
        "prior_started_at".into(),
        sentinel.get("started_at").cloned().unwrap_or(Value::Null),
    );
    if let Some(hb_at) = sentinel.get("last_heartbeat_at") {
        evidence.insert("last_heartbeat_at".into(), hb_at.clone());
    }
    if let Some(mem) = sentinel.get("last_heartbeat_mem") {
        evidence.insert("last_heartbeat_mem".into(), mem.clone());
        if suspected_oom(mem) {
            evidence.insert("suspected_oom".into(), json!(true));
        }
    }
    Some(Value::Object(evidence))
}

/// Boot-time entry point: report any unclean previous exit (diag log +
/// WARNING), then claim the sentinel for the current life (hermes
/// `record_startup`). Returns the evidence when found. Never fails.
pub fn record_startup(home: &Path) -> Option<Value> {
    let evidence = detect_unclean_exit(home);
    if let Some(found) = &evidence {
        let mut record = serde_json::Map::new();
        record.insert("ts".into(), json!(now_iso()));
        record.insert("tag".into(), json!("gateway.previous_unclean_exit"));
        record.insert("pid".into(), json!(std::process::id()));
        for (key, value) in found.as_object().unwrap_or(&serde_json::Map::new()) {
            record.insert(key.clone(), value.clone());
        }
        append_exit_diag(home, Value::Object(record));
        tracing::warn!(
            "previous gateway life (pid={:?}, started_at={:?}) exited UNCLEANLY \
             (no exit path ran — SIGKILL / OOM / host death). last_heartbeat_at={:?} \
             suspected_oom={}",
            found.get("prior_pid"),
            found.get("prior_started_at"),
            found.get("last_heartbeat_at"),
            found
                .get("suspected_oom")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        );
    }
    let (pid, start_time) = {
        let pid = std::process::id();
        (pid, crate::gateway_pidfile::process_start_time(pid))
    };
    write_sentinel(
        home,
        json!({
            "phase": "running",
            "pid": pid,
            "start_time": start_time,
            "started_at": now_iso(),
            "started_epoch": now_epoch(),
        }),
    );
    evidence
}

/// Refresh the running sentinel's heartbeat with a fresh memory sample
/// (hermes loop heartbeat). Only touches sentinels this process owns.
pub fn heartbeat(home: &Path) {
    let Some(sentinel) = read_sentinel(home) else {
        return;
    };
    if sentinel.get("phase").and_then(Value::as_str) != Some("running") {
        return;
    }
    if sentinel.get("pid").and_then(Value::as_i64) != Some(std::process::id() as i64) {
        return; // a replacement already claimed the sentinel
    }
    let mut next = sentinel;
    if let Some(map) = next.as_object_mut() {
        map.insert("last_heartbeat_at".into(), json!(now_iso()));
        map.insert("last_heartbeat_epoch".into(), json!(now_epoch()));
        map.insert("last_heartbeat_mem".into(), sample_memory());
    }
    write_sentinel(home, next);
}

/// Background heartbeat loop (one task per gateway process).
pub fn start_heartbeat(home: PathBuf) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(HEARTBEAT_INTERVAL_SECONDS)).await;
            heartbeat(&home);
        }
    });
}

/// Mark the current life as cleanly exited (hermes `mark_exited`).
/// Only rewrites the sentinel when provably owned by this process —
/// during a `--replace` takeover the replacement claims the sentinel
/// before the old process finishes teardown, and the old life must not
/// clobber the new owner's `running` phase on its way out.
pub fn mark_exited(home: &Path, exit_code: Option<i32>, reason: &str) {
    if let Some(sentinel) = read_sentinel(home) {
        if sentinel.get("pid").and_then(Value::as_i64) != Some(std::process::id() as i64) {
            return;
        }
    }
    write_sentinel(
        home,
        json!({
            "phase": "exited",
            "pid": std::process::id(),
            "exit_code": exit_code,
            "exit_reason": reason,
            "exited_at": now_iso(),
        }),
    );
}

/// One-word summary of how the last gateway life ended: `clean` /
/// `unclean` / `unknown` (hermes `read_prior_exit_label`).
pub fn prior_exit_label(home: &Path) -> &'static str {
    match read_sentinel(home) {
        Some(sentinel) => match sentinel.get("phase").and_then(Value::as_str) {
            Some("exited") => "clean",
            Some("running") => "unclean",
            _ => "unknown",
        },
        None => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_home_has_no_sentinel_and_unknown_label() {
        let dir = tempfile::tempdir().unwrap();
        assert!(detect_unclean_exit(dir.path()).is_none());
        assert_eq!(prior_exit_label(dir.path()), "unknown");
    }

    #[test]
    fn startup_claims_running_sentinel() {
        let dir = tempfile::tempdir().unwrap();
        assert!(record_startup(dir.path()).is_none());
        let sentinel = read_sentinel(dir.path()).unwrap();
        assert_eq!(sentinel["phase"], "running");
        assert_eq!(sentinel["pid"].as_u64().unwrap(), std::process::id() as u64);
        assert_eq!(prior_exit_label(dir.path()), "unclean");
    }

    #[test]
    fn mark_exited_flips_to_clean_with_reason() {
        let dir = tempfile::tempdir().unwrap();
        record_startup(dir.path());
        mark_exited(dir.path(), Some(0), "graceful_shutdown");
        let sentinel = read_sentinel(dir.path()).unwrap();
        assert_eq!(sentinel["phase"], "exited");
        assert_eq!(sentinel["exit_code"], 0);
        assert_eq!(sentinel["exit_reason"], "graceful_shutdown");
        assert_eq!(prior_exit_label(dir.path()), "clean");
        // A clean sentinel is not an unclean death on the next boot.
        assert!(detect_unclean_exit(dir.path()).is_none());
    }

    #[test]
    fn dead_owner_running_sentinel_reports_unclean_with_evidence() {
        let dir = tempfile::tempdir().unwrap();
        // Fabricate a previous life owned by a dead pid.
        write_sentinel(
            dir.path(),
            json!({
                "phase": "running",
                "pid": 999_999_999i64,
                "start_time": 12345u64,
                "started_at": "2026-08-09T00:00:00Z",
                "last_heartbeat_at": "2026-08-09T00:05:00Z",
                "last_heartbeat_mem": {"mem_total_kib": 1000000u64, "mem_available_kib": 1000u64},
            }),
        );
        let evidence = record_startup(dir.path()).expect("unclean finding");
        assert_eq!(evidence["prior_pid"], 999_999_999i64);
        assert_eq!(evidence["suspected_oom"], true);
        // The finding lands in the exit-diag log.
        let log = std::fs::read_to_string(exit_diag_path(dir.path())).unwrap();
        assert!(log.contains("gateway.previous_unclean_exit"), "{log}");
        // The new life now owns a running sentinel.
        let sentinel = read_sentinel(dir.path()).unwrap();
        assert_eq!(sentinel["phase"], "running");
        assert_eq!(sentinel["pid"].as_u64().unwrap(), std::process::id() as u64);
    }

    #[test]
    fn live_owner_sentinel_is_a_takeover_not_a_death() {
        let dir = tempfile::tempdir().unwrap();
        // This very process is alive — a running sentinel it owns must
        // read as a planned handover, not an unclean death.
        write_sentinel(
            dir.path(),
            json!({
                "phase": "running",
                "pid": std::process::id(),
                "start_time": crate::gateway_pidfile::process_start_time(std::process::id()),
                "started_at": "2026-08-09T00:00:00Z",
            }),
        );
        assert!(detect_unclean_exit(dir.path()).is_none());
    }

    #[test]
    fn heartbeat_enriches_owned_sentinel_only() {
        let dir = tempfile::tempdir().unwrap();
        record_startup(dir.path());
        heartbeat(dir.path());
        let sentinel = read_sentinel(dir.path()).unwrap();
        assert!(sentinel.get("last_heartbeat_at").is_some(), "{sentinel}");
        assert!(sentinel.get("last_heartbeat_mem").is_some(), "{sentinel}");

        // A sentinel owned by another pid is left untouched.
        write_sentinel(
            dir.path(),
            json!({"phase": "running", "pid": 424242i64, "started_at": "x"}),
        );
        heartbeat(dir.path());
        let sentinel = read_sentinel(dir.path()).unwrap();
        assert!(sentinel.get("last_heartbeat_at").is_none(), "{sentinel}");
    }

    #[test]
    fn mark_exited_refuses_foreign_sentinels() {
        let dir = tempfile::tempdir().unwrap();
        write_sentinel(dir.path(), json!({"phase": "running", "pid": 424242i64}));
        mark_exited(dir.path(), Some(0), "graceful_shutdown");
        let sentinel = read_sentinel(dir.path()).unwrap();
        assert_eq!(sentinel["phase"], "running", "foreign sentinel untouched");
    }

    #[test]
    fn memory_sample_shape() {
        let sample = sample_memory();
        #[cfg(target_os = "linux")]
        {
            assert!(sample.get("rss_kib").is_some(), "{sample}");
            assert!(sample.get("mem_total_kib").is_some(), "{sample}");
        }
        let _ = sample;
    }

    #[test]
    fn oom_suspicion_thresholds() {
        assert!(suspected_oom(
            &json!({"mem_available_kib": 1000u64, "mem_total_kib": 1000000u64})
        ));
        // 4% of total — under the 5% fraction line.
        assert!(suspected_oom(
            &json!({"mem_available_kib": 40000u64, "mem_total_kib": 1000000u64})
        ));
        assert!(!suspected_oom(
            &json!({"mem_available_kib": 900000u64, "mem_total_kib": 1000000u64})
        ));
        assert!(!suspected_oom(&json!({})));
    }
}

//! Out-of-loop shutdown and runtime liveness backstops — port of
//! hermes `gateway/shutdown_watchdog.py` (#66892, #69089).
//!
//! When the async runtime freezes mid-drain, every runtime-based
//! recovery path is structurally unable to fire: the drain deadline,
//! status rewrites, and forensics all need the same runtime that is
//! stuck. Service supervisors only restart a *dead* process, so a
//! wedged-but-alive gateway sits as a zombie until manual SIGKILL.
//!
//! This module provides:
//!
//! 1. A plain OS-thread shutdown watchdog armed when a restart drain
//!    starts. If shutdown has not completed within
//!    `restart_drain_timeout + grace`, it dumps a diagnostic snapshot,
//!    records the watchdog exit in the lifecycle ledger, then exits
//!    the process so the service manager can revive it.
//! 2. A runtime heartbeat file at `<home>/state/gateway.heartbeat` so
//!    external supervision can distinguish "process alive" from
//!    "runtime frozen" (`gateway_state.json` alone can't — it only
//!    rewrites on transitions/turns).
//! 3. A lifetime thread watchdog that can still diagnose and hard-exit
//!    when the runtime is too frozen to run its own heartbeat or
//!    timeout callbacks.
//!
//! The asyncio-specific selector floor timer from hermes has no tokio
//! analogue (tokio's timer wheel has no unbounded-selector hazard) and
//! is intentionally omitted.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use serde_json::{json, Value};

/// Extra leash beyond the restart drain timeout so a
/// slow-but-progressing drain is not cut short (hermes
/// `DEFAULT_SHUTDOWN_WATCHDOG_GRACE_S`).
pub const DEFAULT_SHUTDOWN_WATCHDOG_GRACE_S: f64 = 60.0;
pub const DEFAULT_HEARTBEAT_INTERVAL_S: f64 = 30.0;
pub const DEFAULT_LOOP_WATCHDOG_INTERVAL_S: f64 = 30.0;
pub const DEFAULT_LOOP_WATCHDOG_TIMEOUT_S: f64 = 10.0;
pub const DEFAULT_LOOP_WATCHDOG_MAX_STRIKES: u32 = 3;

fn heartbeat_relative() -> &'static [&'static str] {
    &["state", "gateway.heartbeat"]
}

fn watchdog_dump_relative() -> &'static [&'static str] {
    &["logs", "gateway-shutdown-watchdog.log"]
}

fn process_home() -> PathBuf {
    crate::config::ulnclaw_home()
}

/// Process start as epoch seconds — memoised on first use (hermes
/// passes `time.time()` at gateway start; supervisors use it to
/// detect PID reuse).
fn process_start_epoch() -> f64 {
    static START: OnceLock<f64> = OnceLock::new();
    *START.get_or_init(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0)
    })
}

/// `<home>/state/gateway.heartbeat` (hermes `get_loop_heartbeat_path`).
pub fn heartbeat_path(home: Option<&Path>) -> PathBuf {
    let base = home.map(PathBuf::from).unwrap_or_else(process_home);
    base.join("state").join("gateway.heartbeat")
}

/// Diagnostic dump path for a fired watchdog (hermes
/// `get_shutdown_watchdog_dump_path`).
pub fn shutdown_watchdog_dump_path(home: Option<&Path>) -> PathBuf {
    let base = home.map(PathBuf::from).unwrap_or_else(process_home);
    base.join("logs").join("gateway-shutdown-watchdog.log")
}

/// Atomically rewrite the loop-liveness heartbeat file (hermes
/// `write_loop_heartbeat`).
///
/// `start_time` is the gateway process start (epoch seconds) so
/// supervisors can detect PID reuse. Best-effort — never panics.
/// Embeds a cheap memory sample (own RSS + MemAvailable + swap via
/// the lifecycle ledger) so the heartbeat doubles as a rolling
/// pre-death telemetry snapshot: after an unclean death the last
/// heartbeat is the closest surviving record of memory pressure.
pub fn write_loop_heartbeat(home: Option<&Path>, extra: Option<Value>) -> PathBuf {
    let path = heartbeat_path(home);
    let mut payload = json!({
        "pid": std::process::id(),
        "updated_at": chrono::Utc::now().to_rfc3339(),
        "monotonic": monotonic_secs(),
        "start_time": process_start_epoch(),
    });
    let mem = crate::lifecycle_ledger::sample_memory();
    if mem.is_object() && !mem.as_object().map(|m| m.is_empty()).unwrap_or(true) {
        payload["mem"] = mem;
    }
    if let Some(extra) = extra {
        if let (Some(base), Some(additions)) = (payload.as_object_mut(), extra.as_object()) {
            for (key, value) in additions {
                base.insert(key.clone(), value.clone());
            }
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, serde_json::to_string(&payload).unwrap_or_default()).is_ok() {
        std::fs::rename(&tmp, &path).ok();
    }
    path
}

fn monotonic_secs() -> f64 {
    static ORIGIN: OnceLock<std::time::Instant> = OnceLock::new();
    let origin = ORIGIN.get_or_init(std::time::Instant::now);
    origin.elapsed().as_secs_f64()
}

/// Wall-clock leash for the shutdown watchdog thread (hermes
/// `resolve_shutdown_watchdog_delay`).
pub fn resolve_shutdown_watchdog_delay(drain_timeout_s: f64, grace_s: f64) -> f64 {
    let drain = if drain_timeout_s.is_finite() && drain_timeout_s > 0.0 {
        drain_timeout_s
    } else {
        0.0
    };
    let grace = if grace_s.is_finite() && grace_s > 0.0 {
        grace_s
    } else {
        DEFAULT_SHUTDOWN_WATCHDOG_GRACE_S
    };
    drain + grace
}

fn write_watchdog_dump(dump_path: &Path, delay_s: f64, snapshot: Option<Value>) {
    if let Some(parent) = dump_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let header = json!({
        "event": "shutdown_watchdog_fired",
        "pid": std::process::id(),
        "delay_s": delay_s,
        "fired_at": chrono::Utc::now().to_rfc3339(),
        "snapshot": snapshot.unwrap_or_else(|| json!({})),
    });
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dump_path)
    {
        use std::io::Write;
        let _ = writeln!(file, "{}", header);
        // Stable Rust has no faulthandler-style all-thread dump; record
        // the firing thread's captured backtrace instead.
        let _ = writeln!(file, "--- backtrace (firing thread) ---");
        let _ = writeln!(file, "{}", std::backtrace::Backtrace::force_capture());
        let _ = writeln!(file, "--- end dump ---");
    }
    eprintln!(
        "Gateway shutdown watchdog fired after {}s (pid={}); diagnostic dump at {}",
        delay_s as u64,
        std::process::id(),
        dump_path.display()
    );
}

/// Options for [`arm_shutdown_watchdog`].
pub struct ShutdownWatchdogOptions {
    pub delay: std::time::Duration,
    /// Metadata snapshot merged into the dump on fire.
    pub snapshot_fn: Option<Arc<dyn Fn() -> Value + Send + Sync>>,
    pub exit_code: i32,
    pub dump_path: Option<PathBuf>,
    /// Home for the default dump path + lifecycle ledger (defaults to
    /// the process home).
    pub home: Option<PathBuf>,
    /// Injectable exit action (tests record instead of exiting).
    pub exit_fn: Arc<dyn Fn(i32) + Send + Sync>,
}

/// Arm a daemon-thread hard-exit backstop for a wedged shutdown path
/// (hermes `arm_shutdown_watchdog`).
///
/// If `done` flips true before `delay` elapses, the thread exits
/// quietly (normal/progressing shutdown completed). Otherwise it
/// dumps diagnostics, records the watchdog exit in the lifecycle
/// ledger (so the next boot reports "shutdown watchdog fired" instead
/// of misclassifying the death), releases the pidfile, and exits the
/// process.
///
/// Returns the `done` flag so the caller can disarm on successful
/// completion.
pub fn arm_shutdown_watchdog(
    done: Arc<AtomicBool>,
    options: ShutdownWatchdogOptions,
) -> Arc<AtomicBool> {
    let delay = options.delay;
    if delay.is_zero() {
        return done;
    }
    let done_flag = done.clone();
    std::thread::Builder::new()
        .name("gateway-shutdown-watchdog".into())
        .spawn(move || {
            // Wait with interruptible chunks so a late disarm doesn't
            // need the full remaining sleep to observe `done`.
            let deadline = std::time::Instant::now() + delay;
            while std::time::Instant::now() < deadline {
                if done_flag.load(Ordering::SeqCst) {
                    return;
                }
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                std::thread::sleep(remaining.min(std::time::Duration::from_secs(1)));
            }
            if done_flag.load(Ordering::SeqCst) {
                return;
            }
            let snapshot = options.snapshot_fn.as_ref().map(|f| f());
            let home = options.home.clone().unwrap_or_else(process_home);
            let target = options
                .dump_path
                .clone()
                .unwrap_or_else(|| shutdown_watchdog_dump_path(Some(&home)));
            write_watchdog_dump(&target, delay.as_secs_f64(), snapshot);
            tracing::error!(
                "shutdown watchdog fired after {}s — forcing process exit (drain path appears wedged; see {})",
                delay.as_secs_f64() as u64,
                target.display()
            );
            // Mirror the graceful-shutdown cleanup: release the pidfile
            // BEFORE the exit so the single-instance guard never points
            // at a dead process, then record the watchdog exit so the
            // next boot's unclean-death detector reports it correctly.
            let _ = std::fs::remove_file(crate::gateway_pidfile::pidfile_path(&home));
            crate::lifecycle_ledger::mark_exited(&home, Some(options.exit_code), "shutdown_watchdog");
            (options.exit_fn)(options.exit_code);
        })
        .ok();
    done
}

/// Handle for the runtime liveness watchdog thread (hermes
/// `_LoopLivenessWatchdogHandle`).
pub struct LoopWatchdogHandle {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl LoopWatchdogHandle {
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            thread.join().ok();
        }
    }
}

/// Start an out-of-loop watchdog that hard-exits after missed probes
/// (hermes `start_loop_liveness_watchdog`).
///
/// Every `probe_interval` the watchdog schedules a no-op task on the
/// runtime; if the task does not run within `probe_timeout` for
/// `max_strikes` consecutive probes, the runtime is frozen — dump
/// diagnostics and exit with the service-restart code so the
/// supervisor revives the gateway.
pub fn start_loop_liveness_watchdog(
    runtime: tokio::runtime::Handle,
    probe_interval: std::time::Duration,
    probe_timeout: std::time::Duration,
    max_strikes: u32,
    exit_code: i32,
) -> Option<LoopWatchdogHandle> {
    start_loop_liveness_watchdog_with(
        runtime,
        probe_interval,
        probe_timeout,
        max_strikes,
        exit_code,
        None,
    )
}

/// Test-friendly variant with an injectable exit action.
pub fn start_loop_liveness_watchdog_with(
    runtime: tokio::runtime::Handle,
    probe_interval: std::time::Duration,
    probe_timeout: std::time::Duration,
    max_strikes: u32,
    exit_code: i32,
    exit_fn: Option<Arc<dyn Fn(i32) + Send + Sync>>,
) -> Option<LoopWatchdogHandle> {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = stop.clone();
    let exit_fn = exit_fn.unwrap_or_else(|| Arc::new(|code| std::process::exit(code)));
    let thread = std::thread::Builder::new()
        .name("gateway-loop-liveness-watchdog".into())
        .spawn(move || {
            let mut strikes = 0u32;
            loop {
                // Interval wait, interruptible by stop.
                let interval_deadline = std::time::Instant::now() + probe_interval;
                while std::time::Instant::now() < interval_deadline {
                    if stop_flag.load(Ordering::SeqCst) {
                        return;
                    }
                    let remaining =
                        interval_deadline.saturating_duration_since(std::time::Instant::now());
                    std::thread::sleep(remaining.min(std::time::Duration::from_millis(50)));
                }
                if stop_flag.load(Ordering::SeqCst) {
                    return;
                }
                let responded = Arc::new(AtomicBool::new(false));
                let probe_flag = responded.clone();
                runtime.spawn(async move {
                    probe_flag.store(true, Ordering::SeqCst);
                });
                let probe_deadline = std::time::Instant::now() + probe_timeout;
                while std::time::Instant::now() < probe_deadline {
                    if stop_flag.load(Ordering::SeqCst) {
                        return;
                    }
                    if responded.load(Ordering::SeqCst) {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                if stop_flag.load(Ordering::SeqCst) {
                    return;
                }
                if responded.load(Ordering::SeqCst) {
                    strikes = 0;
                    continue;
                }
                strikes += 1;
                if strikes < max_strikes {
                    continue;
                }
                let home = process_home();
                let target = shutdown_watchdog_dump_path(Some(&home));
                write_watchdog_dump(
                    &target,
                    probe_timeout.as_secs_f64(),
                    Some(json!({
                        "event": "loop_liveness_watchdog",
                        "missed_probes": strikes,
                    })),
                );
                tracing::error!(
                    "gateway runtime missed {strikes} consecutive liveness probes — forcing exit with code {exit_code}"
                );
                crate::lifecycle_ledger::mark_exited(
                    &home,
                    Some(exit_code),
                    "loop_liveness_watchdog",
                );
                exit_fn(exit_code);
                return;
            }
        })
        .ok()?;
    Some(LoopWatchdogHandle {
        stop,
        thread: Some(thread),
    })
}

/// Rewrite the loop heartbeat file on a cadence until gated off
/// (hermes `loop_heartbeat_forever`).
///
/// Runs as a task on the gateway runtime — if the runtime freezes,
/// this task stops and the file mtime/updated_at goes stale for
/// external monitors.
pub async fn loop_heartbeat_forever(
    interval: std::time::Duration,
    should_continue: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
) {
    let interval = if interval < std::time::Duration::from_secs(1) {
        std::time::Duration::from_secs_f64(DEFAULT_HEARTBEAT_INTERVAL_S)
    } else {
        interval
    };
    // Immediate first write so monitors see a fresh file as soon as
    // the gateway is running, not after the first interval.
    write_loop_heartbeat(None, None);
    loop {
        if let Some(gate) = should_continue.as_ref() {
            if !gate() {
                return;
            }
        }
        tokio::time::sleep(interval).await;
        if let Some(gate) = should_continue.as_ref() {
            if !gate() {
                return;
            }
        }
        write_loop_heartbeat(None, None);
    }
}

/// Monotonic probe counter for tests/diagnostics.
static PROBE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Increment and return the process-wide probe counter.
pub fn bump_probe_counter() -> u64 {
    PROBE_COUNTER.fetch_add(1, Ordering::Relaxed) + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delay_resolution_clamps() {
        assert_eq!(resolve_shutdown_watchdog_delay(60.0, 30.0), 90.0);
        assert_eq!(resolve_shutdown_watchdog_delay(-5.0, 30.0), 30.0);
        assert_eq!(
            resolve_shutdown_watchdog_delay(60.0, f64::NAN),
            60.0 + DEFAULT_SHUTDOWN_WATCHDOG_GRACE_S
        );
    }

    #[test]
    fn heartbeat_write_roundtrip() {
        let temp = tempfile::tempdir().unwrap();
        let path =
            write_loop_heartbeat(Some(temp.path()), Some(json!({"extra_key": "extra_value"})));
        assert_eq!(path, temp.path().join("state").join("gateway.heartbeat"));
        let body: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(body["pid"], std::process::id());
        assert!(body["updated_at"].as_str().unwrap().contains('T'));
        assert!(body["monotonic"].as_f64().unwrap() >= 0.0);
        assert!(body["start_time"].as_f64().unwrap() > 0.0);
        assert_eq!(body["extra_key"], "extra_value");
        // Rewrite overwrites atomically.
        write_loop_heartbeat(Some(temp.path()), None);
        let body2: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(body2.get("extra_key").is_none());
    }

    #[test]
    fn disarm_before_deadline_exits_quietly() {
        let temp = tempfile::tempdir().unwrap();
        let done = Arc::new(AtomicBool::new(false));
        let exited = Arc::new(AtomicBool::new(false));
        let exited_flag = exited.clone();
        arm_shutdown_watchdog(
            done.clone(),
            ShutdownWatchdogOptions {
                delay: std::time::Duration::from_millis(300),
                snapshot_fn: None,
                exit_code: 75,
                dump_path: None,
                home: Some(temp.path().to_path_buf()),
                exit_fn: Arc::new(move |_| exited_flag.store(true, Ordering::SeqCst)),
            },
        );
        done.store(true, Ordering::SeqCst); // disarm immediately
        std::thread::sleep(std::time::Duration::from_millis(500));
        assert!(!exited.load(Ordering::SeqCst));
        assert!(!shutdown_watchdog_dump_path(Some(temp.path())).exists());
    }

    #[test]
    fn fired_watchdog_dumps_and_exits_via_injected_fn() {
        let temp = tempfile::tempdir().unwrap();
        let done = Arc::new(AtomicBool::new(false)); // never disarmed
        let exit_code = Arc::new(std::sync::Mutex::new(None::<i32>));
        let code_slot = exit_code.clone();
        arm_shutdown_watchdog(
            done,
            ShutdownWatchdogOptions {
                delay: std::time::Duration::from_millis(120),
                snapshot_fn: Some(Arc::new(|| json!({"active_runs": 3}))),
                exit_code: 75,
                dump_path: None,
                home: Some(temp.path().to_path_buf()),
                exit_fn: Arc::new(move |code| {
                    *code_slot.lock().unwrap() = Some(code);
                }),
            },
        );
        // Wait for the watchdog to fire.
        let mut observed = None;
        for _ in 0..100 {
            observed = *exit_code.lock().unwrap();
            if observed.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(observed, Some(75));
        let dump = shutdown_watchdog_dump_path(Some(temp.path()));
        let content = std::fs::read_to_string(&dump).expect("dump written");
        assert!(content.contains("shutdown_watchdog_fired"), "{content}");
        assert!(content.contains("\"active_runs\":3"), "{content}");
        // Lifecycle ledger records the watchdog exit.
        let sentinel: Value = serde_json::from_str(
            &std::fs::read_to_string(crate::lifecycle_ledger::sentinel_path(temp.path()))
                .expect("sentinel written"),
        )
        .unwrap();
        assert_eq!(sentinel["exit_reason"], "shutdown_watchdog");
        assert_eq!(sentinel["exit_code"], 75);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn loop_watchdog_responsive_runtime_no_exit() {
        let runtime = tokio::runtime::Handle::current();
        let exited = Arc::new(AtomicBool::new(false));
        let exited_flag = exited.clone();
        let mut handle = start_loop_liveness_watchdog_with(
            runtime,
            std::time::Duration::from_millis(40),
            std::time::Duration::from_millis(200),
            2,
            75,
            Some(Arc::new(move |_| exited_flag.store(true, Ordering::SeqCst))),
        )
        .expect("watchdog starts");
        // Let several probes run against a responsive loop.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        handle.stop();
        assert!(!exited.load(Ordering::SeqCst));
    }

    #[test]
    fn paths_follow_home() {
        let home = Path::new("/tmp/some-home");
        assert_eq!(
            heartbeat_path(Some(home)),
            PathBuf::from("/tmp/some-home/state/gateway.heartbeat")
        );
        assert_eq!(
            shutdown_watchdog_dump_path(Some(home)),
            PathBuf::from("/tmp/some-home/logs/gateway-shutdown-watchdog.log")
        );
    }
}

//! Minimal, optional systemd `sd_notify` support for the gateway
//! (hermes `gateway/systemd_notify.py` parity).
//!
//! When the gateway runs as a `Type=notify` systemd unit, startup
//! completion is reported with `READY=1` (+ a human `STATUS=` line),
//! liveness is proven by periodic `WATCHDOG=1` feeds at half the
//! configured `WATCHDOG_USEC` cadence, and shutdown announces
//! `STOPPING=1` before the drain. Everything is best-effort: a
//! missing socket or an older platform must never prevent the gateway
//! from starting.
//!
//! Gating mirrors hermes: the config toggle (`[gateway]
//! systemd_watchdog_seconds`, default 0 = off) decides whether the
//! watchdog is armed at all, and the actual feed cadence comes from
//! systemd's `WATCHDOG_USEC` env stamp — the runtime never invents a
//! pace systemd didn't ask for.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// Send one non-blocking sd_notify datagram when systemd configured
/// it (hermes `notify`). Failures are deliberately non-fatal.
pub fn notify(message: &str) -> bool {
    let address = std::env::var("NOTIFY_SOCKET").unwrap_or_default();
    let address = address.trim();
    if address.is_empty() || message.is_empty() {
        return false;
    }
    notify_to(address, message)
}

#[cfg(unix)]
fn notify_to(address: &str, message: &str) -> bool {
    use std::os::unix::net::UnixDatagram;
    // Abstract-namespace sockets start with '@' — map to a leading
    // NUL byte (hermes `_notify_address`).
    let path = if let Some(abstract_name) = address.strip_prefix('@') {
        format!("\0{abstract_name}")
    } else {
        address.to_string()
    };
    let socket = match UnixDatagram::unbound() {
        Ok(socket) => socket,
        Err(_) => return false,
    };
    if socket.set_nonblocking(true).is_err() {
        return false;
    }
    if socket.connect(path).is_err() {
        return false;
    }
    match socket.send(message.as_bytes()) {
        Ok(_) => true,
        // WouldBlock (full receiver buffer) and any other IO failure
        // must not stall the gateway.
        Err(_) => false,
    }
}

/// No systemd notify socket on non-unix platforms.
#[cfg(not(unix))]
fn notify_to(_address: &str, _message: &str) -> bool {
    false
}

/// systemd's configured watchdog interval (`WATCHDOG_USEC`), present
/// only when a notify socket is configured too (hermes
/// `watchdog_interval_seconds`).
pub fn watchdog_interval() -> Option<Duration> {
    if std::env::var("NOTIFY_SOCKET")
        .unwrap_or_default()
        .trim()
        .is_empty()
    {
        return None;
    }
    let raw = std::env::var("WATCHDOG_USEC").unwrap_or_default();
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let usec: f64 = raw.parse().ok()?;
    if !usec.is_finite() || usec <= 0.0 {
        return None;
    }
    Some(Duration::from_secs_f64(usec / 1_000_000.0))
}

/// Feeds systemd while the tokio runtime continues to make progress
/// (hermes `SystemdWatchdog`). A tick that wakes later than the lag
/// budget marks the watchdog unhealthy: feeding stops and systemd
/// restarts the unit instead of the gateway wedging silently.
pub struct SystemdWatchdog {
    config_seconds: u64,
    stopping: Arc<AtomicBool>,
    unhealthy: Arc<AtomicBool>,
    stopping_notified: AtomicBool,
    handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl SystemdWatchdog {
    pub fn new(config_seconds: u64) -> Self {
        Self {
            config_seconds,
            stopping: Arc::new(AtomicBool::new(false)),
            unhealthy: Arc::new(AtomicBool::new(false)),
            stopping_notified: AtomicBool::new(false),
            handle: Mutex::new(None),
        }
    }

    /// Config toggle on AND systemd asked for a watchdog.
    pub fn enabled(&self) -> bool {
        self.config_seconds > 0 && watchdog_interval().is_some()
    }

    pub fn unhealthy(&self) -> bool {
        self.unhealthy.load(Ordering::SeqCst)
    }

    /// Start the loop-progress sampler (hermes `start`). Returns false
    /// when the watchdog is not enabled.
    pub fn start(&self) -> bool {
        if !self.enabled() {
            return false;
        }
        {
            let handle = self.handle.lock().unwrap();
            if let Some(task) = handle.as_ref() {
                if !task.is_finished() {
                    return true;
                }
            }
        }
        self.stopping.store(false, Ordering::SeqCst);
        self.unhealthy.store(false, Ordering::SeqCst);
        self.stopping_notified.store(false, Ordering::SeqCst);
        let stopping = self.stopping.clone();
        let unhealthy = self.unhealthy.clone();
        let task = tokio::spawn(async move {
            run_feeder(&stopping, &unhealthy).await;
        });
        *self.handle.lock().unwrap() = Some(task);
        true
    }

    /// Tell systemd startup completed (hermes `ready`).
    pub fn ready(&self, status: &str) -> bool {
        if !self.enabled() {
            return false;
        }
        let safe_status = if status.trim().is_empty() {
            "Gateway running".to_string()
        } else {
            status.replace('\n', " ")
        };
        notify(&format!("READY=1\nSTATUS={safe_status}"))
    }

    /// Stop feeding and emit `STOPPING=1` at most once (hermes
    /// `stop`).
    pub async fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        let task = self.handle.lock().unwrap().take();
        if let Some(task) = task {
            if !task.is_finished() {
                task.abort();
            }
            let _ = task.await;
        }
        if self.enabled() && !self.stopping_notified.swap(true, Ordering::SeqCst) {
            notify("STOPPING=1");
        }
    }
}

// ---------------------------------------------------------------------------
// Process-wide watchdog (the gateway has exactly one)
// ---------------------------------------------------------------------------

static WATCHDOG: OnceLock<SystemdWatchdog> = OnceLock::new();

/// Arm the process-wide watchdog (hermes `_start_systemd_watchdog`):
/// gated by `[gateway] systemd_watchdog_seconds` (> 0) AND systemd's
/// `WATCHDOG_USEC` stamp. Returns true when feeding started.
pub fn arm_watchdog(config_seconds: u64) -> bool {
    let watchdog = WATCHDOG.get_or_init(|| SystemdWatchdog::new(config_seconds));
    watchdog.start()
}

/// Report READY=1 on behalf of the process-wide watchdog (no-op when
/// not armed).
pub fn notify_ready(status: &str) -> bool {
    WATCHDOG.get().map(|w| w.ready(status)).unwrap_or(false)
}

/// Stop the process-wide watchdog, emitting STOPPING=1 at most once.
pub async fn stop_watchdog() {
    if let Some(watchdog) = WATCHDOG.get() {
        watchdog.stop().await;
    }
}

/// Whether the watchdog sampler declared the event loop unhealthy
/// (diagnostics surface).
pub fn watchdog_unhealthy() -> bool {
    WATCHDOG.get().map(|w| w.unhealthy()).unwrap_or(false)
}

async fn run_feeder(stopping: &AtomicBool, unhealthy: &AtomicBool) {
    let Some(interval) = watchdog_interval() else {
        return;
    };
    let cadence = interval.div_f64(2.0).max(Duration::from_millis(10));
    // Lag tolerance (hermes `_lag_tolerance`): a quarter of the
    // watchdog interval, floored at 100ms.
    let tolerance = interval.mul_f64(0.25).max(Duration::from_millis(100));
    let mut scheduled_at = tokio::time::Instant::now() + cadence;
    while !stopping.load(Ordering::SeqCst) && !unhealthy.load(Ordering::SeqCst) {
        let now = tokio::time::Instant::now();
        if scheduled_at > now {
            tokio::time::sleep(scheduled_at - now).await;
        }
        let now = tokio::time::Instant::now();
        if stopping.load(Ordering::SeqCst) {
            return;
        }
        let lag = now.saturating_duration_since(scheduled_at);
        if lag > tolerance {
            unhealthy.store(true, Ordering::SeqCst);
            notify("STATUS=watchdog unhealthy: event loop progress is late");
            return;
        }
        notify("WATCHDOG=1");
        scheduled_at += cadence;
        if scheduled_at < now {
            scheduled_at = now + cadence;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Env-var mutations are process-global; serialize these tests.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn notify_is_a_noop_without_socket() {
        let _guard = ENV_LOCK.lock().unwrap();
        let prev = std::env::var("NOTIFY_SOCKET").ok();
        std::env::remove_var("NOTIFY_SOCKET");
        assert!(!notify("READY=1"));
        assert!(!notify(""));
        match prev {
            Some(v) => std::env::set_var("NOTIFY_SOCKET", v),
            None => std::env::remove_var("NOTIFY_SOCKET"),
        }
    }

    #[test]
    fn notify_delivers_datagram_to_unix_socket() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("notify.sock");
        let receiver = std::os::unix::net::UnixDatagram::bind(&sock_path).unwrap();
        receiver.set_nonblocking(true).unwrap();
        assert!(notify_to(
            sock_path.to_str().unwrap(),
            "READY=1\nSTATUS=testing"
        ));
        let mut buf = [0u8; 128];
        let n = receiver.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"READY=1\nSTATUS=testing");
    }

    #[test]
    fn watchdog_interval_requires_both_env_stamps() {
        let _guard = ENV_LOCK.lock().unwrap();
        let prev_socket = std::env::var("NOTIFY_SOCKET").ok();
        let prev_usec = std::env::var("WATCHDOG_USEC").ok();

        std::env::remove_var("NOTIFY_SOCKET");
        std::env::set_var("WATCHDOG_USEC", "30000000");
        assert!(watchdog_interval().is_none(), "socket missing");

        std::env::set_var("NOTIFY_SOCKET", "/run/systemd/notify");
        std::env::remove_var("WATCHDOG_USEC");
        assert!(watchdog_interval().is_none(), "usec missing");

        std::env::set_var("WATCHDOG_USEC", "30000000");
        assert_eq!(watchdog_interval(), Some(Duration::from_secs(30)));

        std::env::set_var("WATCHDOG_USEC", "not-a-number");
        assert!(watchdog_interval().is_none());

        for (key, prev) in [("NOTIFY_SOCKET", prev_socket), ("WATCHDOG_USEC", prev_usec)] {
            match prev {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }

    #[test]
    fn watchdog_requires_config_and_systemd() {
        let _guard = ENV_LOCK.lock().unwrap();
        let prev_socket = std::env::var("NOTIFY_SOCKET").ok();
        std::env::remove_var("NOTIFY_SOCKET");
        // Config off -> disabled regardless of env.
        assert!(!SystemdWatchdog::new(0).enabled());
        // Config on but no systemd env -> still disabled.
        assert!(!SystemdWatchdog::new(30).enabled());
        assert!(!SystemdWatchdog::new(30).start());
        assert!(!SystemdWatchdog::new(30).ready("x"));
        match prev_socket {
            Some(v) => std::env::set_var("NOTIFY_SOCKET", v),
            None => std::env::remove_var("NOTIFY_SOCKET"),
        }
    }

    #[test]
    fn global_ready_is_noop_when_not_armed() {
        // arm_watchdog(0) never arms (config off), so notify_ready
        // stays a no-op even if a NOTIFY_SOCKET happens to exist.
        assert!(!arm_watchdog(0));
        assert!(!notify_ready("should not send"));
        assert!(!watchdog_unhealthy());
    }
}

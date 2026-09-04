//! Periodic process memory logging for the gateway (hermes
//! `gateway/memory_monitor.py` parity, itself a cline port).
//!
//! The gateway is long-lived and caches agent state, transcripts, tool
//! schemas and MCP connections; a slow leak is only visible as RSS
//! climbing over hours. A single grep-friendly `[MEMORY] ...` line every
//! interval gives maintainers a time series. The baseline is logged
//! immediately, a final snapshot goes out on shutdown, and the monitor
//! never crashes the gateway — when RSS cannot be read it warns once and
//! stays off.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Default logging cadence (hermes default of 5 minutes).
pub const DEFAULT_INTERVAL_SECONDS: u64 = 300;

static RUNNING: AtomicBool = AtomicBool::new(false);
static STOP: AtomicBool = AtomicBool::new(false);
static STARTED_AT: OnceLock<Instant> = OnceLock::new();

/// Current process RSS in MB: `VmRSS` from `/proc/self/status` on
/// Linux (true current usage), the `getrusage` high-water mark on
/// other unices (hermes uses the high-water mark as its leak proxy).
pub fn rss_mb() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
            if let Some(line) = status.lines().find(|l| l.starts_with("VmRSS:")) {
                if let Some(kb) = line
                    .split_whitespace()
                    .nth(1)
                    .and_then(|v| v.parse::<u64>().ok())
                {
                    return Some(kb / 1024);
                }
            }
        }
    }
    #[cfg(unix)]
    {
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } == 0 {
            let maxrss = usage.ru_maxrss as u64;
            #[cfg(target_os = "macos")]
            return Some(maxrss / (1024 * 1024)); // bytes
            #[cfg(not(target_os = "macos"))]
            return Some(maxrss / 1024); // KB
        }
    }
    #[allow(unreachable_code)]
    None
}

/// Live OS thread count (a handy correlate when diagnosing leaks).
pub fn thread_count() -> usize {
    #[cfg(target_os = "linux")]
    {
        if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
            if let Some(line) = status.lines().find(|l| l.starts_with("Threads:")) {
                if let Some(count) = line
                    .split_whitespace()
                    .nth(1)
                    .and_then(|v| v.parse::<usize>().ok())
                {
                    return count;
                }
            }
        }
    }
    0
}

fn uptime_secs() -> u64 {
    STARTED_AT
        .get()
        .map(|start| start.elapsed().as_secs())
        .unwrap_or(0)
}

/// Log one grep-friendly `[MEMORY] ...` snapshot. Safe to call
/// on-demand at lifecycle moments (hermes `log_memory_usage`).
pub fn log_memory_usage(prefix: &str) {
    let tag = if prefix.is_empty() {
        String::new()
    } else {
        format!("{prefix} ")
    };
    match rss_mb() {
        Some(rss) => tracing::info!(
            "[MEMORY] {tag}rss={rss}MB threads={threads} uptime={uptime}s",
            threads = thread_count(),
            uptime = uptime_secs(),
        ),
        None => tracing::info!(
            "[MEMORY] {tag}rss=unavailable threads={threads} uptime={uptime}s",
            threads = thread_count(),
            uptime = uptime_secs(),
        ),
    }
}

/// Start periodic logging in a background task (baseline first, then
/// every `interval`). Idempotent — a second call while the monitor runs
/// is a no-op. Returns false when already running or when RSS cannot be
/// read at all (hermes `start_memory_monitoring`).
pub fn start(interval: Duration) -> bool {
    if RUNNING.swap(true, Ordering::SeqCst) {
        return false;
    }
    if rss_mb().is_none() {
        RUNNING.store(false, Ordering::SeqCst);
        tracing::warn!(
            "[MEMORY] memory monitoring unavailable: could not read process RSS — \
             skipping periodic logging."
        );
        return false;
    }
    STARTED_AT.get_or_init(Instant::now);
    STOP.store(false, Ordering::SeqCst);
    log_memory_usage("baseline");
    tokio::spawn(async move {
        let mut since_last = Instant::now();
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if STOP.load(Ordering::SeqCst) {
                break;
            }
            if since_last.elapsed() >= interval {
                since_last = Instant::now();
                log_memory_usage("");
            }
        }
    });
    true
}

/// Stop the background task and log the final snapshot so "last RSS
/// before exit" is always in the log (hermes `stop_memory_monitoring`).
pub fn stop() {
    if RUNNING.load(Ordering::SeqCst) {
        STOP.store(true, Ordering::SeqCst);
        RUNNING.store(false, Ordering::SeqCst);
        log_memory_usage("shutdown");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rss_readable_on_supported_platforms() {
        // Linux (CI) and macOS both expose RSS; assert structure only
        // where unavailable.
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        assert!(rss_mb().is_some());
    }

    #[tokio::test]
    async fn start_is_idempotent_and_stop_logs_final() {
        // RSS is readable on CI platforms, so the first start wins and
        // the second is a no-op. Guard both outcomes to stay portable.
        let first = start(Duration::from_secs(3600));
        if first {
            assert!(!start(Duration::from_secs(3600)), "second start must no-op");
            stop();
            assert!(start(Duration::from_secs(3600)), "restart after stop works");
            stop();
        } else {
            assert!(rss_mb().is_none());
        }
    }

    #[test]
    fn memory_line_prefixes() {
        // Pure smoke: logging must not panic with or without a tag.
        log_memory_usage("");
        log_memory_usage("baseline");
    }
}

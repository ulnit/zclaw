//! Detect when the gateway is running a stale binary after an
//! in-place update — aligned with hermes `gateway/code_skew.py`.
//!
//! The gateway is a single long-lived process. If the binary on disk
//! is replaced underneath it (a rebuild, a package upgrade, or the
//! window before a graceful restart fires), the running process keeps
//! the old image while users expect the new behavior. We snapshot the
//! executable fingerprint at gateway startup and compare on demand, so
//! health surfaces can say "restart the gateway" clearly instead of
//! leaving users to wonder why new features are missing.
//!
//! If the fingerprint cannot be read (IO error, unusual platform), the
//! boot snapshot stays `None` and skew detection no-ops — it never
//! produces a false positive.

use std::sync::{Mutex, OnceLock};

/// Executable fingerprint: modification time (ns since epoch) + size
/// in bytes. A rebuild or reinstall changes at least one of the two.
#[derive(Debug, Clone, PartialEq)]
pub struct ExeFingerprint {
    pub modified_nanos: i64,
    pub size: u64,
}

impl ExeFingerprint {
    /// Compact label for health surfaces (`<mtime>:<size>`).
    pub fn label(&self) -> String {
        format!("{}:{}", self.modified_nanos, self.size)
    }
}

/// Current on-disk fingerprint of the running executable, if readable.
pub fn fingerprint() -> Option<ExeFingerprint> {
    let path = std::env::current_exe().ok()?;
    let meta = std::fs::metadata(&path).ok()?;
    let modified = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    Some(ExeFingerprint {
        modified_nanos: modified.as_nanos() as i64,
        size: meta.len(),
    })
}

fn boot_slot() -> &'static Mutex<Option<ExeFingerprint>> {
    static BOOT: OnceLock<Mutex<Option<ExeFingerprint>>> = OnceLock::new();
    BOOT.get_or_init(|| Mutex::new(None))
}

/// Snapshot the executable at gateway startup (idempotent — the first
/// call wins, mirroring hermes `record_boot_fingerprint`).
pub fn record_boot_fingerprint() {
    let mut slot = boot_slot().lock().unwrap();
    if slot.is_none() {
        *slot = fingerprint();
    }
}

/// Return `(boot_label, disk_label)` when the on-disk binary drifted
/// since boot, else `None`.
pub fn detect_code_skew() -> Option<(String, String)> {
    let boot = boot_slot().lock().unwrap().clone()?;
    let current = fingerprint()?;
    if current == boot {
        return None;
    }
    Some((boot.label(), current.label()))
}

/// Test-only: overwrite the boot snapshot.
#[doc(hidden)]
pub fn set_boot_fingerprint_for_tests(fp: Option<ExeFingerprint>) {
    *boot_slot().lock().unwrap() = fp;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_of_the_running_binary_is_readable() {
        // The test binary itself is a real executable on disk.
        let fp = fingerprint().expect("running binary fingerprint");
        assert!(fp.size > 0);
        assert!(fp.modified_nanos > 0);
        assert!(fp.label().contains(':'));
    }

    #[test]
    fn no_skew_right_after_boot_snapshot() {
        let _guard = crate::models_dev::test_env_lock();
        record_boot_fingerprint();
        // Idempotent: recording again keeps the first snapshot.
        record_boot_fingerprint();
        assert!(detect_code_skew().is_none());
        set_boot_fingerprint_for_tests(None);
    }

    #[test]
    fn skew_reported_when_disk_drifts() {
        let _guard = crate::models_dev::test_env_lock();
        set_boot_fingerprint_for_tests(Some(ExeFingerprint {
            modified_nanos: 1_000_000,
            size: 42,
        }));
        let (boot, disk) = detect_code_skew().expect("drifted binary reports skew");
        assert_eq!(boot, "1000000:42");
        assert_ne!(disk, boot);
        // Without a boot snapshot, detection no-ops (never a false
        // positive).
        set_boot_fingerprint_for_tests(None);
        assert!(detect_code_skew().is_none());
    }
}

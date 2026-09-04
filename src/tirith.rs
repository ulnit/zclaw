//! Tirith pre-exec security scanning — port of hermes' `tools/tirith_security.py`.
//!
//! Runs the external `tirith` binary to scan commands for content-level
//! threats (homograph URLs, pipe-to-interpreter, terminal injection, ...).
//! The exit code is the verdict source of truth (0 = allow, 1 = block,
//! 2 = warn); JSON stdout only enriches findings/summary and never
//! overrides the verdict. Operational failures (spawn error, timeout,
//! unknown exit code) respect the `tirith_fail_open` setting.
//!
//! Auto-install: when tirith is not on PATH or at `<home>/bin/tirith`, it
//! is downloaded from GitHub releases (SHA-256 always verified; cosign
//! provenance verification when cosign is on PATH). Startup install runs in
//! a background thread; failures persist to a 24h disk marker.

use crate::config::SecurityConfig;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{atomic::AtomicBool, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

const REPO: &str = "sheeki03/tirith";
/// Cosign provenance verification — pinned to the release workflow.
const COSIGN_IDENTITY_REGEXP: &str =
    "^https://github.com/sheeli03/tirith/\\.github/workflows/release\\.yml@refs/tags/v";
const COSIGN_ISSUER: &str = "https://token.actions.githubusercontent.com";
/// Circuit breaker: after this many consecutive spawn/execution failures,
/// disable tirith for the rest of the process (hermes #41400).
const CRASH_LIMIT: u32 = 3;
/// Disk failure-marker TTL (24 h).
const MARKER_TTL_SECS: u64 = 86_400;
const MAX_FINDINGS: usize = 50;
const MAX_SUMMARY_LEN: usize = 500;
const DOWNLOAD_TIMEOUT_SECS: u64 = 10;

/// Verdict action for a scanned command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TirithAction {
    #[default]
    Allow,
    Warn,
    Block,
}

/// Result of a tirith scan.
#[derive(Debug, Clone, Default)]
pub struct TirithVerdict {
    pub action: TirithAction,
    pub findings: Vec<Value>,
    pub summary: String,
}

/// Effective tirith settings after env overrides (hermes
/// `_load_security_config`: `TIRITH_ENABLED` / `TIRITH_BIN` /
/// `TIRITH_TIMEOUT` / `TIRITH_FAIL_OPEN`).
#[derive(Debug, Clone)]
pub struct TirithConfig {
    pub enabled: bool,
    pub path: String,
    pub timeout_secs: u64,
    pub fail_open: bool,
}

pub fn resolve_tirith_config(security: &SecurityConfig) -> TirithConfig {
    TirithConfig {
        enabled: env_bool("TIRITH_ENABLED", security.tirith_enabled),
        path: std::env::var("TIRITH_BIN").unwrap_or_else(|_| security.tirith_path.clone()),
        timeout_secs: env_int("TIRITH_TIMEOUT", security.tirith_timeout),
        fail_open: env_bool("TIRITH_FAIL_OPEN", security.tirith_fail_open),
    }
}

fn env_bool(key: &str, default: bool) -> bool {
    match std::env::var(key) {
        Ok(v) => matches!(v.to_lowercase().as_str(), "1" | "true" | "yes"),
        Err(_) => default,
    }
}

fn env_int(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

// ---------------------------------------------------------------------------
// Process-wide state (hermes module globals)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct TirithState {
    /// Configured path that produced `resolved_path` (the cache invalidates
    /// when the configured path changes — hermes keeps a single global slot
    /// because its config is process-fixed; keying keeps ulnclaw's
    /// multi-config callers/tests correct).
    resolved_for: Option<String>,
    resolved_path: Option<String>,
    install_failed: bool,
    install_failure_reason: String,
    crash_count: u32,
    circuit_open: bool,
    install_thread_active: bool,
    warned: BTreeSet<String>,
}

static STATE: LazyLock<Mutex<TirithState>> = LazyLock::new(|| Mutex::new(TirithState::default()));

fn state() -> MutexGuard<'static, TirithState> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Warn at most once per key for the process lifetime (hermes `_warn_once`)
/// — keeps fail-open misconfiguration from flooding the log per command.
fn warn_once(state: &mut TirithState, key: &str, message: String) {
    if state.warned.contains(key) {
        return;
    }
    state.warned.insert(key.to_string());
    eprintln!("[tirith] {}", message);
}

fn record_crash(state: &mut TirithState) {
    state.crash_count += 1;
    if state.crash_count >= CRASH_LIMIT && !state.circuit_open {
        state.circuit_open = true;
        eprintln!(
            "[tirith] circuit breaker opened after {} consecutive failures; disabling for the rest of the process",
            state.crash_count
        );
    }
}

/// Reset all process-wide state — tests only.
#[cfg(test)]
pub fn reset_state_for_tests() {
    let mut s = state();
    *s = TirithState::default();
}

// ---------------------------------------------------------------------------
// Platform support
// ---------------------------------------------------------------------------

/// Rust target triple for this platform, or None when tirith ships no
/// build (Windows — callers silently fall back to pattern guards).
pub fn detect_target() -> Option<&'static str> {
    let plat = match std::env::consts::OS {
        "macos" => "apple-darwin",
        // Android (Termux) is ABI-compatible with Linux.
        "linux" | "android" => "unknown-linux-gnu",
        _ => return None,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        _ => return None,
    };
    // Leaking a tiny static-formatted string is not worth a OnceLock per
    // combo; the set is closed and minuscule.
    Some(match (arch, plat) {
        ("x86_64", "apple-darwin") => "x86_64-apple-darwin",
        ("x86_64", "unknown-linux-gnu") => "x86_64-unknown-linux-gnu",
        ("aarch64", "apple-darwin") => "aarch64-apple-darwin",
        ("aarch64", "unknown-linux-gnu") => "aarch64-unknown-linux-gnu",
        _ => return None,
    })
}

pub fn is_platform_supported() -> bool {
    detect_target().is_some()
}

// ---------------------------------------------------------------------------
// Install-failure marker (disk persistence, 24h TTL)
// ---------------------------------------------------------------------------

fn failure_marker_path() -> PathBuf {
    crate::config::ulnclaw_home().join(".tirith-install-failed")
}

fn read_failure_reason() -> Option<String> {
    let path = failure_marker_path();
    let meta = std::fs::metadata(&path).ok()?;
    let mtime = meta.modified().ok()?;
    let age = Instant::now().duration_since(system_time_as_instant(mtime));
    if age.as_secs() >= MARKER_TTL_SECS {
        return None;
    }
    std::fs::read_to_string(&path)
        .ok()
        .map(|s| s.trim().to_string())
}

fn system_time_as_instant(t: std::time::SystemTime) -> Instant {
    match t.elapsed() {
        Ok(elapsed) => Instant::now() - elapsed,
        Err(_) => Instant::now(),
    }
}

fn mark_install_failed(reason: &str) {
    if let Some(parent) = failure_marker_path().parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(failure_marker_path(), reason).ok();
}

fn clear_install_failed(state: &mut TirithState) {
    // Reset the warn-once dedupe so a later failure surfaces again (hermes
    // `_reset_spawn_warning_state`).
    state.warned.clear();
    std::fs::remove_file(failure_marker_path()).ok();
}

fn is_install_failed_on_disk() -> bool {
    let Some(reason) = read_failure_reason() else {
        return false;
    };
    if reason == "cosign_missing" && crate::env_probe::which("cosign").is_some() {
        std::fs::remove_file(failure_marker_path()).ok();
        return false;
    }
    true
}

fn ulnclaw_bin_dir() -> PathBuf {
    let dir = crate::config::ulnclaw_home().join("bin");
    std::fs::create_dir_all(&dir).ok();
    dir
}

// ---------------------------------------------------------------------------
// Auto-install
// ---------------------------------------------------------------------------

fn expanduser(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest).display().to_string();
        }
    }
    path.to_string()
}

fn is_explicit_path(configured: &str) -> bool {
    configured != "tirith"
}

fn download_file(url: &str, dest: &Path) -> Result<(), String> {
    let mut request = reqwest::blocking::Request::new(
        reqwest::Method::GET,
        url.parse().map_err(|e| format!("bad url: {}", e))?,
    );
    if let Some(token) = crate::secret_scope::get_secret_lenient("GITHUB_TOKEN", None) {
        if !token.is_empty() {
            request.headers_mut().insert(
                reqwest::header::AUTHORIZATION,
                format!("token {}", token).parse().unwrap(),
            );
        }
    }
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(DOWNLOAD_TIMEOUT_SECS))
        .build()
        .map_err(|e| format!("http client: {}", e))?;
    let response = client
        .execute(request)
        .map_err(|e| format!("download {}: {}", url, e))?;
    if !response.status().is_success() {
        return Err(format!("download {}: HTTP {}", url, response.status()));
    }
    let bytes = response.bytes().map_err(|e| format!("read body: {}", e))?;
    std::fs::write(dest, &bytes).map_err(|e| format!("write {}: {}", dest.display(), e))
}

/// Verify cosign provenance on checksums.txt. `Some(true)` verified,
/// `Some(false)` rejected, `None` cosign unavailable/broken.
fn verify_cosign(checksums: &Path, sig: &Path, cert: &Path) -> Option<bool> {
    let cosign = crate::env_probe::which("cosign")?;
    let result = Command::new(cosign)
        .args([
            "verify-blob",
            "--certificate",
            &cert.display().to_string(),
            "--signature",
            &sig.display().to_string(),
            "--certificate-identity-regexp",
            COSIGN_IDENTITY_REGEXP,
            "--certificate-oidc-issuer",
            COSIGN_ISSUER,
            &checksums.display().to_string(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();
    let Ok(output) = result else {
        return None;
    };
    if output.status.success() {
        Some(true)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprintln!("[tirith] cosign verification failed: {}", stderr.trim());
        Some(false)
    }
}

fn verify_checksum(archive: &Path, checksums: &Path, archive_name: &str) -> bool {
    let Ok(text) = std::fs::read_to_string(checksums) else {
        return false;
    };
    let expected = text.lines().find_map(|line| {
        let mut parts = line.trim().splitn(2, "  ");
        let hash = parts.next()?;
        let name = parts.next()?;
        (name == archive_name).then(|| hash.to_string())
    });
    let Some(expected) = expected else {
        eprintln!("[tirith] no checksum entry for {}", archive_name);
        return false;
    };
    let mut sha = Sha256::new();
    let Ok(mut file) = std::fs::File::open(archive) else {
        return false;
    };
    let mut buffer = [0u8; 8192];
    loop {
        let Ok(n) = file.read(&mut buffer) else {
            return false;
        };
        if n == 0 {
            break;
        }
        sha.update(&buffer[..n]);
    }
    let actual: String = sha
        .finalize()
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect();
    if actual != expected {
        eprintln!(
            "[tirith] checksum mismatch: expected {}, got {}",
            expected, actual
        );
        return false;
    }
    true
}

/// Extract the tirith binary from the release archive (hermes
/// `_extract_tirith_binary`: only a regular file named `tirith`, no `..`).
fn extract_tirith_binary(archive: &Path, dest: &Path) -> Result<(), String> {
    let file = std::fs::File::open(archive).map_err(|e| format!("open archive: {}", e))?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut tar = tar::Archive::new(decoder);
    for entry in tar.entries().map_err(|e| format!("read archive: {}", e))? {
        let mut entry = entry.map_err(|e| format!("archive entry: {}", e))?;
        let path = entry.path().map_err(|e| format!("entry path: {}", e))?;
        let name = path.to_string_lossy().replace('\\', "/");
        if name.contains("..") {
            continue;
        }
        if !(name == "tirith" || name.ends_with("/tirith")) {
            continue;
        }
        if !entry.header().entry_type().is_file() {
            return Err("binary_not_regular_file".into());
        }
        let mut bytes = Vec::new();
        entry
            .read_to_end(&mut bytes)
            .map_err(|_| "binary_extract_failed".to_string())?;
        std::fs::write(dest, &bytes).map_err(|e| format!("write binary: {}", e))?;
        return Ok(());
    }
    Err("binary_not_in_archive".into())
}

/// Download + verify + install tirith to `<home>/bin/tirith`.
/// Returns the installed path or a short failure-reason tag.
pub fn install_tirith(log_failures: bool) -> Result<PathBuf, String> {
    let log = |msg: String| {
        if log_failures {
            eprintln!("[tirith] {}", msg);
        }
    };
    let Some(target) = detect_target() else {
        return Err("unsupported_platform".into());
    };
    let archive_name = format!("tirith-{}.tar.gz", target);
    let base_url = format!("https://github.com/{}/releases/latest/download", REPO);

    let tmpdir = std::env::temp_dir().join(format!(
        "ulnclaw-tirith-install-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmpdir).map_err(|_| "no_space".to_string())?;
    let result = install_into(&tmpdir, &archive_name, &base_url, &log);
    std::fs::remove_dir_all(&tmpdir).ok();
    result
}

fn install_into(
    tmpdir: &Path,
    archive_name: &str,
    base_url: &str,
    log: &dyn Fn(String),
) -> Result<PathBuf, String> {
    let archive_path = tmpdir.join(archive_name);
    let checksums_path = tmpdir.join("checksums.txt");
    let sig_path = tmpdir.join("checksums.txt.sig");
    let cert_path = tmpdir.join("checksums.txt.pem");

    eprintln!(
        "[tirith] not found — downloading latest release for {}...",
        detect_target().unwrap_or("?")
    );
    download_file(&format!("{}/{}", base_url, archive_name), &archive_path).map_err(|e| {
        log(format!("download failed: {}", e));
        "download_failed".to_string()
    })?;
    download_file(&format!("{}/checksums.txt", base_url), &checksums_path).map_err(|e| {
        log(format!("download failed: {}", e));
        "download_failed".to_string()
    })?;

    // Cosign provenance verification — preferred but not mandatory.
    let mut cosign_verified = false;
    if crate::env_probe::which("cosign").is_some() {
        let artifacts = download_file(&format!("{}/checksums.txt.sig", base_url), &sig_path)
            .and_then(|_| download_file(&format!("{}/checksums.txt.pem", base_url), &cert_path));
        match artifacts {
            Ok(()) => match verify_cosign(&checksums_path, &sig_path, &cert_path) {
                Some(true) => cosign_verified = true,
                Some(false) => {
                    log("install aborted: cosign provenance verification failed".into());
                    return Err("cosign_verification_failed".into());
                }
                None => {} // cosign itself broken — SHA-256 only
            },
            Err(_) => {} // artifacts unavailable — SHA-256 only
        }
    } else {
        eprintln!("[tirith] cosign not on PATH — installing with SHA-256 verification only");
    }

    if !verify_checksum(&archive_path, &checksums_path, archive_name) {
        return Err("checksum_failed".into());
    }

    let staged = tmpdir.join("tirith");
    extract_tirith_binary(&archive_path, &staged)?;

    let dest = ulnclaw_bin_dir().join("tirith");
    if std::fs::rename(&staged, &dest).is_err() {
        // Cross-device fallback (hermes shutil.move → copy).
        if std::fs::copy(&staged, &dest).is_err() {
            std::fs::remove_file(&dest).ok();
            return Err("cross_device_copy_failed".into());
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(&dest) {
            let mode = meta.permissions().mode() | 0o111;
            std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(mode)).ok();
        }
    }
    let verification = if cosign_verified {
        "cosign + SHA-256"
    } else {
        "SHA-256 only"
    };
    eprintln!(
        "[tirith] installed to {} ({})",
        dest.display(),
        verification
    );
    Ok(dest)
}

/// Resolve the tirith binary path, auto-installing if necessary (hermes
/// `_resolve_tirith_path`). An explicitly configured path is authoritative
/// — never replaced by a download.
fn resolve_tirith_path(cfg: &TirithConfig) -> String {
    let mut state = state();
    if state.resolved_for.as_deref() == Some(cfg.path.as_str()) {
        if let Some(path) = &state.resolved_path {
            return path.clone();
        }
    }
    let expanded = expanduser(&cfg.path);
    let explicit = is_explicit_path(&cfg.path);
    let install_failed = state.install_failed;

    if !explicit && !is_platform_supported() {
        state.install_failed = true;
        state.install_failure_reason = "unsupported_platform".into();
        return expanded;
    }

    if explicit {
        let path = Path::new(&expanded);
        if path.is_file() && is_executable(path) {
            state.resolved_for = Some(cfg.path.clone());
            state.resolved_path = Some(expanded.clone());
            return expanded;
        }
        if let Some(found) = crate::env_probe::which(&expanded) {
            let found = found.display().to_string();
            state.resolved_for = Some(cfg.path.clone());
            state.resolved_path = Some(found.clone());
            return found;
        }
        warn_once(
            &mut state,
            "tirith_explicit_missing",
            format!(
                "configured tirith path {:?} not found; scanning disabled",
                cfg.path
            ),
        );
        state.install_failed = true;
        state.install_failure_reason = "explicit_path_missing".into();
        return expanded;
    }

    // Default "tirith": cheap local checks always re-run so a manual
    // install is picked up even after a previous network failure.
    if let Some(found) = crate::env_probe::which("tirith") {
        let found = found.display().to_string();
        state.resolved_for = Some(cfg.path.clone());
        state.resolved_path = Some(found.clone());
        state.install_failure_reason.clear();
        clear_install_failed(&mut state);
        return found;
    }
    let home_bin = ulnclaw_bin_dir().join("tirith");
    if home_bin.is_file() && is_executable(&home_bin) {
        let found = home_bin.display().to_string();
        state.resolved_for = Some(cfg.path.clone());
        state.resolved_path = Some(found.clone());
        state.install_failure_reason.clear();
        clear_install_failed(&mut state);
        return found;
    }

    if install_failed {
        if state.install_failure_reason == "cosign_missing"
            && crate::env_probe::which("cosign").is_some()
        {
            state.install_failed = false;
            state.install_failure_reason.clear();
            clear_install_failed(&mut state);
        } else {
            return expanded;
        }
    }

    if let Some(reason) = read_failure_reason() {
        if is_install_failed_on_disk() {
            state.install_failed = true;
            state.install_failure_reason = reason;
            return expanded;
        }
    }

    match install_tirith(true) {
        Ok(path) => {
            let found = path.display().to_string();
            state.resolved_for = Some(cfg.path.clone());
            state.resolved_path = Some(found.clone());
            state.install_failure_reason.clear();
            clear_install_failed(&mut state);
            found
        }
        Err(reason) => {
            state.install_failed = true;
            state.install_failure_reason = reason.clone();
            mark_install_failed(&reason);
            expanded
        }
    }
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Ensure tirith is available, downloading in a background thread if
/// needed (hermes `ensure_installed`). Quick PATH/local checks are
/// synchronous; the network download never blocks startup.
pub fn ensure_installed(security: &SecurityConfig) {
    static STARTED: AtomicBool = AtomicBool::new(false);
    let cfg = resolve_tirith_config(security);
    if !cfg.enabled {
        return;
    }
    {
        let mut state = state();
        if let Some(path) = &state.resolved_path {
            if Path::new(path).is_file() && is_executable(Path::new(path)) {
                return;
            }
        }
        if !is_platform_supported() {
            state.install_failed = true;
            state.install_failure_reason = "unsupported_platform".into();
            return;
        }
        let expanded = expanduser(&cfg.path);
        if is_explicit_path(&cfg.path) {
            let path = Path::new(&expanded);
            if path.is_file() && is_executable(path) {
                state.resolved_path = Some(expanded);
            } else if let Some(found) = crate::env_probe::which(&expanded) {
                state.resolved_path = Some(found.display().to_string());
            } else {
                state.install_failed = true;
                state.install_failure_reason = "explicit_path_missing".into();
            }
            return;
        }
        if let Some(found) = crate::env_probe::which("tirith") {
            state.resolved_path = Some(found.display().to_string());
            clear_install_failed(&mut state);
            return;
        }
        let home_bin = ulnclaw_bin_dir().join("tirith");
        if home_bin.is_file() && is_executable(&home_bin) {
            state.resolved_path = Some(home_bin.display().to_string());
            clear_install_failed(&mut state);
            return;
        }
        if state.install_thread_active {
            return;
        }
        if let Some(reason) = read_failure_reason() {
            if is_install_failed_on_disk() {
                state.install_failed = true;
                state.install_failure_reason = reason;
                return;
            }
        }
        state.install_thread_active = true;
    }
    if STARTED
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        )
        .is_err()
    {
        return;
    }
    std::thread::spawn(|| {
        let outcome = install_tirith(true);
        let mut state = state();
        state.install_thread_active = false;
        match outcome {
            Ok(path) => {
                state.resolved_path = Some(path.display().to_string());
                state.install_failure_reason.clear();
                clear_install_failed(&mut state);
            }
            Err(reason) => {
                state.install_failed = true;
                state.install_failure_reason = reason.clone();
                mark_install_failed(&reason);
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Main API
// ---------------------------------------------------------------------------

/// Run a tirith security scan on a command (hermes `check_command_security`).
///
/// Exit code decides (0 allow / 1 block / 2 warn); JSON enriches.
/// Spawn failures, timeouts and unknown exit codes respect `fail_open`.
/// Never panics for operational failures.
pub fn check_command_security(command: &str, security: &SecurityConfig) -> TirithVerdict {
    let cfg = resolve_tirith_config(security);
    if !cfg.enabled {
        return TirithVerdict::default();
    }
    {
        let state = state();
        if state.circuit_open {
            return TirithVerdict {
                action: TirithAction::Allow,
                findings: vec![],
                summary: "tirith disabled (circuit breaker)".into(),
            };
        }
    }

    if !is_platform_supported() {
        return TirithVerdict::default();
    }

    let tirith_path = resolve_tirith_path(&cfg);

    let mut child = match Command::new(&tirith_path)
        .args([
            "check",
            "--json",
            "--non-interactive",
            "--shell",
            "posix",
            "--",
            command,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(exc) => {
            let mut state = state();
            let key = format!("tirith_spawn_failed:{}", exc.kind());
            warn_once(&mut state, &key, format!("tirith spawn failed: {}", exc));
            record_crash(&mut state);
            return operational_failure(&cfg, format!("tirith unavailable: {}", exc));
        }
    };

    // Poll with a deadline (std Command has no wait timeout).
    let deadline = Instant::now() + Duration::from_secs(cfg.timeout_secs.max(1));
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    child.kill().ok();
                    child.wait().ok();
                    let mut state = state();
                    warn_once(
                        &mut state,
                        &format!("tirith_timeout:{}", cfg.timeout_secs),
                        format!("tirith timed out after {}s", cfg.timeout_secs),
                    );
                    record_crash(&mut state);
                    return operational_failure(
                        &cfg,
                        format!("tirith timed out ({}s)", cfg.timeout_secs),
                    );
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(exc) => {
                let mut state = state();
                warn_once(
                    &mut state,
                    "tirith_wait_failed",
                    format!("tirith wait failed: {}", exc),
                );
                record_crash(&mut state);
                return operational_failure(&cfg, format!("tirith unavailable: {}", exc));
            }
        }
    };

    let mut stdout = String::new();
    if let Some(mut out) = child.stdout.take() {
        out.read_to_string(&mut stdout).ok();
    }

    let Some(exit_code) = status.code() else {
        // Signal-killed (e.g. SIGSEGV) — unknown verdict, fail_open applies.
        let mut state = state();
        record_crash(&mut state);
        return operational_failure(&cfg, "tirith killed by signal (fail-open)".into());
    };

    let action = match exit_code {
        0 => {
            state().crash_count = 0; // successful run resets the breaker
            TirithAction::Allow
        }
        1 => TirithAction::Block,
        2 => TirithAction::Warn,
        other => {
            let mut state = state();
            warn_once(
                &mut state,
                &format!("tirith_exit:{}", other),
                format!("tirith returned unexpected exit code {}", other),
            );
            record_crash(&mut state);
            return operational_failure(&cfg, format!("tirith exit code {}", other));
        }
    };

    // JSON enrichment never overrides the exit-code verdict. Empty stdout
    // counts as an empty object (hermes), not as a parse failure.
    let mut findings: Vec<Value> = vec![];
    let mut summary = String::new();
    let parsed: Option<Value> = if stdout.trim().is_empty() {
        Some(Value::Object(serde_json::Map::new()))
    } else {
        serde_json::from_str(&stdout).ok()
    };
    match parsed {
        Some(data) => {
            findings = data
                .get("findings")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .take(MAX_FINDINGS)
                .collect();
            summary = data
                .get("summary")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .chars()
                .take(MAX_SUMMARY_LEN)
                .collect();
        }
        None => {
            // JSON parse failure degrades findings/summary, not the verdict.
            if action == TirithAction::Block {
                summary = "security issue detected (details unavailable)".into();
            } else if action == TirithAction::Warn {
                summary = "security warning detected (details unavailable)".into();
            }
        }
    }

    // Suppress warn verdicts that consist solely of lookalike_tld findings
    // for the .app TLD — a legitimate gTLD producing false positives
    // (hermes `_is_app_tld_finding`).
    if action == TirithAction::Warn && !findings.is_empty() {
        let all_app_tld = findings.iter().all(is_app_tld_finding);
        if all_app_tld {
            return TirithVerdict::default();
        }
    }

    TirithVerdict {
        action,
        findings,
        summary,
    }
}

fn operational_failure(cfg: &TirithConfig, summary: String) -> TirithVerdict {
    if cfg.fail_open {
        TirithVerdict {
            action: TirithAction::Allow,
            findings: vec![],
            summary: format!("{} (fail-open)", summary),
        }
    } else {
        TirithVerdict {
            action: TirithAction::Block,
            findings: vec![],
            summary: format!("{} (fail-closed)", summary),
        }
    }
}

fn is_app_tld_finding(finding: &Value) -> bool {
    let Some(obj) = finding.as_object() else {
        return false;
    };
    if obj.get("rule_id").and_then(|v| v.as_str()) != Some("lookalike_tld") {
        return false;
    }
    ["value", "tld", "detail", "description", "message"]
        .iter()
        .any(|field| {
            obj.get(*field)
                .and_then(|v| v.as_str())
                .map(|v| v.to_lowercase().contains(".app"))
                .unwrap_or(false)
        })
}

/// Human-readable description of a verdict for approval prompts (hermes
/// `_format_tirith_description`).
pub fn format_description(verdict: &TirithVerdict) -> String {
    let mut parts: Vec<String> = vec![];
    for finding in &verdict.findings {
        let severity = finding
            .get("severity")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let title = finding.get("title").and_then(|v| v.as_str()).unwrap_or("");
        let desc = finding
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !title.is_empty() && !desc.is_empty() {
            parts.push(if severity.is_empty() {
                format!("{}: {}", title, desc)
            } else {
                format!("[{}] {}: {}", severity, title, desc)
            });
        } else if !title.is_empty() {
            parts.push(if severity.is_empty() {
                title.to_string()
            } else {
                format!("[{}] {}", severity, title)
            });
        }
    }
    if parts.is_empty() {
        let summary = if verdict.summary.is_empty() {
            "security issue detected"
        } else {
            &verdict.summary
        };
        return format!("Security scan: {}", summary);
    }
    format!("Security scan — {}", parts.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn security_cfg(path: &str, fail_open: bool) -> SecurityConfig {
        SecurityConfig {
            allow_private_urls: false,
            tirith_enabled: true,
            tirith_path: path.to_string(),
            tirith_timeout: 5,
            tirith_fail_open: fail_open,
        }
    }

    /// Fake tirith: exits with the given code, printing $TIRITH_FAKE_JSON
    /// verbatim when set.
    fn write_fake_tirith(dir: &Path, exit_code: i32) -> PathBuf {
        let path = dir.join(format!("fake-tirith-{}", exit_code));
        let script = format!(
            "#!/bin/sh\nif [ -n \"$TIRITH_FAKE_JSON\" ]; then printf '%s' \"$TIRITH_FAKE_JSON\"; fi\nexit {}\n",
            exit_code
        );
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(script.as_bytes()).unwrap();
        drop(f);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    #[test]
    fn exit_codes_map_to_actions() {
        let _guard = crate::models_dev::test_env_lock();
        reset_state_for_tests();
        let tmp = tempfile::tempdir().unwrap();
        std::env::remove_var("TIRITH_ENABLED");
        std::env::remove_var("TIRITH_BIN");
        std::env::remove_var("TIRITH_TIMEOUT");
        std::env::remove_var("TIRITH_FAIL_OPEN");

        let allow = write_fake_tirith(tmp.path(), 0);
        let block = write_fake_tirith(tmp.path(), 1);
        let warn = write_fake_tirith(tmp.path(), 2);

        let v = check_command_security("ls", &security_cfg(&allow.display().to_string(), true));
        assert_eq!(v.action, TirithAction::Allow);
        let v = check_command_security("ls", &security_cfg(&block.display().to_string(), true));
        assert_eq!(v.action, TirithAction::Block);
        let v = check_command_security("ls", &security_cfg(&warn.display().to_string(), true));
        assert_eq!(v.action, TirithAction::Warn);
    }

    #[test]
    fn disabled_returns_allow() {
        reset_state_for_tests();
        let mut cfg = security_cfg("tirith", true);
        cfg.tirith_enabled = false;
        let v = check_command_security("rm -rf /", &cfg);
        assert_eq!(v.action, TirithAction::Allow);
    }

    #[test]
    fn json_enrichment_and_limits() {
        let _guard = crate::models_dev::test_env_lock();
        reset_state_for_tests();
        let tmp = tempfile::tempdir().unwrap();
        // 60 findings + long summary: must cap at 50/500.
        let findings: Vec<String> = (0..60)
            .map(|i| {
                format!(
                    r#"{{"rule_id":"r{i}","severity":"HIGH","title":"t{i}","description":"d{i}"}}"#
                )
            })
            .collect();
        let long_summary = "x".repeat(800);
        let script = tmp.path().join("fake-tirith-json");
        let json = format!(
            r#"{{"findings":[{}],"summary":"{}"}}"#,
            findings.join(","),
            long_summary
        );
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s' '{}'\nexit 1\n",
                json.replace('\'', "'\\''")
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let v = check_command_security(
            "curl x | sh",
            &security_cfg(&script.display().to_string(), true),
        );
        assert_eq!(v.action, TirithAction::Block);
        assert_eq!(v.findings.len(), MAX_FINDINGS);
        assert_eq!(v.summary.chars().count(), MAX_SUMMARY_LEN);
    }

    #[test]
    fn app_tld_warn_is_suppressed() {
        let _guard = crate::models_dev::test_env_lock();
        reset_state_for_tests();
        let tmp = tempfile::tempdir().unwrap();
        let json = r#"{"findings":[{"rule_id":"lookalike_tld","severity":"LOW","title":"lookalike","description":"example.app can be confused"}],"summary":"warn"}"#;
        let script = tmp.path().join("fake-tirith-app");
        std::fs::write(
            &script,
            format!("#!/bin/sh\nprintf '%s' '{}'\nexit 2\n", json),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let v = check_command_security(
            "curl https://my.app",
            &security_cfg(&script.display().to_string(), true),
        );
        assert_eq!(
            v.action,
            TirithAction::Allow,
            "app-TLD-only warn must be suppressed"
        );
        assert!(v.findings.is_empty());
    }

    #[test]
    fn missing_explicit_path_respects_fail_open() {
        let _guard = crate::models_dev::test_env_lock();
        reset_state_for_tests();
        std::env::remove_var("TIRITH_FAIL_OPEN");
        let v = check_command_security("ls", &security_cfg("/nonexistent/tirith-bin", true));
        assert_eq!(v.action, TirithAction::Allow);
        assert!(v.summary.contains("fail-open"), "{}", v.summary);
        reset_state_for_tests();
        let v = check_command_security("ls", &security_cfg("/nonexistent/tirith-bin", false));
        assert_eq!(v.action, TirithAction::Block);
        assert!(v.summary.contains("fail-closed"), "{}", v.summary);
    }

    #[test]
    fn unknown_exit_code_respects_fail_open() {
        let _guard = crate::models_dev::test_env_lock();
        reset_state_for_tests();
        let tmp = tempfile::tempdir().unwrap();
        let weird = write_fake_tirith(tmp.path(), 7);
        let v = check_command_security("ls", &security_cfg(&weird.display().to_string(), true));
        assert_eq!(v.action, TirithAction::Allow);
        assert!(v.summary.contains("exit code 7"), "{}", v.summary);
    }

    #[test]
    fn timeout_respects_fail_open() {
        let _guard = crate::models_dev::test_env_lock();
        reset_state_for_tests();
        std::env::remove_var("TIRITH_TIMEOUT");
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("fake-tirith-slow");
        std::fs::write(&script, "#!/bin/sh\nsleep 5\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut cfg = security_cfg(&script.display().to_string(), true);
        cfg.tirith_timeout = 1;
        let start = Instant::now();
        let v = check_command_security("ls", &cfg);
        assert!(start.elapsed() < Duration::from_secs(4));
        assert_eq!(v.action, TirithAction::Allow);
        assert!(v.summary.contains("timed out"), "{}", v.summary);
    }

    #[test]
    fn circuit_breaker_opens_after_crash_limit() {
        let _guard = crate::models_dev::test_env_lock();
        reset_state_for_tests();
        let tmp = tempfile::tempdir().unwrap();
        let weird = write_fake_tirith(tmp.path(), 9);
        let cfg = security_cfg(&weird.display().to_string(), true);
        for _ in 0..CRASH_LIMIT {
            let v = check_command_security("ls", &cfg);
            assert_eq!(v.action, TirithAction::Allow);
        }
        // Breaker open: verdict says so and no scan runs.
        let v = check_command_security("ls", &cfg);
        assert_eq!(v.action, TirithAction::Allow);
        assert!(v.summary.contains("circuit breaker"), "{}", v.summary);
    }

    #[test]
    fn platform_target_shape() {
        // Whatever the host, support flag and triple must agree.
        assert_eq!(is_platform_supported(), detect_target().is_some());
        if let Some(target) = detect_target() {
            assert!(target.contains("apple-darwin") || target.contains("unknown-linux-gnu"));
        }
    }

    #[test]
    fn format_description_prefers_findings() {
        let verdict = TirithVerdict {
            action: TirithAction::Warn,
            findings: vec![serde_json::json!({
                "severity": "HIGH",
                "title": "Pipe to interpreter",
                "description": "curl | sh"
            })],
            summary: "ignored".into(),
        };
        assert_eq!(
            format_description(&verdict),
            "Security scan — [HIGH] Pipe to interpreter: curl | sh"
        );
        let empty = TirithVerdict {
            action: TirithAction::Block,
            findings: vec![],
            summary: "".into(),
        };
        assert_eq!(
            format_description(&empty),
            "Security scan: security issue detected"
        );
    }

    #[test]
    fn config_env_overrides() {
        let _guard = crate::models_dev::test_env_lock();
        let security = security_cfg("tirith", true);
        std::env::set_var("TIRITH_ENABLED", "0");
        std::env::set_var("TIRITH_TIMEOUT", "9");
        std::env::set_var("TIRITH_FAIL_OPEN", "no");
        std::env::set_var("TIRITH_BIN", "/opt/tirith");
        let cfg = resolve_tirith_config(&security);
        assert!(!cfg.enabled);
        assert_eq!(cfg.timeout_secs, 9);
        assert!(!cfg.fail_open);
        assert_eq!(cfg.path, "/opt/tirith");
        std::env::remove_var("TIRITH_ENABLED");
        std::env::remove_var("TIRITH_TIMEOUT");
        std::env::remove_var("TIRITH_FAIL_OPEN");
        std::env::remove_var("TIRITH_BIN");
    }
}

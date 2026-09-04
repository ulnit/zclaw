//! External secret sources (hermes `agent/secret_sources/` port).
//!
//! Resolves credentials from external secret managers into
//! environment-variable-shaped values at process startup, after `.env`
//! loads and before providers read the environment. Contract (hermes
//! `base.py`, deliberately not widened): read-only, startup-time and
//! synchronous, never raises and never prompts — failures degrade to a
//! one-line warning and startup continues with whatever `.env` already
//! had. Sources fetch; the orchestrator ([`apply_to_env`]) owns
//! precedence and the actual writes.
//!
//! Sources ported: `command` (user helper via `/bin/sh -c`) and
//! `bitwarden` (Bitwarden Secrets Manager via the `bws` CLI). The
//! 1Password source stays unported.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// `[secrets]` config (hermes `secrets.*` in config.yaml).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SecretsConfig {
    /// Source application order (first claim wins). Unknown names are
    /// ignored. Default when empty: `["command", "bitwarden"]`.
    #[serde(default)]
    pub sources: Vec<String>,
    /// Pre-existing env values always win for these names, even against
    /// a source with `override_existing = true` (hermes
    /// `secrets.preserve_existing`).
    #[serde(default)]
    pub preserve_existing: Vec<String>,
    #[serde(default)]
    pub command: CommandSourceConfig,
    #[serde(default)]
    pub bitwarden: BitwardenSourceConfig,
    #[serde(default)]
    pub onepassword: OnePasswordSourceConfig,
}

/// `[secrets.command]` — resolve secrets via a user-configured helper
/// (hermes `command.py`: keepassxc-cli / secret-tool / tmpfs cat ...).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandSourceConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Shell command run via `/bin/sh -c`. Comes from config.toml only —
    /// never from `.env`.
    #[serde(default)]
    pub command: String,
    /// Hard timeout; any overrun degrades to "no value".
    #[serde(default = "default_command_timeout")]
    pub timeout_seconds: u64,
}

fn default_command_timeout() -> u64 {
    3
}

impl Default for CommandSourceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            command: String::new(),
            timeout_seconds: default_command_timeout(),
        }
    }
}

/// `[secrets.bitwarden]` — Bitwarden Secrets Manager via `bws`
/// (hermes `bitwarden.py`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BitwardenSourceConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Env var holding the bootstrap access token.
    #[serde(default = "default_token_env")]
    pub access_token_env: String,
    #[serde(default)]
    pub project_id: String,
    /// Non-default region / self-hosted endpoint (passed to the
    /// subprocess as `BWS_SERVER_URL`).
    #[serde(default)]
    pub server_url: String,
    /// May beat pre-existing `.env`/shell values (never another source).
    #[serde(default)]
    pub override_existing: bool,
    /// Fetch-cache TTL in seconds; 0 disables BOTH cache layers (hermes
    /// `cache_ttl_seconds`, default 300).
    #[serde(default = "default_cache_ttl")]
    pub cache_ttl_seconds: u64,
    /// Auto-install the pinned `bws` binary into `<home>/bin` on first
    /// use (hermes `auto_install`).
    #[serde(default = "default_true")]
    pub auto_install: bool,
}

fn default_token_env() -> String {
    "BWS_ACCESS_TOKEN".to_string()
}

fn default_cache_ttl() -> u64 {
    300
}

impl Default for BitwardenSourceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            access_token_env: default_token_env(),
            project_id: String::new(),
            server_url: String::new(),
            override_existing: false,
            cache_ttl_seconds: default_cache_ttl(),
            auto_install: true,
        }
    }
}

/// `[secrets.onepassword]` — 1Password `op` CLI mapped source (hermes
/// `onepassword.py`): env-var names bound to `op://vault/item/field`
/// references, each resolved via `op read -- <ref>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnePasswordSourceConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Env-var name → `op://…` reference bindings.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Env var holding the service-account token (always exported to the
    /// child as `OP_SERVICE_ACCOUNT_TOKEN`, which `op` itself reads).
    #[serde(default = "default_op_token_env")]
    pub service_account_token_env: String,
    /// Absolute path to the `op` binary (empty = resolve via PATH).
    #[serde(default)]
    pub binary_path: String,
    /// `op` account shorthand passed as `--account` (optional).
    #[serde(default)]
    pub account: String,
    /// Per-reference `op read` timeout (hermes `_OP_RUN_TIMEOUT`).
    #[serde(default = "default_op_timeout")]
    pub timeout_seconds: u64,
    /// Fetch-cache TTL in seconds; 0 disables BOTH cache layers (hermes
    /// `cache_ttl_seconds`, default 300).
    #[serde(default = "default_cache_ttl")]
    pub cache_ttl_seconds: u64,
    /// Mapped bindings are explicit, so they beat pre-existing `.env`/shell
    /// values by default (hermes `override_existing=True`); never another
    /// secret source.
    #[serde(default = "default_true")]
    pub override_existing: bool,
}

fn default_op_token_env() -> String {
    "OP_SERVICE_ACCOUNT_TOKEN".to_string()
}

fn default_op_timeout() -> u64 {
    30
}

fn default_true() -> bool {
    true
}

impl Default for OnePasswordSourceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            env: HashMap::new(),
            service_account_token_env: default_op_token_env(),
            binary_path: String::new(),
            account: String::new(),
            timeout_seconds: default_op_timeout(),
            cache_ttl_seconds: default_cache_ttl(),
            override_existing: true,
        }
    }
}

/// One source fetch outcome (hermes `FetchResult`, flattened).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FetchResult {
    pub ok: bool,
    pub secrets: BTreeMap<String, String>,
    pub error: Option<String>,
    pub warnings: Vec<String>,
}

/// Apply-phase bookkeeping (hermes `ApplyReport`, flattened).
#[derive(Debug, Clone, Default)]
pub struct ApplyReport {
    /// (var, source name) pairs actually applied.
    pub applied: Vec<(String, String)>,
    pub skipped_existing: Vec<String>,
    pub skipped_protected: Vec<String>,
    /// Conflict descriptions: later source also supplied a claimed var.
    pub conflicts: Vec<String>,
    /// Per-source fetch errors ("source: message").
    pub errors: Vec<String>,
}

impl ApplyReport {
    pub fn applied_any(&self) -> bool {
        !self.applied.is_empty()
    }
}

/// Valid env var name: `[A-Za-z_][A-Za-z0-9_]*` (hermes
/// `is_valid_env_name`).
pub fn is_valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Parse a KEY=VALUE env-file shaped payload (the `command` source
/// output contract). Comments/blank lines ignored; malformed lines are
/// skipped with a warning entry.
pub fn parse_env_output(raw: &str, warnings: &mut Vec<String>) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            warnings.push(format!("skipped malformed line: {line}"));
            continue;
        };
        let key = key.trim();
        if !is_valid_env_name(key) {
            warnings.push(format!("skipped invalid env name: {key}"));
            continue;
        }
        let value = value.trim();
        // Strip one layer of matching quotes (env-file convention).
        let value = if (value.starts_with('"') && value.ends_with('"') && value.len() >= 2)
            || (value.starts_with('\'') && value.ends_with('\'') && value.len() >= 2)
        {
            &value[1..value.len() - 1]
        } else {
            value
        };
        out.insert(key.to_string(), value.to_string());
    }
    out
}

/// Fetch via the user-configured helper command (hermes `command.py`).
/// Security model: the key is passed to the child ONLY via the
/// `ULNCLAW_SECRET_KEY` env var — never interpolated into the shell
/// string; stderr is discarded (it can carry secret material); timeout
/// + 1 MiB output cap; every failure degrades to "no value".
pub fn fetch_command_source(cfg: &CommandSourceConfig) -> FetchResult {
    let mut result = FetchResult::default();
    if !cfg.enabled || cfg.command.trim().is_empty() {
        return result;
    }
    if cfg!(windows) {
        result.error = Some("command source is POSIX-only (needs /bin/sh)".to_string());
        return result;
    }
    let child = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(&cfg.command)
        .env("ULNCLAW_SECRET_KEY", "")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null()) // never surface helper stderr
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(e) => {
            result.error = Some(format!("spawn failed: {e}"));
            return result;
        }
    };
    let timeout = Duration::from_secs(cfg.timeout_seconds.max(1));
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    // Structured fields only — never the command string.
                    result.error = Some(format!("helper exited with {status}"));
                    return result;
                }
                break;
            }
            Ok(None) => {
                if start.elapsed() > timeout {
                    child.kill().ok();
                    child.wait().ok();
                    result.error = Some(format!(
                        "helper timed out after {}s",
                        cfg.timeout_seconds.max(1)
                    ));
                    return result;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                result.error = Some(format!("wait failed: {e}"));
                return result;
            }
        }
    }
    const MAX_OUTPUT: usize = 1024 * 1024;
    let stdout = child
        .wait_with_output()
        .map(|o| o.stdout)
        .unwrap_or_default();
    if stdout.len() > MAX_OUTPUT {
        result.error = Some("helper output exceeded 1 MiB cap".to_string());
        return result;
    }
    let text = String::from_utf8_lossy(&stdout);
    result.secrets = parse_env_output(&text, &mut result.warnings);
    result.ok = true;
    result
}

/// Locate the `bws` binary (hermes `find_bws`):
/// 1. `<home>/bin/bws` (the managed copy — preferred)
/// 2. `bws` on the system PATH
///
/// `install_if_missing` downloads and verifies the pinned version when
/// neither resolves (auto-install failures degrade to None, never block).
pub fn find_bws(home: &Path) -> Option<PathBuf> {
    find_bws_maybe_install(home, false)
}

/// `find_bws` with the hermes `install_if_missing` switch.
pub fn find_bws_maybe_install(home: &Path, install_if_missing: bool) -> Option<PathBuf> {
    let managed = home.join("bin").join("bws");
    if managed.is_file() {
        return Some(managed);
    }
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in path_var.split(':') {
            let candidate = Path::new(dir).join("bws");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    if install_if_missing {
        match install_bws(home, false) {
            Ok(path) => return Some(path),
            Err(e) => eprintln!("[secrets] bws auto-install failed: {e}"),
        }
    }
    None
}

// ---------------------------------------------------------------------------
// bws auto-install (hermes install_bws)
// ---------------------------------------------------------------------------

/// Pinned upstream version. Bump deliberately — never auto-resolve
/// "latest" because upstream release shape (asset names, CLI flags) is
/// allowed to change between majors (hermes `_BWS_VERSION`).
pub const BWS_VERSION: &str = "2.0.0";

const BWS_RELEASE_BASE: &str = "https://github.com/bitwarden/sdk-sm/releases/download/bws-v2.0.0";
const BWS_CHECKSUM_NAME: &str = "bws-sha256-checksums-2.0.0.txt";
/// Whole-request timeout. Hermes uses 60s; release CDN transfers of the
/// ~6 MB zip routinely exceed that on slow links, so ulnclaw allows 5 min.
const BWS_DOWNLOAD_TIMEOUT_SECS: u64 = 300;

/// Map (os, arch, libc) → the upstream asset filename (hermes
/// `_platform_asset_name`; Linux defaults to gnu and switches to musl
/// only when `ldd --version` says so).
pub fn bws_platform_asset_name() -> Result<String, String> {
    let arch = if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else {
        return Err(format!(
            "unsupported architecture for bws auto-install: {}",
            std::env::consts::ARCH
        ));
    };
    if cfg!(target_os = "macos") {
        // Universal binary works on both Intel and Apple Silicon.
        return Ok(format!("bws-macos-universal-{BWS_VERSION}.zip"));
    }
    if cfg!(target_os = "windows") {
        return Ok(format!("bws-{arch}-pc-windows-msvc-{BWS_VERSION}.zip"));
    }
    if cfg!(target_os = "linux") {
        let libc = if ldd_is_musl() { "musl" } else { "gnu" };
        return Ok(format!("bws-{arch}-unknown-linux-{libc}-{BWS_VERSION}.zip"));
    }
    Err(format!(
        "unsupported platform for bws auto-install: {}",
        std::env::consts::OS
    ))
}

fn ldd_is_musl() -> bool {
    // ldd --version writes to stderr on glibc, stdout on musl. Getting it
    // wrong falls back to a clear error from the binary loader (hermes).
    let output = std::process::Command::new("ldd")
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .output();
    match output {
        Ok(out) => {
            let combined = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            combined.to_ascii_lowercase().contains("musl")
        }
        Err(_) => false,
    }
}

/// Download, verify, and install the pinned `bws` binary (hermes
/// `install_bws`). Returns the installed path; errors propagate so the
/// setup wizard can show them (auto-install callers catch and degrade).
pub fn install_bws(home: &Path, force: bool) -> Result<PathBuf, String> {
    let bin_dir = home.join("bin");
    std::fs::create_dir_all(&bin_dir)
        .map_err(|e| format!("cannot create {}: {e}", bin_dir.display()))?;
    let target = bin_dir.join("bws");
    if target.is_file() && !force {
        return Ok(target);
    }

    let asset_name = bws_platform_asset_name()?;
    let asset_url = format!("{BWS_RELEASE_BASE}/{asset_name}");
    let checksum_url = format!("{BWS_RELEASE_BASE}/{BWS_CHECKSUM_NAME}");

    eprintln!("[secrets] downloading {asset_url}");
    // reqwest::blocking spins up its own tokio runtime, which cannot be
    // dropped inside ours — run the download on a dedicated thread.
    let (zip_bytes, checksum_text) = download_bws_assets(&asset_url, &checksum_url)?;

    let expected = expected_sha256(&checksum_text, &asset_name)?;
    let actual = sha256_hex(&zip_bytes);
    if !expected.eq_ignore_ascii_case(&actual) {
        return Err(format!(
            "checksum mismatch for {asset_name}: expected {expected}, got {actual}"
        ));
    }

    let extracted = extract_bws_from_zip(&zip_bytes, "bws")?;

    // Move into place atomically: stage in the final directory so the
    // rename can't cross filesystems (hermes staged install).
    let staged = bin_dir.join(format!(".bws_staging_{}", std::process::id()));
    std::fs::write(&staged, &extracted).map_err(|e| format!("stage binary: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755));
    }
    std::fs::rename(&staged, &target).map_err(|e| {
        let _ = std::fs::remove_file(&staged);
        format!("install binary: {e}")
    })?;
    eprintln!(
        "[secrets] installed bws {BWS_VERSION} at {}",
        target.display()
    );
    Ok(target)
}

fn download_bws_assets(asset_url: &str, checksum_url: &str) -> Result<(Vec<u8>, String), String> {
    let asset_url = asset_url.to_string();
    let checksum_url = checksum_url.to_string();
    // reqwest::blocking spins up its own tokio runtime, which cannot be
    // dropped inside ours — run the download on a dedicated thread.
    // Transient connection resets are common on release CDNs, so retry.
    let handle = std::thread::spawn(move || -> Result<(Vec<u8>, String), String> {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(BWS_DOWNLOAD_TIMEOUT_SECS))
            .build()
            .map_err(|e| format!("http client: {e}"))?;
        let mut last_error = String::new();
        for attempt in 1..=3 {
            let asset = client
                .get(&asset_url)
                .send()
                .and_then(|r| r.error_for_status())
                .and_then(|r| r.bytes())
                .map(|b| b.to_vec());
            let checksums = client
                .get(&checksum_url)
                .send()
                .and_then(|r| r.error_for_status())
                .and_then(|r| r.text());
            match (asset, checksums) {
                (Ok(zip_bytes), Ok(checksum_text)) => return Ok((zip_bytes, checksum_text)),
                (Err(e), _) | (_, Err(e)) => {
                    last_error = format!("{e}");
                    if attempt < 3 {
                        std::thread::sleep(std::time::Duration::from_secs(2 * attempt));
                    }
                }
            }
        }
        Err(format!("download failed after 3 attempts: {last_error}"))
    });
    match handle.join() {
        Ok(result) => result,
        Err(_) => Err("download thread panicked".to_string()),
    }
}

/// Parse the upstream `bws-sha256-checksums-X.Y.Z.txt` (standard
/// sha256sum output: `<hex>  <filename>`) — hermes `_expected_sha256`.
fn expected_sha256(checksum_text: &str, asset_name: &str) -> Result<String, String> {
    for line in checksum_text.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 && parts[parts.len() - 1] == asset_name {
            return Ok(parts[0].to_string());
        }
    }
    Err(format!("no checksum entry for {asset_name}"))
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Find `binary_name` inside the zip (shortest path wins — root over
/// nested, hermes `_pick_zip_member`) and extract it with a zip-slip
/// guard (hermes `_safe_extract_member`).
fn extract_bws_from_zip(zip_bytes: &[u8], binary_name: &str) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip_bytes))
        .map_err(|e| format!("corrupt archive: {e}"))?;
    let mut candidates: Vec<String> = Vec::new();
    for i in 0..archive.len() {
        let entry = archive
            .by_index_raw(i)
            .map_err(|e| format!("archive entry: {e}"))?;
        let name = entry.name().to_string();
        if name.split('/').last() == Some(binary_name) {
            candidates.push(name);
        }
    }
    if candidates.is_empty() {
        return Err(format!(
            "could not find {binary_name} inside downloaded archive"
        ));
    }
    candidates.sort_by_key(|name| name.len());
    let member = candidates.remove(0);
    // Zip-slip guard: member names must stay inside the extraction root.
    let normalized = member.trim_start_matches('/');
    if normalized.split('/').any(|seg| seg == "..") {
        return Err(format!(
            "refusing to extract unsafe archive member {member:?}"
        ));
    }
    let mut file = archive
        .by_name(&member)
        .map_err(|e| format!("archive member: {e}"))?;
    let mut out = Vec::new();
    file.read_to_end(&mut out)
        .map_err(|e| format!("read archive member: {e}"))?;
    if out.is_empty() {
        return Err(format!("archive member {member:?} is empty"));
    }
    Ok(out)
}

/// Fetch via Bitwarden Secrets Manager: `bws secret list <project>
/// --output json` with the bootstrap token in the subprocess env
/// (hermes `_run_bws_list`). The token itself is never returned.
pub fn fetch_bitwarden_source(cfg: &BitwardenSourceConfig, home: &Path) -> FetchResult {
    let mut result = FetchResult::default();
    if !cfg.enabled {
        return result;
    }
    let token = std::env::var(&cfg.access_token_env).unwrap_or_default();
    if token.trim().is_empty() {
        result.error = Some(format!("{} is not set", cfg.access_token_env));
        return result;
    }
    if cfg.project_id.trim().is_empty() {
        result.error = Some("no project_id configured".to_string());
        return result;
    }
    // Encrypted disk cache (hermes): back-to-back CLI invocations don't
    // each pay the bws round-trip. TTL 0 disables both cache layers.
    let cache_key = format!(
        "{}|{}|{}",
        crate::secrets_cache::token_fingerprint(token.trim()),
        cfg.project_id.trim(),
        cfg.server_url.trim()
    );
    let ttl = cfg.cache_ttl_seconds as f64;
    if ttl > 0.0 {
        if let Some(cached) =
            crate::secrets_cache::read_encrypted_bws_cache(home, &cache_key, token.trim(), ttl)
        {
            result.secrets = cached.secrets;
            result.ok = true;
            return result;
        }
    }
    let Some(bws) = find_bws_maybe_install(home, cfg.auto_install) else {
        result.error = Some(
            "bws binary not found (install Bitwarden Secrets Manager CLI, place it in <home>/bin, or enable auto_install)"
                .to_string(),
        );
        return result;
    };
    let mut cmd = std::process::Command::new(&bws);
    cmd.arg("secret")
        .arg("list")
        .arg(&cfg.project_id)
        .arg("--output")
        .arg("json")
        .env(&cfg.access_token_env, token.trim())
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    if !cfg.server_url.trim().is_empty() {
        cmd.env("BWS_SERVER_URL", cfg.server_url.trim());
    }
    let output = match cmd.output() {
        Ok(output) => output,
        Err(e) => {
            result.error = Some(format!("bws spawn failed: {e}"));
            return result;
        }
    };
    if !output.status.success() {
        // bws error text (auth failure etc.) — surface the exit code and
        // the first stderr line only.
        let stderr = String::from_utf8_lossy(&output.stderr);
        let first = stderr.lines().next().unwrap_or("").trim();
        result.error = Some(format!(
            "bws exited with {}{}",
            output.status,
            if first.is_empty() {
                String::new()
            } else {
                format!(": {first}")
            }
        ));
        return result;
    }
    let parsed: serde_json::Result<Vec<serde_json::Value>> = serde_json::from_slice(&output.stdout);
    match parsed {
        Ok(items) => {
            for item in items {
                let key = item.get("key").and_then(|v| v.as_str()).unwrap_or("");
                let value = item.get("value").and_then(|v| v.as_str()).unwrap_or("");
                if key.is_empty() {
                    continue;
                }
                if key == cfg.access_token_env {
                    // Never echo the bootstrap token back into the env.
                    continue;
                }
                if !is_valid_env_name(key) {
                    result
                        .warnings
                        .push(format!("skipped invalid env name from vault: {key}"));
                    continue;
                }
                result.secrets.insert(key.to_string(), value.to_string());
            }
            result.ok = true;
            // Persist the successful pull for the next process (hermes
            // encrypted cache write).
            if ttl > 0.0 {
                let fetched_at = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0);
                crate::secrets_cache::write_encrypted_bws_cache(
                    home,
                    &cache_key,
                    token.trim(),
                    &crate::secrets_cache::CachedFetch {
                        secrets: result.secrets.clone(),
                        fetched_at,
                    },
                );
            }
        }
        Err(e) => {
            result.error = Some(format!("unparseable bws output: {e}"));
        }
    }
    result
}

/// Locate the `op` binary: explicit `binary_path` first, then `PATH`
/// (hermes `find_op`, without the managed-install search dirs).
pub fn find_op(binary_path: &str) -> Option<PathBuf> {
    let trimmed = binary_path.trim();
    if !trimmed.is_empty() {
        let candidate = PathBuf::from(trimmed);
        return if candidate.is_file() {
            Some(candidate)
        } else {
            None
        };
    }
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in path_var.split(':') {
            let candidate = Path::new(dir).join("op");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Minimal allowlisted env for the `op` child (hermes `_op_child_env`):
/// never inherit the full process env, which post-startup holds every
/// resolved credential.
fn op_child_env(token_value: &str) -> Vec<(String, String)> {
    const ALLOWLIST: &[&str] = &[
        "PATH",
        "HOME",
        "USERPROFILE",
        "APPDATA",
        "LOCALAPPDATA",
        "SystemRoot",
        "TMPDIR",
        "TMP",
        "TEMP",
        "XDG_CONFIG_HOME",
        "XDG_RUNTIME_DIR",
        "OP_ACCOUNT",
        "OP_CONNECT_HOST",
        "OP_CONNECT_TOKEN",
        "OP_LOAD_DESKTOP_APP_SETTINGS",
    ];
    let mut env: Vec<(String, String)> = Vec::new();
    for key in ALLOWLIST {
        if let Ok(val) = std::env::var(key) {
            env.push((key.to_string(), val));
        }
    }
    for (key, val) in std::env::vars() {
        if key.starts_with("OP_SESSION_") {
            env.push((key, val));
        }
    }
    if !token_value.is_empty() {
        env.push((
            "OP_SERVICE_ACCOUNT_TOKEN".to_string(),
            token_value.to_string(),
        ));
    }
    env.push(("NO_COLOR".to_string(), "1".to_string()));
    env
}

/// Resolve one `op://` reference via `op read -- <ref>` (hermes
/// `_run_op_read`). Empty output is an error: applying it would silently
/// clobber a good credential with nothing.
fn run_op_read(
    op: &Path,
    reference: &str,
    account: &str,
    token_value: &str,
    timeout_seconds: u64,
) -> Result<String, String> {
    let mut cmd = std::process::Command::new(op);
    cmd.arg("read");
    if !account.trim().is_empty() {
        cmd.args(["--account", account.trim()]);
    }
    // `--` terminates option parsing so a reference can never be
    // mis-parsed as an `op` flag.
    cmd.args(["--", reference])
        .env_clear()
        .envs(op_child_env(token_value))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to invoke op: {e}"))?;
    let timeout = Duration::from_secs(timeout_seconds.max(1));
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    let stderr = child
                        .wait_with_output()
                        .map(|o| String::from_utf8_lossy(&o.stderr).trim().to_string())
                        .unwrap_or_default();
                    return Err(if stderr.is_empty() {
                        format!("op read exited with {status} for {reference:?}")
                    } else {
                        let scrubbed: String = stderr.chars().take(200).collect();
                        format!("op read failed for {reference:?}: {scrubbed}")
                    });
                }
                break;
            }
            Ok(None) => {
                if start.elapsed() > timeout {
                    child.kill().ok();
                    child.wait().ok();
                    return Err(format!(
                        "op read timed out after {}s for {reference:?}",
                        timeout_seconds.max(1)
                    ));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(format!("failed to invoke op: {e}")),
        }
    }
    let stdout = child
        .wait_with_output()
        .map(|o| o.stdout)
        .unwrap_or_default();
    let value = String::from_utf8_lossy(&stdout)
        .trim_end_matches(['\r', '\n'])
        .to_string();
    if value.trim().is_empty() {
        return Err(format!("op read returned an empty value for {reference:?}"));
    }
    Ok(value)
}

/// Fetch via the 1Password `op` CLI (hermes `fetch_onepassword_secrets`,
/// minus the TTL caches). A missing `op` binary fails the whole source;
/// per-reference failures are warnings so one bad binding never sinks the
/// rest.
pub fn fetch_onepassword_source(cfg: &OnePasswordSourceConfig) -> FetchResult {
    let mut result = FetchResult::default();
    if !cfg.enabled || cfg.env.is_empty() {
        return result;
    }
    // Validate bindings (hermes `_validate_references`).
    let mut valid: BTreeMap<String, String> = BTreeMap::new();
    let mut names: Vec<&String> = cfg.env.keys().collect();
    names.sort();
    for name in names {
        let reference = &cfg.env[name];
        if !is_valid_env_name(name) {
            result.warnings.push(format!(
                "onepassword: skipping {name:?}: not a valid env-var name"
            ));
            continue;
        }
        let cleaned = reference.trim();
        if !cleaned.starts_with("op://") {
            result.warnings.push(format!(
                "onepassword: skipping {name:?}: {reference:?} is not an op:// secret reference"
            ));
            continue;
        }
        if name == &cfg.service_account_token_env {
            // Never let a resolved secret clobber the token used to auth.
            result.warnings.push(format!(
                "onepassword: skipping {name:?}: bound to the bootstrap token var"
            ));
            continue;
        }
        valid.insert(name.clone(), cleaned.to_string());
    }
    if valid.is_empty() {
        result.ok = true;
        return result;
    }
    let home = crate::config::ulnclaw_home();
    let token_value = std::env::var(&cfg.service_account_token_env)
        .unwrap_or_default()
        .trim()
        .to_string();
    // Cache key (hermes): token fingerprint | account | refs fingerprint
    // (the disk file is already partitioned by home).
    let refs_canonical: String = valid
        .iter()
        .map(|(name, reference)| format!("{name}={reference}"))
        .collect::<Vec<_>>()
        .join("\n");
    let cache_key = format!(
        "{}|{}|{}",
        crate::secrets_cache::token_fingerprint(&token_value),
        cfg.account.trim(),
        crate::secrets_cache::token_fingerprint(&refs_canonical)
    );
    let ttl = cfg.cache_ttl_seconds as f64;
    if ttl > 0.0 {
        if let Some(cached) = crate::secrets_cache::OP_DISK_CACHE.read(&cache_key, ttl, &home) {
            result.secrets = cached.secrets;
            result.ok = true;
            return result;
        }
    }
    let op = match find_op(&cfg.binary_path) {
        Some(op) => op,
        None => {
            result.error = Some(
                "op CLI not found. Install the 1Password CLI or set                  secrets.onepassword.binary_path to its absolute location"
                    .to_string(),
            );
            return result;
        }
    };
    let mut read_errors = 0usize;
    for (name, reference) in &valid {
        match run_op_read(
            &op,
            reference,
            &cfg.account,
            &token_value,
            cfg.timeout_seconds,
        ) {
            Ok(value) => {
                result.secrets.insert(name.clone(), value);
            }
            Err(err) => {
                result.warnings.push(format!("onepassword: {err}"));
                read_errors += 1;
            }
        }
    }
    result.ok = true;
    // Only a complete, error-free pull is cached, so a transient auth
    // failure isn't frozen in for the whole TTL window (hermes).
    if ttl > 0.0 && read_errors == 0 && !result.secrets.is_empty() {
        let fetched_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        crate::secrets_cache::OP_DISK_CACHE.write(
            &cache_key,
            &crate::secrets_cache::CachedFetch {
                secrets: result.secrets.clone(),
                fetched_at,
            },
            ttl,
            &home,
        );
    }
    result
}

/// Names a source protects from any write (hermes
/// `protected_env_vars`): bitwarden guards its bootstrap token var, and
/// 1Password guards its service-account token var.
fn protected_vars(cfg: &SecretsConfig) -> HashSet<String> {
    let mut protected = HashSet::new();
    if cfg.bitwarden.enabled {
        protected.insert(cfg.bitwarden.access_token_env.clone());
    }
    if cfg.onepassword.enabled {
        protected.insert(cfg.onepassword.service_account_token_env.clone());
    }
    protected
}

/// Ordered enabled sources (hermes `_ordered_enabled_sources`): mapped
/// sources (onepassword) outrank bulk sources regardless of list order;
/// within the bulk shape the `sources` list order wins (default
/// command → bitwarden).
pub fn ordered_sources(cfg: &SecretsConfig) -> Vec<String> {
    let known = ["command", "bitwarden", "onepassword"];
    let order: Vec<String> = if cfg.sources.is_empty() {
        known.iter().map(|s| s.to_string()).collect()
    } else {
        cfg.sources
            .iter()
            .filter(|s| known.contains(&s.as_str()))
            .cloned()
            .collect()
    };
    let enabled: Vec<String> = order
        .into_iter()
        .filter(|name| match name.as_str() {
            "command" => cfg.command.enabled,
            "bitwarden" => cfg.bitwarden.enabled,
            "onepassword" => cfg.onepassword.enabled,
            _ => false,
        })
        .collect();
    let mut mapped: Vec<String> = enabled
        .iter()
        .filter(|n| n.as_str() == "onepassword")
        .cloned()
        .collect();
    let bulk: Vec<String> = enabled
        .into_iter()
        .filter(|n| n.as_str() != "onepassword")
        .collect();
    mapped.extend(bulk);
    mapped
}

/// Precedence engine (hermes `apply_all` apply phase, minus profile
/// aliasing). `env` starts as process-env ∪ .env; the caller writes the
/// diff back. First claim wins; preserve beats everything;
/// override_existing beats pre-existing env but never another source.
pub fn apply_to_env(
    env: &mut HashMap<String, String>,
    cfg: &SecretsConfig,
    fetches: &[(String, FetchResult)],
) -> ApplyReport {
    let mut report = ApplyReport::default();
    let preserve: HashSet<String> = cfg
        .preserve_existing
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let protected = protected_vars(cfg);
    let mut claimed: HashMap<String, String> = HashMap::new();

    for (source, result) in fetches {
        if !result.ok {
            if let Some(err) = &result.error {
                report.errors.push(format!("{source}: {err}"));
            }
            continue;
        }
        let override_existing = (source == "bitwarden" && cfg.bitwarden.override_existing)
            || (source == "onepassword" && cfg.onepassword.override_existing);
        for (var, value) in &result.secrets {
            if !is_valid_env_name(var) {
                continue;
            }
            if protected.contains(var) {
                report.skipped_protected.push(var.clone());
                continue;
            }
            if let Some(winner) = claimed.get(var) {
                report.conflicts.push(format!(
                    "{var}: kept value from {winner}; {source} also supplies it \
                     (first source wins — remove one binding or reorder secrets.sources)"
                ));
                continue;
            }
            let existed = env.contains_key(var);
            if existed && preserve.contains(var) {
                report.skipped_existing.push(var.clone());
                continue;
            }
            if existed && !override_existing {
                report.skipped_existing.push(var.clone());
                continue;
            }
            env.insert(var.clone(), value.clone());
            claimed.insert(var.clone(), source.clone());
            report.applied.push((var.clone(), source.clone()));
        }
    }
    report
}

/// Startup orchestrator: fetch every enabled source, merge with
/// process-env ∪ `.env`, and export the winners into the process
/// environment. Never fails — errors are collected for the caller's
/// one-line warnings (hermes env_loader behavior).
pub fn fetch_all(cfg: &SecretsConfig, home: &Path) -> Vec<(String, FetchResult)> {
    ordered_sources(cfg)
        .into_iter()
        .map(|name| {
            let result = match name.as_str() {
                "command" => fetch_command_source(&cfg.command),
                "bitwarden" => fetch_bitwarden_source(&cfg.bitwarden, home),
                "onepassword" => fetch_onepassword_source(&cfg.onepassword),
                _ => FetchResult::default(),
            };
            (name, result)
        })
        .collect()
}

/// Apply secrets to the process environment (startup hook). Returns the
/// report so callers can warn about fetch errors without blocking.
pub fn apply_all(cfg: &SecretsConfig, home: &Path) -> ApplyReport {
    let fetches = fetch_all(cfg, home);
    if fetches.is_empty() {
        return ApplyReport::default();
    }
    // Base view: current process env plus anything from .env (process
    // env already wins because it is inserted second).
    let mut env: HashMap<String, String> = HashMap::new();
    let env_file = home.join(".env");
    if env_file.is_file() {
        env.extend(crate::config::load_env_file(&env_file));
    }
    for (key, value) in std::env::vars() {
        env.insert(key, value);
    }
    let report = apply_to_env(&mut env, cfg, &fetches);
    for (var, _source) in &report.applied {
        if let Some(value) = env.get(var) {
            // SAFETY: single-threaded startup hook, before any spawn.
            std::env::set_var(var, value);
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_name_validation() {
        assert!(is_valid_env_name("OPENAI_API_KEY"));
        assert!(is_valid_env_name("_PRIVATE"));
        assert!(!is_valid_env_name("9KEY"));
        assert!(!is_valid_env_name("BAD-NAME"));
        assert!(!is_valid_env_name(""));
        assert!(!is_valid_env_name("INJECTION; rm"));
    }

    #[test]
    fn parse_env_output_handles_quotes_comments_and_garbage() {
        let mut warnings = Vec::new();
        let parsed = parse_env_output(
            "# comment\n\nAPI_KEY=abc123\nQUOTED=\"hello world\"\nSINGLE='x'\n  SPACED = v \nnot a line\n1BAD=x\n",
            &mut warnings,
        );
        assert_eq!(parsed.get("API_KEY").unwrap(), "abc123");
        assert_eq!(parsed.get("QUOTED").unwrap(), "hello world");
        assert_eq!(parsed.get("SINGLE").unwrap(), "x");
        assert_eq!(parsed.get("SPACED").unwrap(), "v");
        assert!(!parsed.contains_key("1BAD"));
        assert_eq!(warnings.len(), 2, "garbage + invalid name: {warnings:?}");
    }

    fn fetch_ok(pairs: &[(&str, &str)]) -> FetchResult {
        FetchResult {
            ok: true,
            secrets: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            error: None,
            warnings: vec![],
        }
    }

    #[test]
    fn precedence_first_wins_preserve_and_override() {
        let cfg = SecretsConfig {
            sources: vec!["command".into(), "bitwarden".into()],
            preserve_existing: vec!["KEEP_ME".into()],
            command: CommandSourceConfig {
                enabled: true,
                command: "true".into(),
                timeout_seconds: 3,
            },
            bitwarden: BitwardenSourceConfig {
                enabled: true,
                override_existing: true,
                access_token_env: "BWS_ACCESS_TOKEN".into(),
                ..Default::default()
            },
            onepassword: OnePasswordSourceConfig::default(),
        };
        let mut env = HashMap::new();
        env.insert("EXISTING".to_string(), "old".to_string());
        env.insert("KEEP_ME".to_string(), "precious".to_string());
        env.insert("BWS_ACCESS_TOKEN".to_string(), "tok".to_string());

        let fetches = vec![
            (
                "command".to_string(),
                fetch_ok(&[("NEW_VAR", "from-command"), ("EXISTING", "command-tries")]),
            ),
            (
                "bitwarden".to_string(),
                fetch_ok(&[
                    ("NEW_VAR", "from-bitwarden"),  // conflict: command claimed it
                    ("OVERRIDE_ME", "bw-value"),    // existed, override=true → applied
                    ("KEEP_ME", "bw-wants"),        // preserve → skipped
                    ("BWS_ACCESS_TOKEN", "leaked"), // protected → skipped
                    ("FRESH", "bw-fresh"),
                ]),
            ),
        ];
        let report = apply_to_env(&mut env, &cfg, &fetches);

        assert_eq!(
            env.get("NEW_VAR").unwrap(),
            "from-command",
            "first claim wins"
        );
        assert_eq!(
            env.get("EXISTING").unwrap(),
            "old",
            "no override on command source"
        );
        assert_eq!(
            env.get("OVERRIDE_ME").unwrap(),
            "bw-value",
            "override_existing applied"
        );
        assert_eq!(
            env.get("KEEP_ME").unwrap(),
            "precious",
            "preserve_existing held"
        );
        assert_eq!(
            env.get("BWS_ACCESS_TOKEN").unwrap(),
            "tok",
            "bootstrap token protected"
        );
        assert_eq!(env.get("FRESH").unwrap(), "bw-fresh");

        assert_eq!(report.applied.len(), 3); // NEW_VAR, OVERRIDE_ME, FRESH
        assert_eq!(report.conflicts.len(), 1);
        assert!(report
            .skipped_protected
            .contains(&"BWS_ACCESS_TOKEN".to_string()));
        assert!(report.skipped_existing.contains(&"EXISTING".to_string()));
        assert!(report.skipped_existing.contains(&"KEEP_ME".to_string()));
    }

    #[test]
    fn failed_source_reported_not_fatal() {
        let cfg = SecretsConfig {
            command: CommandSourceConfig {
                enabled: true,
                command: "true".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        let mut env = HashMap::new();
        let fetches = vec![(
            "command".to_string(),
            FetchResult {
                ok: false,
                error: Some("helper timed out after 3s".into()),
                ..Default::default()
            },
        )];
        let report = apply_to_env(&mut env, &cfg, &fetches);
        assert!(env.is_empty());
        assert_eq!(report.errors.len(), 1);
        assert!(report.errors[0].contains("command"));
    }

    #[test]
    fn command_source_disabled_or_empty_is_noop() {
        let result = fetch_command_source(&CommandSourceConfig::default());
        assert!(!result.ok && result.secrets.is_empty() && result.error.is_none());
    }

    #[test]
    fn command_source_parses_helper_output() {
        // Real subprocess round-trip through /bin/sh.
        let cfg = CommandSourceConfig {
            enabled: true,
            command: "printf 'HELLO_T=world\\n# c\\nBROKEN\\n'".into(),
            timeout_seconds: 5,
        };
        if cfg!(windows) {
            return;
        }
        let result = fetch_command_source(&cfg);
        assert!(result.ok, "{result:?}");
        assert_eq!(result.secrets.get("HELLO_T").unwrap(), "world");
        assert_eq!(result.warnings.len(), 1);
    }

    #[test]
    fn command_source_timeout_degrades() {
        if cfg!(windows) {
            return;
        }
        let cfg = CommandSourceConfig {
            enabled: true,
            command: "sleep 5".into(),
            timeout_seconds: 1,
        };
        let start = std::time::Instant::now();
        let result = fetch_command_source(&cfg);
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("timed out"));
        assert!(start.elapsed() < std::time::Duration::from_secs(4));
    }

    #[test]
    fn bitwarden_disabled_or_missing_token() {
        let result = fetch_bitwarden_source(&BitwardenSourceConfig::default(), Path::new("/tmp"));
        assert!(!result.ok && result.error.is_none(), "disabled = silent");
        let cfg = BitwardenSourceConfig {
            enabled: true,
            access_token_env: "DEFINITELY_UNSET_ULNCLAW_TEST_VAR_XYZ".into(),
            ..Default::default()
        };
        let result = fetch_bitwarden_source(&cfg, Path::new("/tmp"));
        assert!(result.error.unwrap().contains("is not set"));
    }

    #[test]
    fn ordering_respects_sources_list_and_enabled() {
        let cfg = SecretsConfig {
            sources: vec!["bitwarden".into(), "command".into()],
            command: CommandSourceConfig {
                enabled: true,
                command: "x".into(),
                ..Default::default()
            },
            bitwarden: BitwardenSourceConfig::default(), // disabled
            ..Default::default()
        };
        assert_eq!(ordered_sources(&cfg), vec!["command".to_string()]);
        let cfg = SecretsConfig::default();
        assert_eq!(
            ordered_sources(&cfg),
            Vec::<String>::new(),
            "nothing enabled -> nothing ordered"
        );
    }

    #[test]
    fn onepassword_disabled_or_empty_is_noop() {
        let result = fetch_onepassword_source(&OnePasswordSourceConfig::default());
        assert!(!result.ok && result.error.is_none(), "disabled = silent");
        let cfg = OnePasswordSourceConfig {
            enabled: true,
            ..Default::default()
        };
        let result = fetch_onepassword_source(&cfg);
        assert!(!result.ok && result.error.is_none(), "no bindings = silent");
    }

    #[test]
    fn onepassword_reference_validation() {
        let mut env = HashMap::new();
        env.insert("GOOD_KEY".to_string(), "op://vault/item/field".to_string());
        env.insert("BAD-NAME".to_string(), "op://vault/item/x".to_string());
        env.insert("NOT_A_REF".to_string(), "plaintext".to_string());
        env.insert(
            "OP_SERVICE_ACCOUNT_TOKEN".to_string(),
            "op://vault/item/token".to_string(),
        );
        let cfg = OnePasswordSourceConfig {
            enabled: true,
            env,
            ..Default::default()
        };
        // No op binary configured: binary_path empty falls through to a
        // PATH search, so force a deterministic miss instead.
        let cfg = OnePasswordSourceConfig {
            binary_path: "/nonexistent-ulnclaw-test/op".into(),
            ..cfg
        };
        let result = fetch_onepassword_source(&cfg);
        // Only GOOD_KEY survives validation, so the source reaches the
        // binary lookup and fails there; the three drops are warnings.
        assert!(result.error.unwrap().contains("op CLI not found"));
        assert_eq!(result.warnings.len(), 3, "{:?}", result.warnings);
        assert!(result
            .warnings
            .iter()
            .any(|w| w.contains("not a valid env-var name")));
        assert!(result
            .warnings
            .iter()
            .any(|w| w.contains("not an op:// secret reference")));
        assert!(result
            .warnings
            .iter()
            .any(|w| w.contains("bootstrap token var")));
    }

    #[test]
    fn onepassword_missing_op_binary_errors() {
        let mut env = HashMap::new();
        env.insert("SOME_KEY".to_string(), "op://vault/item/field".to_string());
        let cfg = OnePasswordSourceConfig {
            enabled: true,
            env,
            binary_path: "/nonexistent-ulnclaw-test/op".into(),
            ..Default::default()
        };
        let result = fetch_onepassword_source(&cfg);
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("op CLI not found"));
    }

    #[test]
    fn onepassword_resolves_references_via_op_read() {
        if cfg!(windows) {
            return;
        }
        let dir = std::env::temp_dir().join(format!("ulnclaw-op-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let op = dir.join("op");
        std::fs::write(
            &op,
            "#!/bin/sh\n\
             while [ \"$1\" != \"--\" ]; do shift; done\n\
             shift\n\
             case \"$1\" in\n\
               \"op://vault/item/field\") printf 's3cret\\n' ;;\n\
               *) echo \"no such item\" >&2; exit 1 ;;\n\
             esac\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&op, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut env = HashMap::new();
        env.insert(
            "TEST_OP_KEY".to_string(),
            "op://vault/item/field".to_string(),
        );
        env.insert(
            "TEST_OP_BAD".to_string(),
            "op://vault/item/missing".to_string(),
        );
        let cfg = OnePasswordSourceConfig {
            enabled: true,
            env,
            binary_path: op.display().to_string(),
            timeout_seconds: 5,
            ..Default::default()
        };
        let result = fetch_onepassword_source(&cfg);
        assert!(result.ok, "{:?}", result.error);
        assert_eq!(
            result.secrets.get("TEST_OP_KEY").map(|s| s.as_str()),
            Some("s3cret")
        );
        assert!(!result.secrets.contains_key("TEST_OP_BAD"));
        assert!(result
            .warnings
            .iter()
            .any(|w| w.contains("TEST_OP_BAD") || w.contains("op://vault/item/missing")));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn onepassword_mapped_outranks_bulk() {
        let cfg = SecretsConfig {
            sources: vec!["command".into(), "bitwarden".into(), "onepassword".into()],
            command: CommandSourceConfig {
                enabled: true,
                command: "x".into(),
                ..Default::default()
            },
            bitwarden: BitwardenSourceConfig {
                enabled: true,
                access_token_env: "T".into(),
                ..Default::default()
            },
            onepassword: OnePasswordSourceConfig {
                enabled: true,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            ordered_sources(&cfg),
            vec![
                "onepassword".to_string(),
                "command".to_string(),
                "bitwarden".to_string()
            ]
        );
    }

    #[test]
    fn onepassword_override_and_token_protection() {
        let mut env_map = HashMap::new();
        env_map.insert("EXISTING_VAR".to_string(), "old".to_string());
        let cfg = SecretsConfig {
            onepassword: OnePasswordSourceConfig {
                enabled: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut secrets = BTreeMap::new();
        secrets.insert("EXISTING_VAR".to_string(), "new".to_string());
        secrets.insert("OP_SERVICE_ACCOUNT_TOKEN".to_string(), "evil".to_string());
        let fetches = vec![(
            "onepassword".to_string(),
            FetchResult {
                ok: true,
                secrets,
                ..Default::default()
            },
        )];
        let report = apply_to_env(&mut env_map, &cfg, &fetches);
        assert_eq!(
            env_map.get("EXISTING_VAR").map(|s| s.as_str()),
            Some("new"),
            "mapped source overrides pre-existing env by default"
        );
        assert_eq!(
            env_map.get("OP_SERVICE_ACCOUNT_TOKEN").map(|s| s.as_str()),
            None,
            "bootstrap token var is protected"
        );
        assert!(report
            .skipped_protected
            .iter()
            .any(|v| v == "OP_SERVICE_ACCOUNT_TOKEN"));
    }
}

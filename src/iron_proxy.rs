//! iron-proxy egress management — port of hermes
//! `agent/proxy_sources/iron_proxy.py` @ v2026.8.3 (CLI surface:
//! `src/egress_cmd.rs`, hermes `hermes_cli/proxy_cli.py`).
//!
//! Routes outbound sandbox traffic through a local TLS-intercepting
//! proxy (github.com/ironsh/iron-proxy, Apache-2.0) so prompt-injected
//! agents never see real provider API keys: the sandbox receives
//! minted per-provider proxy tokens, iron-proxy swaps them for the
//! real upstream credentials (read from the daemon's OWN environment)
//! on allowlisted hosts, and rejects everything else.
//!
//! Lifecycle parity: `install` downloads the pinned v0.39.0 release
//! (SHA-256 checksums enforced + best-effort GPG detached-signature
//! verification via the system `gpg`), setup generates an openssl CA,
//! mints tokens for every known provider in env/Bitwarden and writes
//! `proxy.yaml` + `mappings.json`, `start` spawns the daemon detached
//! with a minimal allowlisted env plus a per-start nonce (PID-recycling
//! defense via /proc environ / cmdline / starttime), `reload` hot-applies
//! ruleset changes through the loopback management API, `stop` goes
//! SIGTERM→SIGKILL with starttime re-verification.
//!
//! Divergences: internal env vars are branded `ULNCLAW_IRON_PROXY_*`
//! (hermes: `HERMES_IRON_PROXY_*`); archive extraction shells out to
//! the system `tar` (member path-traversal still rejected) instead of
//! Python tarfile; sandbox scope is Docker-only, matching hermes.

use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Pinned upstream release (hermes `_IRON_PROXY_VERSION`).
pub const IRON_PROXY_VERSION: &str = "0.39.0";
/// hermes `_IRON_PROXY_RELEASE_BASE`.
pub const IRON_PROXY_RELEASE_BASE: &str =
    "https://github.com/ironsh/iron-proxy/releases/download/v0.39.0";
const CHECKSUM_NAME: &str = "checksums.txt";
/// Detached signature for checksums.txt + the signing public key, both
/// shipped on the release (optional GPG verification of the release
/// channel — SHA-256 only protects the archive if checksums.txt itself
/// came from an uncompromised channel).
const CHECKSUM_SIG_NAME: &str = "checksums.txt.asc";
const PUBKEY_NAME: &str = "public-key.asc";

/// hermes `_DOWNLOAD_TIMEOUT` (binary is ~16 MB).
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);
/// hermes `_RUN_TIMEOUT`.
const RUN_TIMEOUT: Duration = Duration::from_secs(30);
/// hermes `_STARTUP_GRACE_SECONDS`.
const STARTUP_GRACE: Duration = Duration::from_secs(5);

/// Management (operator) API bearer env var injected into the daemon at
/// start (hermes `_MGMT_API_KEY_ENV`, branded).
pub const MGMT_API_KEY_ENV: &str = "ULNCLAW_IRON_PROXY_MGMT_KEY";
/// The management listener binds loopback at tunnel_port + 2 (tunnel is
/// CONNECT/MITM, +1 plain-HTTP forward).
const MGMT_PORT_OFFSET: u16 = 2;
/// hermes `_MGMT_RELOAD_TIMEOUT`.
const MGMT_RELOAD_TIMEOUT: Duration = Duration::from_secs(15);

/// hermes `_DEFAULT_TUNNEL_PORT`.
pub const DEFAULT_TUNNEL_PORT: u16 = 9090;

/// Hosts allowed by default for AI inference traffic; anything else is
/// 403'd (hermes `_DEFAULT_ALLOWED_HOSTS`).
pub const DEFAULT_ALLOWED_HOSTS: &[&str] = &[
    "openrouter.ai",
    "*.openrouter.ai",
    "api.openai.com",
    "api.anthropic.com",
    "generativelanguage.googleapis.com",
    "api.x.ai",
    "api.mistral.ai",
    "api.groq.com",
    "api.together.xyz",
    "api.deepseek.com",
    "inference.nousresearch.com",
];

/// Provider env-var name -> upstream hosts on which the Authorization
/// Bearer token should be swapped (hermes `_BEARER_PROVIDERS`).
pub const BEARER_PROVIDERS: &[(&str, &[&str])] = &[
    ("OPENROUTER_API_KEY", &["openrouter.ai", "*.openrouter.ai"]),
    ("OPENAI_API_KEY", &["api.openai.com"]),
    ("GROQ_API_KEY", &["api.groq.com"]),
    ("TOGETHER_API_KEY", &["api.together.xyz"]),
    ("DEEPSEEK_API_KEY", &["api.deepseek.com"]),
    ("MISTRAL_API_KEY", &["api.mistral.ai"]),
    ("XAI_API_KEY", &["api.x.ai"]),
    ("NOUS_API_KEY", &["inference.nousresearch.com"]),
];

/// Header-auth provider spec (hermes `_HEADER_AUTH_PROVIDERS` entries).
pub struct HeaderAuthProvider {
    pub env_name: &'static str,
    pub hosts: &'static [&'static str],
    pub match_headers: &'static [&'static str],
    /// Interchangeable env-var names for the SAME upstream credential;
    /// aliases collapse into a single mapping (two require-rules on the
    /// same host would reject each other's requests).
    pub aliases: &'static [&'static str],
}

/// hermes `_HEADER_AUTH_PROVIDERS` — providers whose API authenticates
/// with a NON-Authorization header (iron-proxy v0.39 `match_headers`
/// targets arbitrary header names case-insensitively).
pub const HEADER_AUTH_PROVIDERS: &[HeaderAuthProvider] = &[
    // Anthropic native: x-api-key. Authorization is also matched so an
    // SDK sending the token as a Bearer (OAuth-style) still swaps.
    HeaderAuthProvider {
        env_name: "ANTHROPIC_API_KEY",
        hosts: &["api.anthropic.com"],
        match_headers: &["x-api-key", "Authorization"],
        aliases: &[],
    },
    // Azure OpenAI: api-key header (AAD bearer flows use Authorization).
    HeaderAuthProvider {
        env_name: "AZURE_OPENAI_API_KEY",
        hosts: &[
            "*.openai.azure.com",
            "*.cognitiveservices.azure.com",
            "*.services.ai.azure.com",
        ],
        match_headers: &["api-key", "Authorization"],
        aliases: &[],
    },
    // Google AI Studio (Gemini): x-goog-api-key header; SDKs passing
    // ?key=<token> are covered by match_query.
    HeaderAuthProvider {
        env_name: "GEMINI_API_KEY",
        hosts: &["generativelanguage.googleapis.com"],
        match_headers: &["x-goog-api-key"],
        aliases: &["GOOGLE_API_KEY"],
    },
];

/// Providers we recognize but whose auth genuinely cannot be swapped by
/// a static header/query replacement (hermes `_NON_BEARER_PROVIDERS`).
pub const NON_BEARER_PROVIDERS: &[&str] = &[
    // AWS Bedrock / SageMaker: SigV4-signed requests.
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    // GCP Vertex AI: OAuth bearer minted by the SDK from a
    // service-account file, not a static env key.
    "GOOGLE_APPLICATION_CREDENTIALS",
];

/// Default SSRF-protection deny list applied to the proxy's outbound
/// traffic (hermes `_DEFAULT_UPSTREAM_DENY_CIDRS`).
pub const DEFAULT_UPSTREAM_DENY_CIDRS: &[&str] = &[
    "127.0.0.0/8",    // IPv4 loopback
    "::1/128",        // IPv6 loopback
    "169.254.0.0/16", // IPv4 link-local incl. AWS/GCP/Azure IMDS
    "fe80::/10",      // IPv6 link-local
    "10.0.0.0/8",     // RFC1918
    "172.16.0.0/12",  // RFC1918
    "192.168.0.0/16", // RFC1918
    "fc00::/7",       // IPv6 ULA
    // IPv4-mapped IPv6 covers the dual-stack IMDS bypass.
    "::ffff:0:0/96",
    // RFC6598 / CGNAT (AWS VPC shared services, K8s pod networks).
    "100.64.0.0/10",
    // RFC2544 benchmark range.
    "198.18.0.0/15",
];

/// Min env vars the iron-proxy subprocess actually needs; everything
/// else is stripped (hermes `_PROXY_SUBPROCESS_ENV_ALLOWLIST`).
const PROXY_SUBPROCESS_ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "TMPDIR",
    "TZ",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "NO_COLOR",
    "SSL_CERT_DIR",
    "SSL_CERT_FILE",
    "SYSTEMROOT",  // Windows
    "USERPROFILE", // Windows
];

/// Env vars stripped from the subprocess env even if allowlisted or
/// named in mappings — they would recurse the proxy back through
/// itself or send its traffic through a corporate proxy (hermes
/// `_PROXY_SUBPROCESS_ENV_STRIP`).
const PROXY_SUBPROCESS_ENV_STRIP: &[&str] = &[
    "HTTPS_PROXY",
    "https_proxy",
    "HTTP_PROXY",
    "http_proxy",
    "ALL_PROXY",
    "all_proxy",
    "NO_PROXY",
    "no_proxy",
];

/// Nonce env var planted in the child at start (hermes
/// `_HERMES_IRON_PROXY_NONCE_ENV`, branded) — lets `pid_alive` confirm
/// a candidate PID still refers to *our* binary across PID recycling.
const NONCE_ENV: &str = "ULNCLAW_IRON_PROXY_NONCE";

/// Cached `iron-proxy --version` output keyed by binary path (hermes
/// `_VERSION_CACHE`).
static VERSION_CACHE: std::sync::Mutex<Option<HashMap<String, String>>> =
    std::sync::Mutex::new(None);
/// In-process nonce set by `start_proxy` (hermes `_proxy_nonce`).
static PROXY_NONCE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Map a sandbox-visible proxy token to a real upstream credential
/// lookup (hermes `TokenMapping`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenMapping {
    pub proxy_token: String,
    /// Env-var name iron-proxy reads at egress time.
    pub real_env_name: String,
    pub upstream_hosts: Vec<String>,
    /// Request headers iron-proxy scans for the proxy token.
    pub match_headers: Vec<String>,
    /// Additional env-var names the SANDBOX receives the same proxy
    /// token under (e.g. GOOGLE_API_KEY for GEMINI_API_KEY).
    pub alias_env_names: Vec<String>,
}

/// Snapshot of the iron-proxy installation + runtime state (hermes
/// `ProxyStatus`).
#[derive(Debug, Clone, Default)]
pub struct ProxyStatus {
    pub enabled: bool,
    pub binary_path: Option<PathBuf>,
    pub binary_version: Option<String>,
    pub config_path: Option<PathBuf>,
    pub ca_cert_path: Option<PathBuf>,
    pub pid: Option<u32>,
    pub listening: bool,
    pub tunnel_port: u16,
    pub warnings: Vec<String>,
}

impl ProxyStatus {
    pub fn installed(&self) -> bool {
        self.binary_path
            .as_ref()
            .map(|p| p.exists())
            .unwrap_or(false)
    }

    pub fn configured(&self) -> bool {
        self.config_path
            .as_ref()
            .map(|p| p.exists())
            .unwrap_or(false)
            && self
                .ca_cert_path
                .as_ref()
                .map(|p| p.exists())
                .unwrap_or(false)
    }
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

fn bin_dir() -> PathBuf {
    crate::config::ulnclaw_home().join("bin")
}

/// Read-only state dir — status probes must not materialize
/// `<home>/proxy/` (hermes `_proxy_state_dir_ro`).
fn proxy_state_dir_ro() -> PathBuf {
    crate::config::ulnclaw_home().join("proxy")
}

/// Writable state dir, created 0o700 (holds the CA signing key, audit
/// log, pidfile) — hermes `_proxy_state_dir`.
fn proxy_state_dir() -> PathBuf {
    let dir = proxy_state_dir_ro();
    let _ = std::fs::create_dir_all(&dir);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    dir
}

fn platform_binary_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "iron-proxy.exe"
    } else {
        "iron-proxy"
    }
}

/// Map (os, arch) → upstream release asset filename (hermes
/// `_platform_asset_name`). Windows builds aren't published upstream.
fn platform_asset_name() -> Result<String, String> {
    let os = if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else {
        return Err(format!(
            "iron-proxy does not ship native Windows binaries as of \
             v{IRON_PROXY_VERSION}. Run the proxy on a Linux/macOS host, or inside WSL."
        ));
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => {
            return Err(format!(
                "Unsupported platform for iron-proxy auto-install: {os} {other}"
            ))
        }
    };
    Ok(format!(
        "iron-proxy_{IRON_PROXY_VERSION}_{os}_{arch}.tar.gz"
    ))
}

/// PATH lookup (hermes `shutil.which`).
fn which(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        let candidate = dir.join(name);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            return meta.permissions().mode() & 0o111 != 0;
        }
        return false;
    }
    #[cfg(not(unix))]
    true
}

/// Subprocess probe with a hard timeout (hermes guards every CLI
/// interaction; a hung binary must not wedge status probes).
fn output_with_timeout(
    mut cmd: std::process::Command,
    timeout: Duration,
) -> Option<std::process::Output> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = cmd.output();
        let _ = tx.send(result);
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok(out)) => Some(out),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Binary discovery + lazy install
// ---------------------------------------------------------------------------

/// Return a path to a usable `iron-proxy` binary (hermes
/// `find_iron_proxy`): managed copy first, then PATH, then optional
/// auto-install.
pub fn find_iron_proxy(install_if_missing: bool) -> Option<PathBuf> {
    let managed = bin_dir().join(platform_binary_name());
    if is_executable(&managed) {
        return Some(managed);
    }
    if let Some(system) = which("iron-proxy") {
        return Some(system);
    }
    if install_if_missing {
        match install_iron_proxy(false) {
            Ok(path) => return Some(path),
            Err(e) => eprintln!("[egress] iron-proxy auto-install failed: {e}"),
        }
    }
    None
}

/// Download, verify, and install the pinned `iron-proxy` binary (hermes
/// `install_iron_proxy`). Returns the installed executable path.
pub fn install_iron_proxy(force: bool) -> Result<PathBuf, String> {
    let bin_dir = bin_dir();
    std::fs::create_dir_all(&bin_dir).map_err(|e| format!("create {}: {e}", bin_dir.display()))?;
    let target = bin_dir.join(platform_binary_name());
    if target.exists() && !force {
        return Ok(target);
    }

    let asset_name = platform_asset_name()?;
    let asset_url = format!("{IRON_PROXY_RELEASE_BASE}/{asset_name}");
    let checksum_url = format!("{IRON_PROXY_RELEASE_BASE}/{CHECKSUM_NAME}");

    let tmp = std::env::temp_dir().join(format!("ulnclaw-iron-proxy-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&tmp).map_err(|e| format!("tempdir: {e}"))?;
    let result = install_into(
        &tmp,
        &bin_dir,
        &target,
        &asset_name,
        &asset_url,
        &checksum_url,
    );
    let _ = std::fs::remove_dir_all(&tmp);
    result
}

fn install_into(
    tmp: &Path,
    bin_dir: &Path,
    target: &Path,
    asset_name: &str,
    asset_url: &str,
    checksum_url: &str,
) -> Result<PathBuf, String> {
    let archive_path = tmp.join(asset_name);
    let checksum_path = tmp.join(CHECKSUM_NAME);

    eprintln!("[egress] downloading {asset_url}");
    http_download(asset_url, &archive_path)?;
    http_download(checksum_url, &checksum_path)?;

    // Defense-in-depth: verify the GPG signature of checksums.txt
    // before trusting it. Best-effort — hard-fails only on a
    // present-but-bad signature (tamper signal).
    verify_checksums_signature(tmp, &checksum_path)?;

    let expected = expected_sha256(&checksum_path, asset_name)?;
    let actual = sha256_file(&archive_path)?;
    if !expected.eq_ignore_ascii_case(&actual) {
        return Err(format!(
            "Checksum mismatch for {asset_name}: expected {expected}, got {actual}"
        ));
    }

    let member = pick_tar_member(&archive_path, platform_binary_name())?;
    let status = std::process::Command::new("tar")
        .args([
            "-xzf",
            &archive_path.to_string_lossy(),
            "-C",
            &tmp.to_string_lossy(),
            &member,
        ])
        .status()
        .map_err(|e| format!("spawn tar: {e}"))?;
    if !status.success() {
        return Err(format!("tar extraction failed for member {member}"));
    }
    let extracted = tmp.join(&member);

    // Stage into the final directory then atomically rename so the new
    // binary is never visible half-written.
    let staged = bin_dir.join(format!(".iron-proxy_{}", uuid::Uuid::new_v4()));
    std::fs::copy(&extracted, &staged).map_err(|e| format!("stage binary: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755));
    }
    std::fs::rename(&staged, target).map_err(|e| {
        let _ = std::fs::remove_file(&staged);
        format!("install rename: {e}")
    })?;

    // Invalidate the version cache so a freshly-installed binary
    // re-probes `--version` on the next status call.
    if let Ok(mut guard) = VERSION_CACHE.lock() {
        if let Some(cache) = guard.as_mut() {
            cache.remove(&target.to_string_lossy().to_string());
        }
    }

    eprintln!(
        "[egress] installed iron-proxy {IRON_PROXY_VERSION} at {}",
        target.display()
    );
    Ok(target.to_path_buf())
}

fn http_download(url: &str, dest: &Path) -> Result<(), String> {
    let response = reqwest::blocking::Client::builder()
        .timeout(DOWNLOAD_TIMEOUT)
        .user_agent("ulnclaw")
        .build()
        .map_err(|e| format!("HTTP client: {e}"))?
        .get(url)
        .send()
        .map_err(|e| format!("Failed to download {url}: {e}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "Failed to download {url}: HTTP {}",
            response.status()
        ));
    }
    let bytes = response
        .bytes()
        .map_err(|e| format!("Failed to download {url}: {e}"))?;
    std::fs::write(dest, &bytes).map_err(|e| format!("write {}: {e}", dest.display()))?;
    Ok(())
}

/// Best-effort GPG verification of `checksums.txt` (hermes
/// `_verify_checksums_signature`). Ok(false) when verification is
/// unavailable (no gpg / missing assets — SHA-256 still enforced);
/// Err ONLY when verification actively FAILS (tamper signal).
fn verify_checksums_signature(tmp: &Path, checksum_path: &Path) -> Result<bool, String> {
    let Some(gpg) = which("gpg") else {
        eprintln!(
            "[egress] gpg not found on PATH — skipping iron-proxy release-signature \
             verification (SHA-256 checksum check still enforced)."
        );
        return Ok(false);
    };

    let sig_url = format!("{IRON_PROXY_RELEASE_BASE}/{CHECKSUM_SIG_NAME}");
    let pubkey_url = format!("{IRON_PROXY_RELEASE_BASE}/{PUBKEY_NAME}");
    let sig_path = tmp.join(CHECKSUM_SIG_NAME);
    let pubkey_path = tmp.join(PUBKEY_NAME);
    if http_download(&sig_url, &sig_path).is_err()
        || http_download(&pubkey_url, &pubkey_path).is_err()
    {
        eprintln!(
            "[egress] iron-proxy release signature assets unavailable — skipping GPG \
             verification (SHA-256 checksum check still enforced)."
        );
        return Ok(false);
    }

    // Ephemeral keyring so we never touch the user's real GPG home.
    let gnupg_home = tmp.join("gnupg");
    let _ = std::fs::create_dir_all(&gnupg_home);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&gnupg_home, std::fs::Permissions::from_mode(0o700));
    }
    let base = |args: &[&str]| {
        let mut cmd = std::process::Command::new(&gpg);
        cmd.arg("--homedir")
            .arg(gnupg_home.as_os_str())
            .args(["--batch", "--no-tty"])
            .args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        cmd
    };

    let imported = output_with_timeout(
        base(&["--import", &pubkey_path.to_string_lossy()]),
        Duration::from_secs(60),
    );
    match imported {
        Some(out) if out.status.success() => {}
        _ => {
            eprintln!(
                "[egress] could not import iron-proxy signing key — skipping GPG \
                 verification (SHA-256 still enforced)."
            );
            return Ok(false);
        }
    }

    let verified = output_with_timeout(
        base(&[
            "--verify",
            &sig_path.to_string_lossy(),
            &checksum_path.to_string_lossy(),
        ]),
        Duration::from_secs(60),
    );
    match verified {
        Some(out) if out.status.success() => {
            eprintln!("[egress] verified iron-proxy checksums.txt GPG signature.");
            Ok(true)
        }
        Some(out) => {
            let detail = String::from_utf8_lossy(&out.stderr);
            let detail: String = detail.chars().take(300).collect();
            Err(format!(
                "iron-proxy checksums.txt failed GPG signature verification — refusing \
                 to install (possible release-channel tampering). gpg: {detail}"
            ))
        }
        None => Err("iron-proxy GPG verification timed out".into()),
    }
}

/// Parse the standard `sha256sum` output: `<hex>  <filename>` (hermes
/// `_expected_sha256`).
fn expected_sha256(checksum_file: &Path, asset_name: &str) -> Result<String, String> {
    let text = std::fs::read_to_string(checksum_file)
        .map_err(|e| format!("read {}: {e}", checksum_file.display()))?;
    for line in text.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 && parts[parts.len() - 1] == asset_name {
            return Ok(parts[0].to_string());
        }
    }
    Err(format!(
        "No checksum entry for {asset_name} in {}",
        checksum_file
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
    ))
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let digest = Sha256::digest(&bytes);
    Ok(digest.iter().map(|b| format!("{b:02x}")).collect())
}

/// Find the binary inside the upstream tar (hermes `_pick_tar_member`):
/// leaf name match, no absolute paths, no `..` traversal, shortest
/// candidate wins. Listing rides `tar -tzf`.
fn pick_tar_member(archive: &Path, binary_name: &str) -> Result<String, String> {
    let out = output_with_timeout(
        {
            let mut cmd = std::process::Command::new("tar");
            cmd.args(["-tzf", &archive.to_string_lossy()])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            cmd
        },
        RUN_TIMEOUT,
    )
    .ok_or_else(|| "tar listing timed out".to_string())?;
    if !out.status.success() {
        return Err("tar listing failed on downloaded archive".into());
    }
    let listing = String::from_utf8_lossy(&out.stdout);
    let mut candidates: Vec<&str> = Vec::new();
    for line in listing.lines() {
        let name = line.trim_end();
        if name.is_empty() || name.ends_with('/') {
            continue; // directories / empty entries
        }
        if name.starts_with('/') || Path::new(name).components().any(|c| c.as_os_str() == "..") {
            continue;
        }
        if Path::new(name)
            .file_name()
            .map(|leaf| leaf == binary_name)
            .unwrap_or(false)
        {
            candidates.push(name);
        }
    }
    if candidates.is_empty() {
        return Err(format!(
            "Could not find {binary_name} inside downloaded archive"
        ));
    }
    candidates.sort_by_key(|name| name.len());
    Ok(candidates[0].to_string())
}

/// `iron-proxy --version`, stripped; empty on failure. Cached by
/// binary path (hermes `iron_proxy_version`).
pub fn iron_proxy_version(binary: &Path) -> String {
    let key = binary.to_string_lossy().to_string();
    if let Ok(guard) = VERSION_CACHE.lock() {
        if let Some(cache) = guard.as_ref() {
            if let Some(hit) = cache.get(&key) {
                return hit.clone();
            }
        }
    }
    let version = output_with_timeout(
        {
            let mut cmd = std::process::Command::new(binary);
            cmd.arg("--version")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            cmd
        },
        RUN_TIMEOUT,
    )
    .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
    .unwrap_or_default();
    if let Ok(mut guard) = VERSION_CACHE.lock() {
        guard
            .get_or_insert_with(HashMap::new)
            .insert(key, version.clone());
    }
    version
}

// ---------------------------------------------------------------------------
// CA cert + tokens
// ---------------------------------------------------------------------------

/// Generate (or return existing) iron-proxy CA cert + key via the host
/// `openssl` (hermes `ensure_ca_cert`).
pub fn ensure_ca_cert(force: bool) -> Result<(PathBuf, PathBuf), String> {
    let state = proxy_state_dir();
    let ca_crt = state.join("ca.crt");
    let ca_key = state.join("ca.key");
    if ca_crt.exists() && ca_key.exists() && !force {
        return Ok((ca_crt, ca_key));
    }
    if which("openssl").is_none() {
        return Err(
            "openssl not found on PATH. Install OpenSSL (apt: `openssl`, brew: \
             `openssl`) to generate the iron-proxy CA cert."
                .into(),
        );
    }

    // 10-year cert: iron-proxy mints short-lived leaf certs from this
    // CA, so the CA only rotates when explicitly forced.
    let tmp = std::env::temp_dir().join(format!("ulnclaw-proxy-ca-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&tmp).map_err(|e| format!("tempdir: {e}"))?;
    let result = generate_ca(&tmp, &ca_crt, &ca_key);
    let _ = std::fs::remove_dir_all(&tmp);
    result?;
    eprintln!("[egress] generated iron-proxy CA at {}", ca_crt.display());
    Ok((ca_crt, ca_key))
}

fn generate_ca(tmp: &Path, ca_crt: &Path, ca_key: &Path) -> Result<(), String> {
    let tmp_key = tmp.join("ca.key");
    let tmp_crt = tmp.join("ca.crt");
    let genrsa = output_with_timeout(
        {
            let mut cmd = std::process::Command::new("openssl");
            cmd.args(["genrsa", "-out", &tmp_key.to_string_lossy(), "4096"])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            cmd
        },
        Duration::from_secs(60),
    )
    .ok_or("openssl genrsa timed out")?;
    if !genrsa.status.success() {
        return Err(format!(
            "openssl genrsa failed: {}",
            String::from_utf8_lossy(&genrsa.stderr).trim()
        ));
    }
    let req = output_with_timeout(
        {
            let mut cmd = std::process::Command::new("openssl");
            cmd.args([
                "req",
                "-x509",
                "-new",
                "-nodes",
                "-key",
                &tmp_key.to_string_lossy(),
                "-sha256",
                "-days",
                "3650",
                "-subj",
                "/CN=ulnclaw iron-proxy CA",
                "-addext",
                "basicConstraints=critical,CA:TRUE",
                "-addext",
                "keyUsage=critical,keyCertSign",
                "-out",
                &tmp_crt.to_string_lossy(),
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
            cmd
        },
        Duration::from_secs(60),
    )
    .ok_or("openssl req timed out")?;
    if !req.status.success() {
        return Err(format!(
            "openssl req failed: {}",
            String::from_utf8_lossy(&req.stderr).trim()
        ));
    }

    // CRITICAL: the key must be 0o600 from the very first byte — stage
    // with explicit perms, then atomically rename (no TOCTOU window).
    let key_bytes = std::fs::read(&tmp_key).map_err(|e| format!("read tmp key: {e}"))?;
    let crt_bytes = std::fs::read(&tmp_crt).map_err(|e| format!("read tmp cert: {e}"))?;
    write_private_atomic(ca_key, &key_bytes)?;
    std::fs::write(ca_crt, &crt_bytes).map_err(|e| format!("write ca.crt: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(ca_crt, std::fs::Permissions::from_mode(0o644));
    }
    Ok(())
}

/// Write bytes to `path` 0o600 via a staged temp file + atomic rename
/// (O_NOFOLLOW guards against planted symlinks).
fn write_private_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let staged = path.with_extension(format!(
        "{}.staged",
        path.extension().unwrap_or_default().to_string_lossy()
    ));
    let _ = std::fs::remove_file(&staged);
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options
            .open(&staged)
            .map_err(|e| format!("stage {}: {e}", path.display()))?;
        use std::io::Write;
        file.write_all(bytes)
            .map_err(|e| format!("write {}: {e}", path.display()))?;
    }
    std::fs::rename(&staged, path).map_err(|e| {
        let _ = std::fs::remove_file(&staged);
        format!("rename {}: {e}", path.display())
    })?;
    Ok(())
}

/// Mint a fresh opaque token to hand to the sandbox (hermes
/// `mint_proxy_token`): recognizable prefix + 128-bit random suffix.
pub fn mint_proxy_token(prefix: &str) -> String {
    let mut bytes = [0u8; 32];
    bytes[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    bytes[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    let digest = Sha256::digest(bytes);
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("{prefix}-{}", &hex[..32])
}

fn management_token_path() -> PathBuf {
    proxy_state_dir().join("management.token")
}

/// Management-API bearer key, minted on first call; stored 0600 (hermes
/// `ensure_management_token`).
pub fn ensure_management_token(force: bool) -> Result<String, String> {
    let path = management_token_path();
    if !force {
        if let Ok(existing) = std::fs::read_to_string(&path) {
            let trimmed = existing.trim();
            if !trimmed.is_empty() {
                return Ok(trimmed.to_string());
            }
        }
    }
    let token = mint_proxy_token("ulnclaw-mgmt");
    write_private_atomic(&path, token.as_bytes())?;
    Ok(token)
}

fn read_management_token() -> Option<String> {
    let path = proxy_state_dir_ro().join("management.token");
    match std::fs::read_to_string(path) {
        Ok(text) if !text.trim().is_empty() => Some(text.trim().to_string()),
        _ => None,
    }
}

/// `(host, port)` of the management listener from proxy.yaml (hermes
/// `_read_management_listen_from_config`).
fn read_management_listen_from_config(config_path: Option<&Path>) -> Option<(String, u16)> {
    let owned = proxy_state_dir_ro().join("proxy.yaml");
    let cfg = config_path.unwrap_or(&owned);
    let text = std::fs::read_to_string(cfg).ok()?;
    let value: serde_yaml::Value = serde_yaml::from_str(&text).ok()?;
    let listen = value.get("management")?.get("listen")?.as_str()?;
    let (host, port) = listen.rsplit_once(':')?;
    Some((
        if host.is_empty() {
            "127.0.0.1".into()
        } else {
            host.to_string()
        },
        port.parse().ok()?,
    ))
}

/// Hot-reload the running daemon's ruleset via the management API
/// (hermes `reload_proxy`): POST /v1/reload, no restart, no dropped
/// connections; validation failures leave the running config untouched.
pub fn reload_proxy() -> Result<(), String> {
    let pid = read_pid();
    if pid.is_none() || !pid_alive(pid.unwrap_or(0)) {
        return Err(
            "iron-proxy is not running — nothing to reload.  Run `ulnclaw egress start`.".into(),
        );
    }
    let Some((host, port)) = read_management_listen_from_config(None) else {
        return Err(
            "The generated proxy.yaml has no management listener (written before reload \
             support).  Re-run `ulnclaw egress setup` and use `ulnclaw egress restart` \
             this one time."
                .into(),
        );
    };
    let Some(token) = read_management_token() else {
        return Err(
            "management.token is missing — re-run `ulnclaw egress setup`, then \
             `ulnclaw egress restart`."
                .into(),
        );
    };

    let client = reqwest::blocking::Client::builder()
        .timeout(MGMT_RELOAD_TIMEOUT)
        .build()
        .map_err(|e| format!("HTTP client: {e}"))?;
    let result = client
        .post(format!("http://{host}:{port}/v1/reload"))
        .bearer_auth(token)
        .body(Vec::new())
        .send();
    match result {
        Ok(resp) => {
            let status = resp.status().as_u16();
            if status == 200 {
                return Ok(());
            }
            let body: String = resp.text().unwrap_or_default().chars().take(500).collect();
            if status == 422 {
                return Err(format!(
                    "iron-proxy rejected the new config (validation failed; the running \
                     ruleset is unchanged): {body}"
                ));
            }
            if status == 401 {
                return Err(
                    "management API rejected our key (401).  The running daemon was \
                     started with a different management.token — run `ulnclaw egress \
                     restart`."
                        .into(),
                );
            }
            Err(format!("management reload failed (HTTP {status}): {body}"))
        }
        Err(e) => Err(format!(
            "could not reach the management API at {host}:{port} ({e}).  If the daemon \
             was started before reload support, run `ulnclaw egress restart` once."
        )),
    }
}

// ---------------------------------------------------------------------------
// Listen-address policy
// ---------------------------------------------------------------------------

/// Single host:port bind the proxy should listen on (hermes
/// `_default_http_listen`): Linux binds the docker bridge gateway (what
/// `host.docker.internal` resolves to inside containers); macOS/Windows
/// Docker Desktop resolves via VPNkit, so loopback is reachable and
/// least-exposed. Never 0.0.0.0.
fn default_http_listen(tunnel_port: u16) -> Vec<String> {
    if cfg!(target_os = "linux") {
        match detect_docker_bridge_ip() {
            Some(ip) => vec![format!("{ip}:{tunnel_port}")],
            None => {
                eprintln!(
                    "[egress] no docker bridge detected — binding loopback only; \
                     containers cannot reach the proxy until docker0 exists."
                );
                vec![format!("127.0.0.1:{tunnel_port}")]
            }
        }
    } else {
        vec![format!("127.0.0.1:{tunnel_port}")]
    }
}

/// docker0 bridge IPv4, if present (hermes `_detect_docker_bridge_ip`).
/// Validated: only RFC1918-private addresses are accepted — a hostile
/// `ip` shim cannot inject 0.0.0.0 or a public address.
fn detect_docker_bridge_ip() -> Option<String> {
    let out = output_with_timeout(
        {
            let mut cmd = std::process::Command::new("ip");
            cmd.args(["-4", "-o", "addr", "show", "docker0"])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            cmd
        },
        Duration::from_secs(2),
    )?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut candidate: Option<String> = None;
    for line in stdout.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        for (i, tok) in parts.iter().enumerate() {
            if *tok == "inet" && i + 1 < parts.len() {
                candidate = Some(parts[i + 1].split('/').next().unwrap_or("").to_string());
                break;
            }
        }
        if candidate.is_some() {
            break;
        }
    }
    let candidate = candidate.filter(|c| !c.is_empty())?;
    let addr: std::net::Ipv4Addr = candidate.parse().ok()?;
    // docker0 must be RFC1918 private: rejects unspecified, loopback,
    // link-local (IMDS), multicast, CGNAT and public addresses alike.
    if !addr.is_private() {
        eprintln!(
            "[egress] refusing suspicious docker bridge IP {candidate} reported by `ip`; \
             skipping bridge bind."
        );
        return None;
    }
    Some(addr.to_string())
}

// ---------------------------------------------------------------------------
// Proxy config + token mapping generation
// ---------------------------------------------------------------------------

/// Build the iron-proxy YAML config for a given mapping set (hermes
/// `build_proxy_config`). iron-proxy reads real secrets from its OWN
/// environment (`source: {type: env, var: ...}`); the sandbox never
/// sees them. Schema mirrors iron-proxy v0.39.0.
pub fn build_proxy_config(
    mappings: &[TokenMapping],
    ca_cert: &Path,
    ca_key: &Path,
    tunnel_port: u16,
    _audit_log: Option<&Path>,
    allowed_hosts: Option<Vec<String>>,
    upstream_deny_cidrs: Option<Vec<String>>,
    http_listen: Option<Vec<String>>,
) -> serde_yaml::Value {
    let mut hosts: Vec<String> = allowed_hosts.unwrap_or_else(|| {
        DEFAULT_ALLOWED_HOSTS
            .iter()
            .map(|s| s.to_string())
            .collect()
    });
    for m in mappings {
        for h in &m.upstream_hosts {
            if !hosts.contains(h) {
                hosts.push(h.clone());
            }
        }
    }

    let mut secrets_rules: Vec<serde_yaml::Value> = Vec::new();
    for m in mappings {
        let match_headers: Vec<String> = if m.match_headers.is_empty() {
            vec!["Authorization".into()]
        } else {
            m.match_headers.clone()
        };
        secrets_rules.push(serde_yaml::to_value(serde_json::json!({
            "source": {"type": "env", "var": m.real_env_name},
            "replace": {
                "proxy_value": m.proxy_token,
                // Per-provider header set; v0.39 matches header names
                // case-insensitively. Query matching covers SDKs passing
                // ?key=<token>; body matching stays off.
                "match_headers": match_headers,
                "match_query": true,
                "match_body": false,
                // Fail closed: requests reaching an allowlisted upstream
                // WITHOUT the proxy token are rejected instead of
                // forwarded as-is.
                "require": true,
            },
            "rules": m.upstream_hosts.iter().map(|h| serde_json::json!({"host": h})).collect::<Vec<_>>(),
        }))
        .unwrap_or(serde_yaml::Value::Null));
    }

    let deny_cidrs: Vec<String> = upstream_deny_cidrs.unwrap_or_else(|| {
        DEFAULT_UPSTREAM_DENY_CIDRS
            .iter()
            .map(|s| s.to_string())
            .collect()
    });

    // iron-proxy v0.39 takes ONE bind per listener field. tunnel_listen
    // is the CONNECT/MITM listener sandboxes hit via HTTPS_PROXY;
    // http_listen is the absolute-form plain-HTTP forward listener on
    // tunnel_port+1. Both get the sandbox-facing bind host.
    let listens: Vec<String> = http_listen.unwrap_or_else(|| default_http_listen(tunnel_port));
    let primary_listen = listens
        .first()
        .cloned()
        .unwrap_or_else(|| format!("127.0.0.1:{tunnel_port}"));
    let bind_host = primary_listen
        .rsplit_once(':')
        .map(|(h, _)| h.to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "127.0.0.1".into());
    let plain_http_listen = format!("{bind_host}:{}", tunnel_port + 1);

    // NOTE: `log.audit_path` is NOT a field in iron-proxy v0.39's
    // config.Log struct — the pre-created audit.log is a forward-compat
    // sentinel only (consumed by ensure_audit_log/docs).
    serde_yaml::to_value(serde_json::json!({
        // Required by the binary's parser; tunnel-only mode keeps it on
        // an ephemeral loopback port.
        "dns": {
            "listen": "127.0.0.1:0",
            "proxy_ip": "127.0.0.1",
        },
        "proxy": {
            "tunnel_listen": primary_listen,
            "http_listen": plain_http_listen,
            // Direct-TLS listener gets a loopback ephemeral port.
            "https_listen": "127.0.0.1:0",
            "max_request_body_bytes": 16 * 1024 * 1024,
            "max_response_body_bytes": 0,
            "upstream_response_header_timeout": "120s",
            // SSRF protection: deny outbound to cloud metadata +
            // loopback + RFC1918 by default; [] opts out.
            "upstream_deny_cidrs": deny_cidrs,
        },
        // v0.39 starts a metrics server on :9090 by default — the same
        // port as the default tunnel — so pin it to an ephemeral
        // loopback port to avoid a guaranteed bind collision.
        "metrics": {
            "listen": "127.0.0.1:0",
        },
        // Operator-facing management API — loopback only, bearer-key
        // authenticated; POST /v1/reload hot-swaps the transform
        // pipeline. Sandboxes must never reach it.
        "management": {
            "listen": format!("127.0.0.1:{}", tunnel_port + MGMT_PORT_OFFSET),
            "api_key_env": MGMT_API_KEY_ENV,
        },
        "tls": {
            "ca_cert": ca_cert.to_string_lossy(),
            "ca_key": ca_key.to_string_lossy(),
            "cert_cache_size": 1000,
            "leaf_cert_expiry_hours": 168,
        },
        "transforms": [
            {"name": "allowlist", "config": {"domains": hosts}},
            {"name": "secrets", "config": {"secrets": secrets_rules}},
        ],
        "log": {"level": "info"},
    }))
    .unwrap_or(serde_yaml::Value::Null)
}

/// Create the audit log 0o600 (hermes `ensure_audit_log`). On v0.39 the
/// daemon never writes it — the pre-create is forward-compat for a
/// future dedicated audit stream.
pub fn ensure_audit_log(audit_path: &Path) -> Result<(), String> {
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(audit_path).map_err(|e| {
            format!(
                "Refusing to start: could not pre-create audit log {} with restrictive \
                 permissions ({e}).  Move or chmod any existing file at that path and retry.",
                audit_path.display()
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
        }
    }
    Ok(())
}

/// Serialize the config to `<home>/proxy/proxy.yaml` (chmod 0600 before
/// the atomic replace — the config embeds proxy token values; hermes
/// `write_proxy_config`).
pub fn write_proxy_config(config: &serde_yaml::Value) -> Result<PathBuf, String> {
    let state = proxy_state_dir();
    let out = state.join("proxy.yaml");
    let tmp_path = state.join(".proxy.yaml.tmp");
    let text = serde_yaml::to_string(config).map_err(|e| format!("serialize proxy.yaml: {e}"))?;
    std::fs::write(&tmp_path, text).map_err(|e| format!("write tmp proxy.yaml: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp_path, &out).map_err(|e| format!("rename proxy.yaml: {e}"))?;
    Ok(out)
}

/// Persist sandbox-visible proxy tokens to `mappings.json` (read by the
/// Docker backend at sandbox start; NOT read by iron-proxy itself —
/// hermes `write_mappings`).
pub fn write_mappings(mappings: &[TokenMapping]) -> Result<PathBuf, String> {
    let state = proxy_state_dir();
    let out = state.join("mappings.json");
    let payload = serde_json::json!({
        "version": 1,
        "tokens": mappings.iter().map(|m| serde_json::json!({
            "proxy_token": m.proxy_token,
            "env_name": m.real_env_name,
            "upstream_hosts": m.upstream_hosts,
            "match_headers": m.match_headers,
            "alias_env_names": m.alias_env_names,
        })).collect::<Vec<_>>(),
    });
    let tmp_path = state.join(".mappings.json.tmp");
    std::fs::write(
        &tmp_path,
        serde_json::to_string_pretty(&payload).map_err(|e| format!("serialize mappings: {e}"))?,
    )
    .map_err(|e| format!("write tmp mappings.json: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp_path, &out).map_err(|e| format!("rename mappings.json: {e}"))?;
    Ok(out)
}

/// Read mappings.json; empty on any error (hermes `load_mappings`).
pub fn load_mappings() -> Vec<TokenMapping> {
    load_mappings_from(&proxy_state_dir_ro().join("mappings.json"))
}

fn load_mappings_from(path: &Path) -> Vec<TokenMapping> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(payload) = serde_json::from_str::<serde_json::Value>(&text) else {
        eprintln!("[egress] failed to read iron-proxy mappings.json (parse error)");
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in payload
        .get("tokens")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
    {
        let Some(proxy_token) = item.get("proxy_token").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(env_name) = item.get("env_name").and_then(|v| v.as_str()) else {
            continue;
        };
        let string_list = |key: &str| -> Vec<String> {
            item.get(key)
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default()
        };
        let mut match_headers = string_list("match_headers");
        if match_headers.is_empty() {
            // Pre-header-auth files load with the bearer default —
            // identical to their behavior at write time.
            match_headers = vec!["Authorization".into()];
        }
        out.push(TokenMapping {
            proxy_token: proxy_token.to_string(),
            real_env_name: env_name.to_string(),
            upstream_hosts: string_list("upstream_hosts"),
            match_headers,
            alias_env_names: string_list("alias_env_names"),
        });
    }
    out
}

/// Mint a TokenMapping for every known provider whose env var is set
/// (hermes `discover_provider_mappings`). `available_env_names`
/// overrides the lookup source (Bitwarden mode).
pub fn discover_provider_mappings(available_env_names: Option<&[String]>) -> Vec<TokenMapping> {
    let names: std::collections::HashSet<String> = match available_env_names {
        Some(list) => list.iter().cloned().collect(),
        None => std::env::vars()
            .filter(|(_, v)| !v.is_empty())
            .map(|(k, _)| k)
            .collect(),
    };
    let prefix_for = |env_name: &str| env_name.to_lowercase().replace("_api_key", "");

    let mut mappings = Vec::new();
    for (env_name, hosts) in BEARER_PROVIDERS {
        if !names.contains(*env_name) {
            continue;
        }
        mappings.push(TokenMapping {
            proxy_token: mint_proxy_token(&prefix_for(env_name)),
            real_env_name: env_name.to_string(),
            upstream_hosts: hosts.iter().map(|s| s.to_string()).collect(),
            match_headers: vec!["Authorization".into()],
            alias_env_names: Vec::new(),
        });
    }
    for spec in HEADER_AUTH_PROVIDERS {
        // A mapping is minted when the canonical name OR any alias is
        // available; aliases collapse into ONE mapping (two require-rules
        // on the same host would reject each other's requests).
        if !names.contains(spec.env_name) && !spec.aliases.iter().any(|a| names.contains(*a)) {
            continue;
        }
        mappings.push(TokenMapping {
            proxy_token: mint_proxy_token(&prefix_for(spec.env_name)),
            real_env_name: spec.env_name.to_string(),
            upstream_hosts: spec.hosts.iter().map(|s| s.to_string()).collect(),
            match_headers: spec.match_headers.iter().map(|s| s.to_string()).collect(),
            alias_env_names: spec.aliases.iter().map(|s| s.to_string()).collect(),
        });
    }
    mappings
}

/// Env-var names for providers we recognize but can't proxy (hermes
/// `discover_uncovered_providers`) — the wizard/status print a warning.
pub fn discover_uncovered_providers(available_env_names: Option<&[String]>) -> Vec<String> {
    let names: std::collections::HashSet<String> = match available_env_names {
        Some(list) => list.iter().cloned().collect(),
        None => std::env::vars()
            .filter(|(_, v)| !v.is_empty())
            .map(|(k, _)| k)
            .collect(),
    };
    NON_BEARER_PROVIDERS
        .iter()
        .filter(|n| names.contains(**n))
        .map(|n| n.to_string())
        .collect()
}

/// Combine an existing mapping set with freshly discovered providers
/// (hermes `merge_mappings`): preserve tokens for providers already in
/// `existing` (re-setup must not invalidate tokens baked into running
/// sandboxes) unless `rotate`; providers no longer discovered are
/// dropped.
pub fn merge_mappings(
    existing: &[TokenMapping],
    discovered: Vec<TokenMapping>,
    rotate: bool,
) -> Vec<TokenMapping> {
    let by_name: HashMap<&str, &TokenMapping> = existing
        .iter()
        .map(|m| (m.real_env_name.as_str(), m))
        .collect();
    discovered
        .into_iter()
        .map(|d| match by_name.get(d.real_env_name.as_str()) {
            Some(prior) if !rotate => TokenMapping {
                // Preserve the token; refresh hosts/headers/aliases in
                // case the provider spec changed since last setup.
                proxy_token: prior.proxy_token.clone(),
                real_env_name: prior.real_env_name.clone(),
                upstream_hosts: d.upstream_hosts,
                match_headers: d.match_headers,
                alias_env_names: d.alias_env_names,
            },
            _ => d,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Subprocess lifecycle
// ---------------------------------------------------------------------------

fn pidfile_path() -> PathBuf {
    proxy_state_dir().join("iron-proxy.pid")
}

/// Read the pidfile without materializing the state dir (hermes
/// `_read_pid`).
fn read_pid() -> Option<u32> {
    let pf = proxy_state_dir_ro().join("iron-proxy.pid");
    let text = std::fs::read_to_string(&pf).ok()?;
    let pid: u32 = text.trim().parse().ok()?;
    (pid > 0).then_some(pid)
}

fn mint_nonce() -> String {
    let digest = Sha256::digest(uuid::Uuid::new_v4().as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// /proc/<pid>/stat starttime (field 22) — cheap PID-recycling
/// detection (hermes `_pid_proc_starttime`).
fn pid_proc_starttime(pid: u32) -> Option<String> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm can contain spaces/parens — split from the right parenthesis.
    let rparen = text.rfind(')')?;
    let fields: Vec<&str> = text[rparen + 1..].split_whitespace().collect();
    // original field 22 (1-indexed) → tail index 22-3 = 19
    fields.get(19).map(|s| s.to_string())
}

fn persisted_nonce_path() -> PathBuf {
    proxy_state_dir_ro().join("iron-proxy.nonce")
}

/// On-disk nonce written next to the pidfile (cross-CLI-invocation
/// PID-recycling defense; hermes `_read_persisted_nonce`).
fn read_persisted_nonce() -> Option<String> {
    let path = persisted_nonce_path();
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .ok()?;
        let meta = file.metadata().ok()?;
        if meta.uid() != unsafe { libc::getuid() } {
            return None;
        }
        let mut buf = vec![0u8; 256];
        use std::io::Read;
        let n = (&file).read(&mut buf).ok()?;
        let text = String::from_utf8_lossy(&buf[..n]).trim().to_string();
        return (!text.is_empty()).then_some(text);
    }
    #[cfg(not(unix))]
    {
        let text = std::fs::read_to_string(&path).ok()?.trim().to_string();
        (!text.is_empty()).then_some(text)
    }
}

fn process_exists(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let r = unsafe { libc::kill(pid as i32, 0) };
        if r == 0 {
            return true;
        }
        // EPERM = alive but not ours.
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

/// True iff `pid` is alive AND is an iron-proxy process (hermes
/// `_pid_alive`). Defends against PID reuse via three signals:
/// 1. /proc/<pid>/environ carries our nonce (most reliable, Linux)
/// 2. /proc/<pid>/cmdline argv0 basename matches the managed binary
/// 3. `ps -p <pid>` comm basename matches (macOS fallback)
fn pid_alive(pid: u32) -> bool {
    if pid == 0 || !process_exists(pid) {
        return false;
    }

    let mut nonce_candidates: Vec<String> = Vec::new();
    if let Ok(guard) = PROXY_NONCE.lock() {
        if let Some(nonce) = guard.as_ref() {
            nonce_candidates.push(nonce.clone());
        }
    }
    if let Some(on_disk) = read_persisted_nonce() {
        if !nonce_candidates.contains(&on_disk) {
            nonce_candidates.push(on_disk);
        }
    }
    if !nonce_candidates.is_empty() {
        if let Ok(env_bytes) = std::fs::read(format!("/proc/{pid}/environ")) {
            for nonce in &nonce_candidates {
                let needle = format!("{NONCE_ENV}={nonce}");
                if env_bytes
                    .windows(needle.len())
                    .any(|w| w == needle.as_bytes())
                {
                    return true;
                }
            }
        }
    }

    // Fallback: cmdline basename match.
    if let Ok(bytes) = std::fs::read(format!("/proc/{pid}/cmdline")) {
        if let Some(argv0) = bytes.split(|b| *b == 0).next() {
            let argv0 = String::from_utf8_lossy(argv0);
            let base = Path::new(argv0.as_ref())
                .file_name()
                .map(|b| b.to_string_lossy().to_string())
                .unwrap_or_default();
            return base.starts_with("iron-proxy");
        }
    }

    // macOS / non-Linux fallback: `ps` comm basename.
    if let Some(out) = output_with_timeout(
        {
            let mut cmd = std::process::Command::new("ps");
            cmd.args(["-p", &pid.to_string(), "-o", "comm="])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            cmd
        },
        Duration::from_secs(2),
    ) {
        if out.status.success() {
            let comm = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let base = Path::new(&comm)
                .file_name()
                .map(|b| b.to_string_lossy().to_string())
                .unwrap_or_default();
            return base.starts_with("iron-proxy");
        }
    }

    // Exotic platforms: if the OS says alive we believe it.
    true
}

/// Write the pidfile with O_EXCL + O_NOFOLLOW + ownership check;
/// persists the nonce sibling for cross-process checks (hermes
/// `_write_pidfile_safely`).
fn write_pidfile_safely(pidfile: &Path, pid: u32) -> Result<(), String> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let open_err = |e: std::io::Error| {
        format!(
            "Refusing to write pidfile {}: {e}.  Remove that path manually and retry.",
            pidfile.display()
        )
    };
    let mut file = match options.open(pidfile) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // Either another start is in flight, or a stale pidfile
            // survived a crash — discriminate and retry once if stale.
            if let Some(existing) = read_pid() {
                if pid_alive(existing) {
                    return Err(format!(
                        "Another iron-proxy start appears to be in progress (pidfile {} \
                         -> pid {existing}).  Run `ulnclaw egress stop` if that proxy is \
                         stuck.",
                        pidfile.display()
                    ));
                }
            }
            let _ = std::fs::remove_file(pidfile);
            options.open(pidfile).map_err(open_err)?
        }
        Err(e) => return Err(open_err(e)),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let Ok(meta) = file.metadata() {
            if meta.uid() != unsafe { libc::getuid() } {
                return Err(format!(
                    "pidfile {} has unexpected owner uid={}",
                    pidfile.display(),
                    meta.uid()
                ));
            }
        }
    }
    use std::io::Write;
    file.write_all(pid.to_string().as_bytes())
        .map_err(|e| format!("write pidfile: {e}"))?;
    drop(file);

    // Persist the nonce next to the pidfile (best-effort; without it we
    // fall back to argv0-basename matching).
    if let Ok(guard) = PROXY_NONCE.lock() {
        if let Some(nonce) = guard.as_ref() {
            let noncefile = pidfile.with_extension("nonce");
            let _ = write_private_atomic(&noncefile, nonce.as_bytes());
        }
    }
    Ok(())
}

/// Best-effort SIGTERM → wait → SIGKILL for a child we own (hermes
/// `_kill_and_wait`).
fn kill_and_wait(child: &mut std::process::Child, grace_seconds: u64) {
    #[cfg(unix)]
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(grace_seconds);
    loop {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    #[cfg(unix)]
    unsafe {
        libc::kill(child.id() as i32, libc::SIGKILL);
    }
    let _ = child.wait();
}

/// Construct the minimal env for the iron-proxy subprocess (hermes
/// `_build_proxy_subprocess_env`): allowlisted infra vars + the real
/// upstream secrets named in mappings.json, everything else stripped.
/// With `refresh_from_bitwarden` the secrets come from `bws` instead —
/// that is what delivers the rotation promise of credential_source
/// bitwarden.
pub fn build_proxy_subprocess_env(
    extra_env: Option<&HashMap<String, String>>,
    refresh_from_bitwarden: bool,
    bitwarden_cfg: Option<&crate::secrets::BitwardenSourceConfig>,
    allow_env_fallback: bool,
) -> Result<HashMap<String, String>, String> {
    let parent: HashMap<String, String> = std::env::vars().collect();
    let mut env: HashMap<String, String> = HashMap::new();
    for name in PROXY_SUBPROCESS_ENV_ALLOWLIST {
        if let Some(value) = parent.get(*name) {
            env.insert(name.to_string(), value.clone());
        }
    }

    // Forward the real upstream secrets — but only those. For alias
    // providers the rule is keyed on the canonical name; when only the
    // alias is set in the host env, mirror its value into the canonical.
    let mappings = load_mappings();
    let mut needed: Vec<String> = Vec::new();
    let mut alias_sources: HashMap<String, Vec<String>> = HashMap::new();
    for m in &mappings {
        needed.push(m.real_env_name.clone());
        if !m.alias_env_names.is_empty() {
            alias_sources.insert(m.real_env_name.clone(), m.alias_env_names.clone());
        }
    }
    for name in &needed {
        if let Some(value) = parent.get(name) {
            env.insert(name.clone(), value.clone());
        } else if let Some(aliases) = alias_sources.get(name) {
            for alias in aliases {
                if let Some(value) = parent.get(alias).filter(|v| !v.is_empty()) {
                    env.insert(name.clone(), value.clone());
                    break;
                }
            }
        }
    }

    if refresh_from_bitwarden {
        match bitwarden_cfg {
            Some(cfg) => {
                let result =
                    crate::secrets::fetch_bitwarden_source(cfg, &crate::config::ulnclaw_home());
                if !result.ok {
                    let detail = result.error.unwrap_or_else(|| "unknown error".into());
                    if !allow_env_fallback {
                        return Err(format!(
                            "Bitwarden refresh failed at proxy start: {detail}.  Either \
                             fix the Bitwarden config, switch to credential_source: env \
                             via `ulnclaw egress setup --no-bitwarden`, or set \
                             `proxy.allow_env_fallback: true` to opt into the legacy \
                             host-env fallback."
                        ));
                    }
                    eprintln!(
                        "[egress] Bitwarden refresh failed ({detail}) — falling back to \
                         host env (allow_env_fallback=true)."
                    );
                } else {
                    let mut missing: Vec<String> = Vec::new();
                    for name in &needed {
                        match result.secrets.get(name) {
                            Some(value) => {
                                env.insert(name.clone(), value.clone());
                            }
                            None => missing.push(name.clone()),
                        }
                    }
                    if !missing.is_empty() {
                        // Don't silently keep stale host-env values when
                        // BWS mode was explicitly selected.
                        if !allow_env_fallback {
                            return Err(format!(
                                "Bitwarden refresh did not return secrets for {}.  Either \
                                 add the secrets to your BWS project, switch to \
                                 credential_source: env via `ulnclaw egress setup \
                                 --no-bitwarden`, or set `proxy.allow_env_fallback: true` \
                                 to opt into the legacy host-env fallback.",
                                missing.join(", ")
                            ));
                        }
                        eprintln!(
                            "[egress] Bitwarden refresh did not return secrets for {} — \
                             falling back to host env for those names \
                             (allow_env_fallback=true).",
                            missing.join(", ")
                        );
                    }
                    if !result.warnings.is_empty() {
                        eprintln!(
                            "[egress] Bitwarden refresh produced {} warning(s); run \
                             `ulnclaw secrets bitwarden status` for detail.",
                            result.warnings.len()
                        );
                    }
                }
            }
            None => {
                if !allow_env_fallback {
                    return Err(
                        "credential_source=bitwarden but the Bitwarden config is missing.  \
                         Either configure [secrets.bitwarden], switch to credential_source: \
                         env, or set `proxy.allow_env_fallback: true` to opt into the \
                         legacy fallback behaviour."
                            .into(),
                    );
                }
                eprintln!(
                    "[egress] credential_source=bitwarden but no Bitwarden config — proxy \
                     will fall back to parent env (allow_env_fallback=true)."
                );
            }
        }
    }

    // Caller-supplied overrides win (intentionally last).
    if let Some(extra) = extra_env {
        for (key, value) in extra {
            env.insert(key.clone(), value.clone());
        }
    }

    // Strip proxy-recursion-risk vars regardless of how they got in.
    for name in PROXY_SUBPROCESS_ENV_STRIP {
        env.remove(*name);
    }
    env.entry("NO_COLOR".into()).or_insert_with(|| "1".into());
    Ok(env)
}

/// Options for `start_proxy` (hermes `start_proxy` kwargs).
pub struct StartOptions {
    pub binary: Option<PathBuf>,
    pub config_path: Option<PathBuf>,
    pub extra_env: Option<HashMap<String, String>>,
    pub install_if_missing: bool,
    pub refresh_secrets_from_bitwarden: bool,
    pub bitwarden_config: Option<crate::secrets::BitwardenSourceConfig>,
    pub allow_env_fallback: bool,
}

/// Spawn iron-proxy as a managed background subprocess (hermes
/// `start_proxy`). Idempotent — an already-running proxy returns the
/// live status.
pub fn start_proxy(opts: &StartOptions) -> Result<ProxyStatus, String> {
    if let Some(existing) = read_pid() {
        if pid_alive(existing) {
            return Ok(get_status());
        }
    }

    let bin_path = opts
        .binary
        .clone()
        .or_else(|| find_iron_proxy(opts.install_if_missing))
        .ok_or("iron-proxy binary not available — run `ulnclaw egress install`.")?;
    let cfg = opts
        .config_path
        .clone()
        .unwrap_or_else(|| proxy_state_dir().join("proxy.yaml"));
    if !cfg.exists() {
        return Err(format!(
            "iron-proxy config not found at {}. Run `ulnclaw egress setup` first.",
            cfg.display()
        ));
    }

    // Minimal env: os.environ wholesale would ship every operator
    // secret to the proxy (/proc/<pid>/environ exposure).
    let mut env = build_proxy_subprocess_env(
        opts.extra_env.as_ref(),
        opts.refresh_secrets_from_bitwarden,
        opts.bitwarden_config.as_ref(),
        opts.allow_env_fallback,
    )?;

    // Management API: the daemon validates api_key_env is non-empty at
    // startup when management.listen is set.
    if read_management_listen_from_config(Some(&cfg)).is_some() {
        env.insert(
            MGMT_API_KEY_ENV.to_string(),
            ensure_management_token(false)?,
        );
    }

    // Per-start nonce for PID-recycling defense.
    let nonce = mint_nonce();
    if let Ok(mut guard) = PROXY_NONCE.lock() {
        *guard = Some(nonce.clone());
    }
    env.insert(NONCE_ENV.to_string(), nonce);

    let log_path = proxy_state_dir().join("iron-proxy.log");
    let log_file = open_log_append(&log_path)?;
    let stdout = log_file
        .try_clone()
        .map_err(|e| format!("dup log fd: {e}"))?;
    let stderr = log_file
        .try_clone()
        .map_err(|e| format!("dup log fd: {e}"))?;

    let mut cmd = std::process::Command::new(&bin_path);
    cmd.arg("-config")
        .arg(&cfg)
        .env_clear()
        .envs(&env)
        .stdin(std::process::Stdio::null())
        .stdout(stdout)
        .stderr(stderr);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn iron-proxy: {e}"))?;
    drop(log_file);

    // Pidfile IMMEDIATELY after spawn, BEFORE listening verification —
    // if the parent dies mid-poll, `egress stop` can still clean up.
    let pidfile = pidfile_path();
    if let Err(e) = write_pidfile_safely(&pidfile, child.id()) {
        kill_and_wait(&mut child, 2);
        return Err(e);
    }

    // Poll-with-timeout (the Go binary normally comes up in <200 ms).
    // Probe the CONFIGURED bind host — on Linux that's the docker
    // bridge gateway, where loopback never connects.
    let (probe_host, tunnel_port) = read_http_listen_from_config()
        .unwrap_or_else(|| ("127.0.0.1".to_string(), DEFAULT_TUNNEL_PORT));
    let deadline = std::time::Instant::now() + STARTUP_GRACE;
    let mut listening = false;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let tail = tail_log(&log_path, 20);
                let _ = std::fs::remove_file(&pidfile);
                return Err(format!(
                    "iron-proxy exited immediately (code {}). Last log lines:\n{tail}",
                    status.code().unwrap_or(-1)
                ));
            }
            _ => {}
        }
        if port_listening(&probe_host, tunnel_port) {
            listening = true;
            break;
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // Final exit check — the process may have died right at deadline.
    if let Ok(Some(status)) = child.try_wait() {
        let tail = tail_log(&log_path, 20);
        let _ = std::fs::remove_file(&pidfile);
        return Err(format!(
            "iron-proxy exited immediately (code {}). Last log lines:\n{tail}",
            status.code().unwrap_or(-1)
        ));
    }
    if !listening {
        // Alive but non-listening at deadline: kill it, otherwise the
        // orphan holds the port and restarts fail with address-in-use.
        let tail = tail_log(&log_path, 20);
        kill_and_wait(&mut child, 2);
        let _ = std::fs::remove_file(&pidfile);
        return Err(format!(
            "iron-proxy did not bind {probe_host}:{tunnel_port} within {}s.  Process \
             was killed.  Last log lines:\n{tail}",
            STARTUP_GRACE.as_secs()
        ));
    }

    eprintln!(
        "[egress] started iron-proxy pid={} config={}",
        child.id(),
        cfg.display()
    );
    // Dropping the Child handle leaves the detached daemon running.
    drop(child);
    Ok(get_status())
}

fn open_log_append(log_path: &Path) -> Result<std::fs::File, String> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(log_path).map_err(|e| {
        format!(
            "Refusing to write iron-proxy log {}: {e}.  Remove that path manually and retry.",
            log_path.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
        if let Ok(meta) = file.metadata() {
            if meta.uid() != unsafe { libc::getuid() } {
                return Err(format!(
                    "iron-proxy log {} has unexpected owner uid={}; refusing to write.",
                    log_path.display(),
                    meta.uid()
                ));
            }
        }
    }
    Ok(file)
}

/// Stop the managed iron-proxy; true if it was running (hermes
/// `stop_proxy`).
pub fn stop_proxy() -> bool {
    fn cleanup_state_files() {
        let _ = std::fs::remove_file(proxy_state_dir_ro().join("iron-proxy.pid"));
        let _ = std::fs::remove_file(proxy_state_dir_ro().join("iron-proxy.nonce"));
    }
    fn clear_nonce() {
        if let Ok(mut guard) = PROXY_NONCE.lock() {
            *guard = None;
        }
    }

    let Some(pid) = read_pid() else {
        cleanup_state_files();
        clear_nonce();
        return false;
    };
    if !pid_alive(pid) {
        cleanup_state_files();
        clear_nonce();
        return false;
    }

    // Capture starttime BEFORE signalling — if the pid gets recycled
    // mid-wait, abort the SIGKILL.
    let starttime_before = pid_proc_starttime(pid);
    #[cfg(unix)]
    {
        if unsafe { libc::kill(pid as i32, libc::SIGTERM) } != 0 {
            cleanup_state_files();
            clear_nonce();
            return false;
        }
    }

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut exited = false;
    while std::time::Instant::now() < deadline {
        if !pid_alive(pid) {
            exited = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if !exited {
        let starttime_after = pid_proc_starttime(pid);
        let recycled = (starttime_before.is_some()
            && starttime_after.is_some()
            && starttime_before != starttime_after)
            || !pid_alive(pid);
        if recycled {
            eprintln!(
                "[egress] iron-proxy pid={pid} appears recycled before SIGKILL; not killing."
            );
        } else {
            #[cfg(unix)]
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
            }
        }
    }

    cleanup_state_files();
    clear_nonce();
    eprintln!("[egress] stopped iron-proxy pid={pid}");
    true
}

/// Snapshot the current proxy state — does NOT start anything (hermes
/// `get_status`); read-only so it can run per container-create.
pub fn get_status() -> ProxyStatus {
    let mut status = ProxyStatus::default();
    let (probe_host, port) = read_http_listen_from_config()
        .unwrap_or_else(|| ("127.0.0.1".to_string(), DEFAULT_TUNNEL_PORT));
    status.tunnel_port = port;

    if let Some(binary) = find_iron_proxy(false) {
        let version = iron_proxy_version(&binary);
        status.binary_path = Some(binary);
        status.binary_version = (!version.is_empty()).then_some(version);
    }

    let state = proxy_state_dir_ro();
    let cfg = state.join("proxy.yaml");
    let ca = state.join("ca.crt");
    if cfg.exists() {
        status.config_path = Some(cfg);
    }
    if ca.exists() {
        status.ca_cert_path = Some(ca);
    }

    if let Some(pid) = read_pid() {
        if pid_alive(pid) {
            status.pid = Some(pid);
            status.listening = port_listening(&probe_host, port);
        }
    }
    status
}

/// `(host, port)` of the configured sandbox-facing listener
/// (`proxy.tunnel_listen`, falling back to `proxy.http_listen`) —
/// hermes `_read_http_listen_from_config`.
fn read_http_listen_from_config() -> Option<(String, u16)> {
    let cfg = proxy_state_dir_ro().join("proxy.yaml");
    let text = std::fs::read_to_string(&cfg).ok()?;
    let value: serde_yaml::Value = serde_yaml::from_str(&text).ok()?;
    let proxy_block = value.get("proxy")?;
    let listen = proxy_block
        .get("tunnel_listen")
        .or_else(|| proxy_block.get("http_listen"))?
        .as_str()?;
    let (host, port) = listen.rsplit_once(':')?;
    Some((
        if host.is_empty() {
            "127.0.0.1".into()
        } else {
            host.to_string()
        },
        port.parse().ok()?,
    ))
}

/// Cheap TCP connect probe (hermes `_port_listening`).
fn port_listening(host: &str, port: u16) -> bool {
    let Ok(addr) = format!("{host}:{port}").parse::<std::net::SocketAddr>() else {
        return false;
    };
    std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok()
}

fn tail_log(path: &Path, lines: usize) -> String {
    let Ok(data) = std::fs::read(path) else {
        return "(no log file)".into();
    };
    let start = data.len().saturating_sub(8192);
    let text = String::from_utf8_lossy(&data[start..]);
    let collected: Vec<&str> = text.lines().collect();
    collected[collected.len().saturating_sub(lines)..].join("\n")
}

/// Clear module-level caches between tests (hermes `_reset_for_tests`).
#[cfg(test)]
pub fn reset_for_tests() {
    if let Ok(mut guard) = VERSION_CACHE.lock() {
        *guard = None;
    }
    if let Ok(mut guard) = PROXY_NONCE.lock() {
        *guard = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mint_token_format() {
        let token = mint_proxy_token("openai");
        assert!(token.starts_with("openai-"));
        let suffix = token.strip_prefix("openai-").unwrap();
        assert_eq!(suffix.len(), 32);
        assert!(suffix.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(token, mint_proxy_token("openai"));
    }

    #[test]
    fn sha256_file_known_digest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob");
        std::fs::write(&path, b"abc").unwrap();
        assert_eq!(
            sha256_file(&path).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn expected_sha256_parses_sha256sum_format() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("checksums.txt");
        std::fs::write(
            &path,
            "deadbeef  iron-proxy_0.39.0_linux_amd64.tar.gz\nfeedface  other.tar.gz\n",
        )
        .unwrap();
        assert_eq!(
            expected_sha256(&path, "iron-proxy_0.39.0_linux_amd64.tar.gz").unwrap(),
            "deadbeef"
        );
        assert!(expected_sha256(&path, "missing.tar.gz").is_err());
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn platform_asset_name_matches_hermes() {
        assert_eq!(
            platform_asset_name().unwrap(),
            format!("iron-proxy_{IRON_PROXY_VERSION}_linux_amd64.tar.gz")
        );
    }

    #[cfg(unix)]
    #[test]
    fn pick_tar_member_prefers_shortest_and_rejects_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("iron-proxy"), b"#!/bin/sh\n").unwrap();
        std::fs::create_dir_all(root.join("nested")).unwrap();
        std::fs::write(root.join("nested").join("iron-proxy"), b"#!/bin/sh\n").unwrap();
        std::fs::write(root.join("README.md"), b"docs").unwrap();
        let archive = root.join("archive.tar.gz");
        let status = std::process::Command::new("tar")
            .args([
                "-czf",
                &archive.to_string_lossy(),
                "-C",
                &root.to_string_lossy(),
                "iron-proxy",
                "nested",
                "README.md",
            ])
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(
            pick_tar_member(&archive, "iron-proxy").unwrap(),
            "iron-proxy"
        );
        assert!(pick_tar_member(&archive, "no-such-bin").is_err());
    }

    #[test]
    fn discover_provider_mappings_env_names_and_aliases() {
        let names: Vec<String> = vec!["OPENAI_API_KEY".into(), "GOOGLE_API_KEY".into()];
        let mappings = discover_provider_mappings(Some(&names));
        assert_eq!(mappings.len(), 2);
        let openai = mappings
            .iter()
            .find(|m| m.real_env_name == "OPENAI_API_KEY")
            .unwrap();
        assert_eq!(openai.upstream_hosts, vec!["api.openai.com".to_string()]);
        assert_eq!(openai.match_headers, vec!["Authorization".to_string()]);
        assert!(openai.proxy_token.starts_with("openai-"));
        // Alias GOOGLE_API_KEY collapses into the canonical GEMINI mapping.
        let gemini = mappings
            .iter()
            .find(|m| m.real_env_name == "GEMINI_API_KEY")
            .unwrap();
        assert_eq!(gemini.alias_env_names, vec!["GOOGLE_API_KEY".to_string()]);
        assert_eq!(gemini.match_headers, vec!["x-goog-api-key".to_string()]);
        assert!(gemini.proxy_token.starts_with("gemini-"));
    }

    #[test]
    fn discover_uncovered_providers_list() {
        let names: Vec<String> = vec![
            "AWS_ACCESS_KEY_ID".into(),
            "GOOGLE_APPLICATION_CREDENTIALS".into(),
            "OPENAI_API_KEY".into(),
        ];
        let uncovered = discover_uncovered_providers(Some(&names));
        assert_eq!(
            uncovered,
            vec![
                "AWS_ACCESS_KEY_ID".to_string(),
                "GOOGLE_APPLICATION_CREDENTIALS".to_string()
            ]
        );
    }

    fn mapping(env: &str, token: &str) -> TokenMapping {
        TokenMapping {
            proxy_token: token.into(),
            real_env_name: env.into(),
            upstream_hosts: vec![format!("api.{env}.example")],
            match_headers: vec!["Authorization".into()],
            alias_env_names: Vec::new(),
        }
    }

    #[test]
    fn merge_mappings_preserves_rotates_and_drops() {
        let existing = vec![mapping("OPENAI_API_KEY", "keep-me")];
        let discovered = vec![
            mapping("OPENAI_API_KEY", "fresh-openai"),
            mapping("GROQ_API_KEY", "fresh-groq"),
        ];
        // Preserve mode: existing token kept, new provider minted.
        let merged = merge_mappings(&existing, discovered.clone(), false);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].proxy_token, "keep-me");
        assert_eq!(merged[1].proxy_token, "fresh-groq");
        // Rotate mode: everything freshly minted.
        let rotated = merge_mappings(&existing, discovered.clone(), true);
        assert_eq!(rotated[0].proxy_token, "fresh-openai");
        // Providers no longer discovered are dropped.
        let merged = merge_mappings(
            &existing,
            vec![mapping("GROQ_API_KEY", "fresh-groq")],
            false,
        );
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].real_env_name, "GROQ_API_KEY");
    }

    #[test]
    fn build_proxy_config_schema_matches_hermes() {
        let mappings = vec![mapping("anthropic", "tok-123")];
        let config = build_proxy_config(
            &mappings,
            Path::new("/state/ca.crt"),
            Path::new("/state/ca.key"),
            9090,
            None,
            None,
            None,
            None,
        );
        let proxy = &config["proxy"];
        assert!(proxy["tunnel_listen"].as_str().unwrap().ends_with(":9090"));
        assert!(proxy["http_listen"].as_str().unwrap().ends_with(":9091"));
        assert_eq!(proxy["https_listen"].as_str().unwrap(), "127.0.0.1:0");
        let deny: Vec<&str> = proxy["upstream_deny_cidrs"]
            .as_sequence()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(deny.contains(&"169.254.0.0/16"));
        assert!(deny.contains(&"::ffff:0:0/96"));
        // Management listener: loopback, tunnel_port + 2.
        assert_eq!(
            config["management"]["listen"].as_str().unwrap(),
            "127.0.0.1:9092"
        );
        assert_eq!(
            config["management"]["api_key_env"].as_str().unwrap(),
            MGMT_API_KEY_ENV
        );
        // Allowlist transform carries defaults plus mapping hosts.
        let domains: Vec<&str> = config["transforms"][0]["config"]["domains"]
            .as_sequence()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(domains.contains(&"api.openai.com"));
        assert!(domains.contains(&"api.anthropic.example"));
        // Secrets rule: fail-closed require + env source.
        let secret = &config["transforms"][1]["config"]["secrets"][0];
        assert_eq!(secret["source"]["type"].as_str().unwrap(), "env");
        assert_eq!(secret["source"]["var"].as_str().unwrap(), "anthropic");
        assert_eq!(
            secret["replace"]["proxy_value"].as_str().unwrap(),
            "tok-123"
        );
        assert_eq!(secret["replace"]["require"].as_bool().unwrap(), true);
        assert_eq!(secret["replace"]["match_body"].as_bool().unwrap(), false);
        assert_eq!(
            secret["rules"][0]["host"].as_str().unwrap(),
            "api.anthropic.example"
        );
        // TLS block references the CA pair.
        assert_eq!(config["tls"]["ca_cert"].as_str().unwrap(), "/state/ca.crt");
        // Explicit empty deny list opts out (hermetic tests).
        let no_deny = build_proxy_config(
            &mappings,
            Path::new("/state/ca.crt"),
            Path::new("/state/ca.key"),
            9090,
            None,
            None,
            Some(Vec::new()),
            None,
        );
        assert!(no_deny["proxy"]["upstream_deny_cidrs"]
            .as_sequence()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn mappings_roundtrip_via_state_dir() {
        let _guard = crate::models_dev::test_env_lock();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("ULNCLAW_HOME", dir.path());
        let mappings = vec![
            TokenMapping {
                proxy_token: "openai-abc".into(),
                real_env_name: "OPENAI_API_KEY".into(),
                upstream_hosts: vec!["api.openai.com".into()],
                match_headers: vec!["Authorization".into()],
                alias_env_names: Vec::new(),
            },
            TokenMapping {
                proxy_token: "gemini-def".into(),
                real_env_name: "GEMINI_API_KEY".into(),
                upstream_hosts: vec!["generativelanguage.googleapis.com".into()],
                match_headers: vec!["x-goog-api-key".into()],
                alias_env_names: vec!["GOOGLE_API_KEY".into()],
            },
        ];
        let path = write_mappings(&mappings).unwrap();
        assert_eq!(path.file_name().unwrap(), "mappings.json");
        assert_eq!(load_mappings(), mappings);
        std::env::remove_var("ULNCLAW_HOME");
    }

    #[test]
    fn management_token_persists_until_forced() {
        let _guard = crate::models_dev::test_env_lock();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("ULNCLAW_HOME", dir.path());
        let first = ensure_management_token(false).unwrap();
        assert!(first.starts_with("ulnclaw-mgmt-"));
        assert_eq!(ensure_management_token(false).unwrap(), first);
        assert_ne!(ensure_management_token(true).unwrap(), first);
        std::env::remove_var("ULNCLAW_HOME");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pid_proc_starttime_reads_own_process() {
        assert!(pid_proc_starttime(std::process::id()).is_some());
        assert!(pid_proc_starttime(999_999_999).is_none());
    }

    #[test]
    fn port_listening_probe() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(port_listening("127.0.0.1", port));
        // Deterministic negative: port 1 is privileged and unbound.
        assert!(!port_listening("127.0.0.1", 1));
    }

    #[test]
    fn subprocess_env_strips_proxy_chain_and_keeps_allowlist() {
        let _guard = crate::models_dev::test_env_lock();
        let dir = tempfile::tempdir().unwrap();
        let prev_home = std::env::var("ULNCLAW_HOME").ok();
        let prev_path = std::env::var("PATH").ok();
        let prev_proxy = std::env::var("HTTPS_PROXY").ok();
        std::env::set_var("ULNCLAW_HOME", dir.path());
        std::env::set_var("HTTPS_PROXY", "http://corp-proxy:3128");
        std::env::set_var("PATH", "/usr/bin");
        let env = build_proxy_subprocess_env(None, false, None, false).unwrap();
        assert!(!env.contains_key("HTTPS_PROXY"));
        assert_eq!(env.get("PATH").map(|s| s.as_str()), Some("/usr/bin"));
        assert_eq!(env.get("NO_COLOR").map(|s| s.as_str()), Some("1"));
        match prev_proxy {
            Some(v) => std::env::set_var("HTTPS_PROXY", v),
            None => std::env::remove_var("HTTPS_PROXY"),
        }
        match prev_path {
            Some(v) => std::env::set_var("PATH", v),
            None => std::env::remove_var("PATH"),
        }
        match prev_home {
            Some(v) => std::env::set_var("ULNCLAW_HOME", v),
            None => std::env::remove_var("ULNCLAW_HOME"),
        }
    }

    #[test]
    fn bitwarden_mode_fails_closed_without_config() {
        let err = build_proxy_subprocess_env(None, true, None, false).unwrap_err();
        assert!(err.contains("credential_source=bitwarden"));
        // allow_env_fallback degrades to a warning instead.
        let env = build_proxy_subprocess_env(None, true, None, true).unwrap();
        assert!(env.contains_key("NO_COLOR"));
    }
}

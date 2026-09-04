//! Local Chromium-family CDP attach helpers.
//!
//! Port of hermes `hermes_cli/browser_connect.py` (v2026.8.3): candidate
//! discovery across macOS/Windows/Linux (incl. WSL), dual-stack loopback
//! CDP probes, port arbitration, and a diagnostics-rich visible debug
//! browser launch (per-candidate attempts, stderr tail, single-instance
//! absorption hint).

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Default CDP debug port (hermes `DEFAULT_BROWSER_CDP_PORT`).
pub const DEFAULT_BROWSER_CDP_PORT: u16 = 9222;

/// Default local CDP URL (hermes `DEFAULT_BROWSER_CDP_URL`).
pub fn default_browser_cdp_url() -> String {
    format!("http://127.0.0.1:{}", DEFAULT_BROWSER_CDP_PORT)
}

// =========================================================================
// Candidate binaries (hermes _DARWIN_APPS / _*_BROWSER_GROUPS)
// =========================================================================

const DARWIN_APPS: &[&str] = &[
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/Applications/Chromium.app/Contents/MacOS/Chromium",
    "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
    "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
];

/// `(bin names searched on PATH, fixed install paths)` per browser family.
const LINUX_GROUPS: &[(&[&str], &[&str])] = &[
    (
        &["google-chrome", "google-chrome-stable"],
        &[
            "/opt/google/chrome/chrome",
            "/usr/bin/google-chrome",
            "/usr/bin/google-chrome-stable",
        ],
    ),
    (
        &["chromium-browser", "chromium"],
        &["/usr/bin/chromium-browser", "/usr/bin/chromium"],
    ),
    (
        &["brave-browser", "brave-browser-stable", "brave"],
        &[
            "/usr/bin/brave-browser",
            "/usr/bin/brave-browser-stable",
            "/usr/bin/brave",
            "/snap/bin/brave",
            "/opt/brave.com/brave/brave-browser",
            "/opt/brave.com/brave/brave",
            "/opt/brave-bin/brave",
        ],
    ),
    (
        &["microsoft-edge", "microsoft-edge-stable", "msedge"],
        &[
            "/usr/bin/microsoft-edge",
            "/usr/bin/microsoft-edge-stable",
            "/opt/microsoft/msedge/microsoft-edge",
            "/opt/microsoft/msedge/msedge",
        ],
    ),
];

/// `(bin names, install path parts joined onto ProgramFiles-style bases)`.
const WINDOWS_GROUPS: &[(&[&str], &[&[&str]])] = &[
    (
        &["chrome.exe", "chrome"],
        &[&["Google", "Chrome", "Application", "chrome.exe"]],
    ),
    (
        &["chromium.exe", "chromium"],
        &[
            &["Chromium", "Application", "chrome.exe"],
            &["Chromium", "Application", "chromium.exe"],
        ],
    ),
    (
        &["brave.exe", "brave"],
        &[&["BraveSoftware", "Brave-Browser", "Application", "brave.exe"]],
    ),
    (
        &["msedge.exe", "msedge"],
        &[&["Microsoft", "Edge", "Application", "msedge.exe"]],
    ),
];

/// `shutil.which` analogue: find `name` (or `name.exe`) on `PATH`.
fn which(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let with_ext = dir.join(format!("{}.exe", name));
            if with_ext.is_file() {
                return Some(with_ext);
            }
        }
    }
    None
}

fn add_windows_install_paths(
    out: &mut Vec<PathBuf>,
    seen: &mut std::collections::HashSet<PathBuf>,
    bases: &[&str],
) {
    for base in bases.iter().filter(|b| !b.is_empty()) {
        for (_, install_parts) in WINDOWS_GROUPS {
            for parts in *install_parts {
                // WSL `/mnt/c/...` bases are POSIX paths regardless of host.
                let mut path = PathBuf::from(base);
                for part in *parts {
                    path = path.join(part);
                }
                if path.is_file() && seen.insert(path.clone()) {
                    out.push(path);
                }
            }
        }
    }
}

fn add_candidate(
    out: &mut Vec<PathBuf>,
    seen: &mut std::collections::HashSet<PathBuf>,
    path: PathBuf,
) {
    if path.is_file() && seen.insert(path.clone()) {
        out.push(path);
    }
}

/// Detected Chromium-family binaries for `system` ("Darwin", "Windows",
/// "Linux"/other) — hermes `get_chrome_debug_candidates`.
pub fn get_chrome_debug_candidates(system: &str) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();

    match system {
        "Darwin" => {
            for app in DARWIN_APPS {
                add_candidate(&mut candidates, &mut seen, PathBuf::from(app));
            }
        }
        "Windows" => {
            let bases: Vec<String> = ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"]
                .iter()
                .filter_map(|var| std::env::var(var).ok())
                .collect();
            for (names, install_parts) in WINDOWS_GROUPS {
                for name in *names {
                    if let Some(found) = which(name) {
                        add_candidate(&mut candidates, &mut seen, found);
                    }
                }
                for base in &bases {
                    for parts in *install_parts {
                        let mut path = PathBuf::from(base);
                        for part in *parts {
                            path = path.join(part);
                        }
                        add_candidate(&mut candidates, &mut seen, path);
                    }
                }
            }
        }
        _ => {
            for (names, paths) in LINUX_GROUPS {
                for name in *names {
                    if let Some(found) = which(name) {
                        add_candidate(&mut candidates, &mut seen, found);
                    }
                }
                for path in *paths {
                    add_candidate(&mut candidates, &mut seen, PathBuf::from(path));
                }
            }
            // WSL: the Windows install may be reachable via /mnt/c.
            add_windows_install_paths(
                &mut candidates,
                &mut seen,
                &["/mnt/c/Program Files", "/mnt/c/Program Files (x86)"],
            );
        }
    }
    candidates
}

/// Host system name in hermes `platform.system()` style.
pub fn host_system() -> &'static str {
    match std::env::consts::OS {
        "macos" => "Darwin",
        "windows" => "Windows",
        _ => "Linux",
    }
}

/// Profile dir for the managed debug browser (hermes `chrome_debug_data_dir`).
pub fn chrome_debug_data_dir() -> PathBuf {
    crate::config::ulnclaw_home().join("chrome-debug")
}

fn chrome_debug_args(port: u16, data_dir: &Path) -> Vec<String> {
    vec![
        format!("--remote-debugging-port={}", port),
        format!("--user-data-dir={}", data_dir.display()),
        "--no-first-run".to_string(),
        "--no-default-browser-check".to_string(),
    ]
}

// =========================================================================
// CDP probes — dual-stack aware (hermes is_browser_debug_ready & friends)
// =========================================================================

fn probe_client(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(timeout)
        .connect_timeout(timeout)
        .build()
        .unwrap_or_default()
}

/// True when `url` exposes a reachable Chrome DevTools endpoint
/// (hermes `is_browser_debug_ready`). Probes `/json/version` then `/json`;
/// `ws(s)://…/devtools/browser/…` URLs get a bare TCP connect.
pub async fn is_browser_debug_ready(url: &str, timeout: Duration) -> bool {
    let normalized = if url.contains("://") {
        url.to_string()
    } else {
        format!("http://{}", url)
    };
    let Ok(parsed) = url::Url::parse(&normalized) else {
        return false;
    };
    let scheme = parsed.scheme();
    let port = match parsed.port() {
        Some(p) => p,
        None => match scheme {
            "https" | "wss" => 443,
            _ => 80,
        },
    };

    if (scheme == "ws" || scheme == "wss") && parsed.path().starts_with("/devtools/browser/") {
        let Some(host) = parsed.host_str() else {
            return false;
        };
        return tokio::time::timeout(
            timeout,
            tokio::net::TcpStream::connect((host.to_string(), port)),
        )
        .await
        .map(|r| r.is_ok())
        .unwrap_or(false);
    }

    let http_scheme = match scheme {
        "ws" => "http",
        "wss" => "https",
        other => other,
    };
    if http_scheme != "http" && http_scheme != "https" {
        return false;
    }
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let host_part = if host.contains(':') {
        format!("[{}]", host)
    } else {
        host.to_string()
    };
    let root = match parsed.port() {
        Some(p) => format!("{}://{}:{}", http_scheme, host_part, p),
        None => format!("{}://{}", http_scheme, host_part),
    };
    let client = probe_client(timeout);
    for probe in [format!("{}/json/version", root), format!("{}/json", root)] {
        match client.get(&probe).send().await {
            Ok(resp) if resp.status().is_success() => return true,
            _ => continue,
        }
    }
    false
}

/// Both loopback literals: a squatter on the IPv4 loopback can push the
/// debug browser to bind `[::1]` only (hermes `_LOOPBACK_PROBE_HOSTS`).
const LOOPBACK_PROBE_HOSTS: &[&str] = &["127.0.0.1", "[::1]"];
const LOOPBACK_SOCKET_HOSTS: &[&str] = &["127.0.0.1", "::1"];

/// First loopback URL (IPv4 first, then IPv6) speaking CDP on `port`
/// (hermes `discover_local_cdp_url`).
pub async fn discover_local_cdp_url(port: u16, timeout: Duration) -> Option<String> {
    for host in LOOPBACK_PROBE_HOSTS {
        let url = format!("http://{}:{}", host, port);
        if is_browser_debug_ready(&url, timeout).await {
            return Some(url);
        }
    }
    None
}

/// True when either loopback accepts TCP on `port` — used after a failed
/// CDP probe to distinguish "port free" from "another app squatting"
/// (hermes `local_port_in_use`).
pub async fn local_port_in_use(port: u16, timeout: Duration) -> bool {
    for host in LOOPBACK_SOCKET_HOSTS {
        let connect = tokio::net::TcpStream::connect((host.to_string(), port));
        if tokio::time::timeout(timeout, connect)
            .await
            .map(|r| r.is_ok())
            .unwrap_or(false)
        {
            return true;
        }
    }
    false
}

/// First port after `preferred` bindable on both loopbacks (hermes
/// `find_free_debug_port`). Falls back to `preferred + 1`.
pub fn find_free_debug_port(preferred: u16, attempts: u16) -> u16 {
    for port in preferred + 1..preferred + 1 + attempts {
        let bindable = ["127.0.0.1", "::1"]
            .iter()
            .all(|host| std::net::TcpListener::bind((*host, port)).is_ok());
        if bindable {
            return port;
        }
    }
    preferred + 1
}

/// Shell-quote one argument for a manual launch command.
fn shell_quote(arg: &str) -> String {
    if arg.is_empty() || arg.contains(char::is_whitespace) || arg.contains('"') {
        format!("\"{}\"", arg.replace('"', "\\\""))
    } else {
        arg.to_string()
    }
}

/// Manual launch command for the user when auto-launch is impossible
/// (hermes `manual_chrome_debug_command`).
pub fn manual_chrome_debug_command(port: u16, system: &str) -> Option<String> {
    let candidates = get_chrome_debug_candidates(system);
    if let Some(first) = candidates.first() {
        let mut argv: Vec<String> = vec![first.display().to_string()];
        argv.extend(chrome_debug_args(port, &chrome_debug_data_dir()));
        return Some(
            argv.iter()
                .map(|a| shell_quote(a))
                .collect::<Vec<_>>()
                .join(" "),
        );
    }
    if system == "Darwin" {
        let data_dir = chrome_debug_data_dir();
        return Some(format!(
            "open -a \"Google Chrome\" --args --remote-debugging-port={} \
             --user-data-dir=\"{}\" --no-first-run --no-default-browser-check",
            port,
            data_dir.display()
        ));
    }
    None
}

// =========================================================================
// Launch with diagnostics (hermes LaunchAttempt / ChromeDebugLaunch)
// =========================================================================

/// Outcome of one candidate-binary launch attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchState {
    Ready,
    Starting,
    Exited,
    SpawnFailed,
}

/// One candidate's launch outcome (hermes `LaunchAttempt`).
#[derive(Debug, Clone)]
pub struct LaunchAttempt {
    pub binary: String,
    pub state: LaunchState,
    pub returncode: Option<i32>,
    pub stderr_tail: String,
}

/// Structured result of [`launch_chrome_debug`] (hermes `ChromeDebugLaunch`).
///
/// `launched` mirrors the legacy boolean contract: a launch command ran and
/// the browser is ready or still starting (it does NOT guarantee the CDP
/// port ever opens). `attempts` carries per-candidate diagnostics.
#[derive(Debug, Clone, Default)]
pub struct ChromeDebugLaunch {
    pub launched: bool,
    pub attempts: Vec<LaunchAttempt>,
}

const STDERR_TAIL_LIMIT: usize = 2000;
const LAUNCH_STDERR_LOG: &str = "launch-stderr.log";

impl ChromeDebugLaunch {
    /// Best user-facing explanation for a failed/soft launch (hermes
    /// `ChromeDebugLaunch.hint`).
    pub fn hint(&self) -> Option<String> {
        for attempt in &self.attempts {
            if attempt.state == LaunchState::Exited && attempt.returncode == Some(0) {
                let name = Path::new(&attempt.binary)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| attempt.binary.clone());
                return Some(format!(
                    "{name} exited immediately without opening the debug port — an already-running \
                     {name} instance likely absorbed the launch (Chromium's single-instance \
                     behavior). Close ALL of its processes (including background/tray instances) \
                     and retry /browser connect."
                ));
            }
        }
        for attempt in &self.attempts {
            if attempt.state == LaunchState::Exited && !attempt.stderr_tail.is_empty() {
                let name = Path::new(&attempt.binary)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| attempt.binary.clone());
                let last = attempt
                    .stderr_tail
                    .lines()
                    .last()
                    .unwrap_or("")
                    .trim()
                    .to_string();
                return Some(format!(
                    "{name} exited before the debug port opened: {last}"
                ));
            }
        }
        None
    }
}

fn read_stderr_tail(path: &Path) -> String {
    let Ok(data) = std::fs::read(path) else {
        return String::new();
    };
    let start = data.len().saturating_sub(STDERR_TAIL_LIMIT);
    String::from_utf8_lossy(&data[start..]).trim().to_string()
}

/// Classify a launched browser as ready, exited, or still starting within a
/// short grace window (hermes `_wait_for_browser_debug_ready_or_exit`).
async fn wait_ready_or_exit(
    child: &mut tokio::process::Child,
    port: u16,
    timeout: Duration,
    interval: Duration,
) -> LaunchState {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        let probe_timeout = interval.min(Duration::from_millis(200));
        if discover_local_cdp_url(port, probe_timeout).await.is_some() {
            return LaunchState::Ready;
        }
        match child.try_wait() {
            Ok(Some(_)) => return LaunchState::Exited,
            Ok(None) => {}
            Err(_) => return LaunchState::Exited,
        }
        tokio::time::sleep(interval).await;
    }
    LaunchState::Starting
}

/// Launch a Chromium-family browser with remote debugging, trying each
/// detected candidate in turn with diagnostics (hermes `launch_chrome_debug`).
pub async fn launch_chrome_debug(port: u16, system: &str) -> ChromeDebugLaunch {
    let candidates = get_chrome_debug_candidates(system);
    if candidates.is_empty() {
        tracing::info!("browser debug launch: no Chromium-family binary found (system={system})");
        return ChromeDebugLaunch::default();
    }
    launch_chrome_debug_candidates(port, &candidates).await
}

/// Candidate-driven core of [`launch_chrome_debug`] (test-friendly).
pub async fn launch_chrome_debug_candidates(
    port: u16,
    candidates: &[PathBuf],
) -> ChromeDebugLaunch {
    let mut result = ChromeDebugLaunch::default();

    let data_dir = chrome_debug_data_dir();
    std::fs::create_dir_all(&data_dir).ok();
    let stderr_path = data_dir.join(LAUNCH_STDERR_LOG);

    for candidate in candidates {
        let Ok(stderr_file) = std::fs::File::create(&stderr_path) else {
            result.attempts.push(LaunchAttempt {
                binary: candidate.display().to_string(),
                state: LaunchState::SpawnFailed,
                returncode: None,
                stderr_tail: String::new(),
            });
            continue;
        };
        let mut cmd = tokio::process::Command::new(&candidate);
        cmd.args(chrome_debug_args(port, &data_dir))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::from(stderr_file));
        #[cfg(unix)]
        cmd.process_group(0); // hermes start_new_session=True

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(err) => {
                result.attempts.push(LaunchAttempt {
                    binary: candidate.display().to_string(),
                    state: LaunchState::SpawnFailed,
                    returncode: None,
                    stderr_tail: String::new(),
                });
                tracing::info!(
                    "browser debug launch: failed to spawn {}: {}",
                    candidate.display(),
                    err
                );
                continue;
            }
        };
        tracing::info!(
            "browser debug launch: spawned {} (pid={:?}) with --remote-debugging-port={}",
            candidate.display(),
            child.id(),
            port
        );

        let state = wait_ready_or_exit(
            &mut child,
            port,
            Duration::from_secs(2),
            Duration::from_millis(100),
        )
        .await;
        let mut attempt = LaunchAttempt {
            binary: candidate.display().to_string(),
            state,
            returncode: None,
            stderr_tail: String::new(),
        };

        if attempt.state != LaunchState::Exited {
            // Dropping the handle leaves the browser running (detached
            // process group); tokio's SIGCHLD watcher reaps it on exit.
            drop(child);
            result.attempts.push(attempt);
            result.launched = true;
            return result;
        }

        attempt.returncode = child.wait().await.ok().and_then(|s| s.code());
        attempt.stderr_tail = read_stderr_tail(&stderr_path);
        tracing::warn!(
            "browser debug launch: {} exited (code={:?}) before port {} opened{}",
            candidate.display(),
            attempt.returncode,
            port,
            if attempt.stderr_tail.is_empty() {
                String::new()
            } else {
                format!("; stderr tail: {}", attempt.stderr_tail)
            }
        );
        result.attempts.push(attempt);
    }
    result
}

/// Boolean contract wrapper (hermes `try_launch_chrome_debug`).
pub async fn try_launch_chrome_debug(port: u16, system: &str) -> bool {
    launch_chrome_debug(port, system).await.launched
}

// =========================================================================
// High-level connect flow (hermes `/browser connect` default-URL branch)
// =========================================================================

/// Result of the default local connect flow.
#[derive(Debug, Clone, Default)]
pub struct ConnectOutcome {
    /// The CDP URL to use, when one came up.
    pub url: Option<String>,
    /// True when the endpoint answered before any launch.
    pub already_open: bool,
    /// Progress/diagnostic lines for the user (hermes prints them inline).
    pub messages: Vec<String>,
}

/// Hermes `/browser connect` default flow: discover on both loopbacks,
/// arbitrate a squatted port, auto-launch with diagnostics, wait for CDP.
pub async fn connect_local_default(port: u16) -> ConnectOutcome {
    let mut outcome = ConnectOutcome::default();

    if let Some(found) = discover_local_cdp_url(port, Duration::from_secs(1)).await {
        outcome.url = Some(found.clone());
        outcome.already_open = true;
        outcome.messages.push(format!(
            "✓ Chromium-family browser is already listening at {found}"
        ));
        return outcome;
    }

    let mut launch_port = port;
    if local_port_in_use(port, Duration::from_millis(500)).await {
        launch_port = find_free_debug_port(port, 10);
        outcome.messages.push(format!(
            "⚠ Port {port} is occupied by another application that isn't a CDP browser"
        ));
        outcome.messages.push(format!(
            "  (an IDE debugger or dev server may be using it) — launching on port {launch_port} instead..."
        ));
    } else {
        outcome.messages.push(
            "Chromium-family browser isn't running with remote debugging — attempting to launch..."
                .to_string(),
        );
    }

    let launch = launch_chrome_debug(launch_port, host_system()).await;
    if launch.launched {
        for _ in 0..10 {
            if let Some(found) = discover_local_cdp_url(launch_port, Duration::from_secs(1)).await {
                outcome.url = Some(found);
                outcome.messages.push(format!(
                    "✓ Chromium-family browser launched and listening on port {launch_port}"
                ));
                return outcome;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        outcome.messages.push(format!(
            "⚠ Browser launched but port {launch_port} isn't responding yet"
        ));
        outcome.messages.push(
            "  Try again in a few seconds — the debug instance may still is starting".to_string(),
        );
        return outcome;
    }

    outcome
        .messages
        .push("⚠ Could not auto-launch a Chromium-family browser".to_string());
    if let Some(hint) = launch.hint() {
        outcome.messages.push(format!("  {hint}"));
    }
    if let Some(manual) = manual_chrome_debug_command(launch_port, host_system()) {
        outcome
            .messages
            .push("  Start one manually and re-run /browser connect:".to_string());
        outcome.messages.push(format!("    {manual}"));
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_url_and_constants() {
        assert_eq!(DEFAULT_BROWSER_CDP_PORT, 9222);
        assert_eq!(default_browser_cdp_url(), "http://127.0.0.1:9222");
    }

    #[test]
    fn candidates_only_contain_existing_files() {
        for system in ["Linux", "Darwin", "Windows"] {
            for candidate in get_chrome_debug_candidates(system) {
                assert!(candidate.is_file(), "candidate must exist: {candidate:?}");
            }
        }
    }

    #[test]
    fn manual_command_quotes_spaces() {
        // With no candidates on Linux the fallback is None unless a browser
        // is installed; assert structure when present.
        let cmd = manual_chrome_debug_command(9333, host_system());
        if let Some(cmd) = cmd {
            assert!(cmd.contains("--remote-debugging-port=9333"));
            assert!(cmd.contains("--no-first-run"));
        }
        // Darwin without the app installed still yields the open -a form.
        let darwin = manual_chrome_debug_command(9333, "Darwin");
        assert!(darwin.is_some());
    }

    #[test]
    fn shell_quote_handles_spaces() {
        assert_eq!(shell_quote("plain"), "plain");
        assert_eq!(shell_quote("with space"), "\"with space\"");
        assert_eq!(shell_quote(""), "\"\"");
    }

    #[test]
    fn hint_prefers_single_instance_absorption() {
        let launch = ChromeDebugLaunch {
            launched: false,
            attempts: vec![
                LaunchAttempt {
                    binary: "/usr/bin/google-chrome".to_string(),
                    state: LaunchState::Exited,
                    returncode: Some(0),
                    stderr_tail: String::new(),
                },
                LaunchAttempt {
                    binary: "/usr/bin/chromium".to_string(),
                    state: LaunchState::Exited,
                    returncode: Some(1),
                    stderr_tail: "crash".to_string(),
                },
            ],
        };
        let hint = launch.hint().unwrap();
        assert!(hint.contains("single-instance"));
        assert!(hint.contains("google-chrome"));

        let crash_only = ChromeDebugLaunch {
            launched: false,
            attempts: vec![LaunchAttempt {
                binary: "/usr/bin/chromium".to_string(),
                state: LaunchState::Exited,
                returncode: Some(1),
                stderr_tail: "line one\nline two".to_string(),
            }],
        };
        assert_eq!(
            crash_only.hint().unwrap(),
            "chromium exited before the debug port opened: line two"
        );

        assert!(ChromeDebugLaunch::default().hint().is_none());
    }

    #[test]
    fn find_free_debug_port_skips_occupied() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let occupied = listener.local_addr().unwrap().port();
        let found = find_free_debug_port(occupied, 10);
        assert_ne!(found, occupied);
        assert!(found > occupied);
    }

    #[tokio::test]
    async fn port_in_use_detects_listener() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(local_port_in_use(port, Duration::from_millis(500)).await);
        drop(listener);
        // P461: under parallel test load the kernel can take a moment to
        // release the socket — poll for the free state instead of betting
        // on a single probe.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut freed = false;
        while std::time::Instant::now() < deadline {
            if !local_port_in_use(port, Duration::from_millis(200)).await {
                freed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(freed, "port stayed in use after listener drop");
    }

    #[tokio::test]
    async fn debug_ready_false_for_dead_port() {
        // Pick a port nothing listens on.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let url = format!("http://127.0.0.1:{port}");
        assert!(!is_browser_debug_ready(&url, Duration::from_millis(300)).await);
        // Invalid scheme is rejected without probing.
        assert!(!is_browser_debug_ready("ftp://127.0.0.1:21", Duration::from_millis(300)).await);
    }

    #[tokio::test]
    async fn launch_with_fake_exiting_binary_collects_diagnostics() {
        // Fake candidate that exits 0 immediately → hermes single-instance
        // absorption case. Never touches a real browser.
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("fake-chrome.sh");
        std::fs::write(&fake, "#!/bin/sh\nexit 0\n").unwrap();
        #[allow(clippy::permissions)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let _guard = crate::models_dev::test_env_lock();
        let prev_home = std::env::var("ULNCLAW_HOME").ok();
        std::env::set_var("ULNCLAW_HOME", dir.path());
        let result = launch_chrome_debug_candidates(49777, &[fake.clone()]).await;
        assert!(!result.launched);
        assert_eq!(result.attempts.len(), 1);
        assert_eq!(result.attempts[0].state, LaunchState::Exited);
        assert_eq!(result.attempts[0].returncode, Some(0));
        let hint = result.hint().unwrap();
        assert!(hint.contains("single-instance"));
        match prev_home {
            Some(v) => std::env::set_var("ULNCLAW_HOME", v),
            None => std::env::remove_var("ULNCLAW_HOME"),
        }
    }

    #[tokio::test]
    async fn launch_with_stderr_records_tail() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("crashy.sh");
        std::fs::write(&fake, "#!/bin/sh\necho 'boom failure' >&2\nexit 1\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let _guard = crate::models_dev::test_env_lock();
        let prev_home = std::env::var("ULNCLAW_HOME").ok();
        std::env::set_var("ULNCLAW_HOME", dir.path());
        let result = launch_chrome_debug_candidates(49778, &[fake.clone()]).await;
        assert!(!result.launched);
        let attempt = &result.attempts[0];
        assert_eq!(attempt.state, LaunchState::Exited);
        assert_eq!(attempt.returncode, Some(1));
        assert!(attempt.stderr_tail.contains("boom failure"));
        assert!(result.hint().unwrap().contains("boom failure"));
        match prev_home {
            Some(v) => std::env::set_var("ULNCLAW_HOME", v),
            None => std::env::remove_var("ULNCLAW_HOME"),
        }
    }
}

//! WhatsApp bridge process supervision — port of the hermes
//! `plugins/platforms/whatsapp` adapter's bridge lifecycle
//! (`connect`/`disconnect`, v2026.8.3).
//!
//! The Baileys bridge (`scripts/whatsapp-bridge/bridge.js`) is bundled
//! into the binary at build time and synced to
//! `<home>/scripts/whatsapp-bridge/` at runtime (hermes mirrors the
//! install-tree bridge into `HERMES_HOME` when read-only; ulnclaw is a
//! single binary, so the embedded source *is* the install tree). The
//! supervisor then:
//!
//! 1. resolves `node` on PATH (`find_node_executable`),
//! 2. runs `npm install` when `node_modules` is missing or its
//!    `.hermes-pkg-hash` stamp disagrees with `package.json`,
//! 3. kills stale bridges — pidfile-verified (`bridge.pid` carries the
//!    PID plus the kernel start time so a recycled PID can never be
//!    signalled) and LISTEN-state port occupants,
//! 4. spawns `node bridge.js --port N --session DIR --mode MODE` with
//!    stdout/stderr appended to `<session>/../bridge.log`,
//! 5. waits for readiness in two phases (HTTP up ≤15 s, then
//!    `status == "connected"` ≤15 s more; proceeds with a warning if
//!    WhatsApp is still connecting),
//! 6. performs the staleness handshake on reuse: a running bridge is
//!    only adopted when its `/health` `scriptHash` matches the on-disk
//!    bridge.js hash and `sendReadReceipts` matches config.

use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Embedded bridge source (repo `scripts/whatsapp-bridge/bridge.js`).
pub fn bridge_script_source() -> &'static str {
    include_str!("../scripts/whatsapp-bridge/bridge.js")
}

/// Embedded `package.json` for the bridge.
pub fn bridge_package_source() -> &'static str {
    include_str!("../scripts/whatsapp-bridge/package.json")
}

/// First 16 hex chars of SHA-256 (hermes `_file_content_hash`).
pub fn hash16_bytes(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    let full: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    full[..16].to_string()
}

/// File content hash; `""` when unreadable (hermes semantics).
pub fn file_hash16(path: &Path) -> String {
    match std::fs::read(path) {
        Ok(bytes) => hash16_bytes(&bytes),
        Err(_) => String::new(),
    }
}

/// Resolve (and sync) the bridge directory under `<home>` (hermes
/// `resolve_whatsapp_bridge_dir`). Embedded sources are written when
/// missing or when the on-disk copy differs (post-update), preserving
/// any user-edited `node_modules`.
pub fn resolve_bridge_dir(home: &Path) -> PathBuf {
    let dir = home.join("scripts").join("whatsapp-bridge");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("[whatsapp] cannot create bridge dir {}: {e}", dir.display());
        return dir;
    }
    for (name, source) in [
        ("bridge.js", bridge_script_source()),
        ("package.json", bridge_package_source()),
    ] {
        let target = dir.join(name);
        let needs_write = match std::fs::read(&target) {
            Ok(existing) => hash16_bytes(&existing) != hash16_bytes(source.as_bytes()),
            Err(_) => true,
        };
        if needs_write {
            if let Err(e) = std::fs::write(&target, source) {
                eprintln!("[whatsapp] cannot sync {name} to {}: {e}", target.display());
            }
        }
    }
    dir
}

/// Locate a Node.js tool (`node`/`npm`) on PATH (hermes
/// `find_node_executable_on_path`; ulnclaw targets Linux, no managed
/// Node tree).
pub fn find_node_executable(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn package_hash_path(bridge_dir: &Path) -> PathBuf {
    bridge_dir.join("node_modules").join(".hermes-pkg-hash")
}

/// True when `node_modules` exists and its stamp matches `package.json`
/// (hermes `_deps_fresh` check).
pub fn deps_fresh(bridge_dir: &Path) -> bool {
    if !bridge_dir.join("node_modules").exists() {
        return false;
    }
    let pkg_hash = file_hash16(&bridge_dir.join("package.json"));
    if pkg_hash.is_empty() {
        return false;
    }
    match std::fs::read_to_string(package_hash_path(bridge_dir)) {
        Ok(stamp) => stamp.trim() == pkg_hash,
        Err(_) => false,
    }
}

/// Run `npm install --silent` in the bridge dir when deps are stale and
/// write the hash stamp on success (hermes auto-install). Timeout via
/// `WHATSAPP_NPM_INSTALL_TIMEOUT` seconds (default 300).
pub async fn ensure_dependencies(bridge_dir: &Path) -> Result<(), String> {
    if deps_fresh(bridge_dir) {
        return Ok(());
    }
    let npm = find_node_executable("npm").ok_or_else(|| "npm not found on PATH".to_string())?;
    eprintln!(
        "[whatsapp] installing bridge dependencies in {}",
        bridge_dir.display()
    );
    let timeout_secs = std::env::var("WHATSAPP_NPM_INSTALL_TIMEOUT")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(300);
    let output = tokio::time::timeout(
        Duration::from_secs(timeout_secs),
        tokio::process::Command::new(&npm)
            .args(["install", "--silent"])
            .current_dir(bridge_dir)
            .stdin(std::process::Stdio::null())
            .output(),
    )
    .await
    .map_err(|_| format!("npm install timed out after {timeout_secs}s"))?
    .map_err(|e| format!("npm install failed to start: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("npm install failed: {}", stderr.trim()));
    }
    let pkg_hash = file_hash16(&bridge_dir.join("package.json"));
    if !pkg_hash.is_empty() {
        let _ = std::fs::write(package_hash_path(bridge_dir), &pkg_hash);
    }
    eprintln!("[whatsapp] bridge dependencies installed");
    Ok(())
}

// ---------------------------------------------------------------------------
// PID identity helpers (hermes `gateway/status.py` + `_bridge_pid_is_ours`)
// ---------------------------------------------------------------------------

/// Alive check that never signals the target (`kill(pid, 0)`).
pub fn pid_exists(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    crate::process_ctl::alive(pid as u32)
}

/// Kernel start time (clock ticks since boot, field 22 of
/// `/proc/<pid>/stat`) — the definitive identity for a PID.
pub fn pid_start_time(pid: i32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm (field 2) may contain spaces and parens — split after the
    // last ')'. Fields after it start at 3 (state).
    let rest = stat.rsplit_once(')').map(|(_, r)| r)?;
    let tokens: Vec<&str> = rest.split_whitespace().collect();
    tokens.get(22 - 3)?.parse::<u64>().ok()
}

/// Space-joined command line (`/proc/<pid>/cmdline`).
pub fn read_process_cmdline(pid: i32) -> Option<String> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    if raw.is_empty() {
        return None;
    }
    let text = raw
        .split(|b| *b == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect::<Vec<_>>()
        .join(" ");
    let text = text.trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// True only if `pid` is alive AND still our node bridge for this
/// session (hermes `_bridge_pid_is_ours`): start-time match when the
/// pidfile recorded one, otherwise the cmdline must contain `node` and
/// the session path. A recycled PID is never ours.
pub fn bridge_pid_is_ours(pid: i32, session_path: &Path, expected_start: Option<u64>) -> bool {
    if !pid_exists(pid) {
        return false;
    }
    if let Some(expected) = expected_start {
        return pid_start_time(pid) == Some(expected);
    }
    match read_process_cmdline(pid) {
        Some(cmdline) => {
            cmdline.contains("node") && cmdline.contains(session_path.to_string_lossy().as_ref())
        }
        None => false,
    }
}

/// Parse `bridge.pid`: line 1 = pid, optional line 2 = kernel start
/// time (legacy files have only the pid).
pub fn read_bridge_pidfile(session_dir: &Path) -> Option<(i32, Option<u64>)> {
    let text = std::fs::read_to_string(session_dir.join("bridge.pid")).ok()?;
    let mut lines = text.lines();
    let pid = lines.next()?.trim().parse::<i32>().ok()?;
    let start = lines
        .next()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .and_then(|line| line.parse::<u64>().ok());
    Some((pid, start))
}

/// Write pid + kernel start time (hermes `_write_bridge_pidfile`).
pub fn write_bridge_pidfile(session_dir: &Path, pid: i32) {
    let text = match pid_start_time(pid) {
        Some(start) => format!("{pid}\n{start}"),
        None => format!("{pid}"),
    };
    let _ = std::fs::write(session_dir.join("bridge.pid"), text);
}

/// Kill a bridge recorded in a previous run's pidfile — only after
/// re-validating the PID still names our bridge (hermes
/// `_kill_stale_bridge_by_pidfile`).
pub fn kill_stale_bridge_by_pidfile(session_dir: &Path) {
    let pid_file = session_dir.join("bridge.pid");
    if !pid_file.exists() {
        return;
    }
    match read_bridge_pidfile(session_dir) {
        Some((pid, start)) => {
            if bridge_pid_is_ours(pid, session_dir, start) {
                let _ = crate::process_ctl::terminate(pid as u32);
                eprintln!("[whatsapp] killed stale bridge pid {pid} from pidfile");
            } else if pid_exists(pid) {
                eprintln!(
                    "[whatsapp] not killing pidfile pid {pid}: recycled onto an unrelated process; skipping"
                );
            }
        }
        None => {}
    }
    let _ = std::fs::remove_file(&pid_file);
}

// ---------------------------------------------------------------------------
// Port occupant cleanup (hermes `_kill_port_process`, LISTEN-only)
// ---------------------------------------------------------------------------

/// PIDs from `lsof -ti tcp:PORT -sTCP:LISTEN` output.
pub fn parse_lsof_pids(output: &str) -> Vec<i32> {
    output
        .lines()
        .filter_map(|line| line.trim().parse::<i32>().ok())
        .collect()
}

/// PIDs from `ss -ltnHp "sport = :PORT"` output (`users:(("node",pid=123,...))`).
pub fn parse_ss_listener_pids(output: &str) -> Vec<i32> {
    let mut pids = Vec::new();
    for (index, _) in output.match_indices("pid=") {
        let tail = &output[index + 4..];
        let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(pid) = digits.parse::<i32>() {
            if !pids.contains(&pid) {
                pids.push(pid);
            }
        }
    }
    pids
}

/// SIGTERM any process LISTENING on the port (never clients; hermes
/// `_kill_port_process`). lsof first, `ss` fallback.
pub fn kill_port_process(port: u16) {
    let mut pids = Vec::new();
    if let Ok(output) = std::process::Command::new("lsof")
        .args(["-ti", &format!("tcp:{port}"), "-sTCP:LISTEN"])
        .output()
    {
        pids = parse_lsof_pids(&String::from_utf8_lossy(&output.stdout));
    }
    if pids.is_empty() {
        if let Ok(output) = std::process::Command::new("ss")
            .args(["-ltnHp", &format!("sport = :{port}")])
            .output()
        {
            pids = parse_ss_listener_pids(&String::from_utf8_lossy(&output.stdout));
        }
    }
    for pid in pids {
        let _ = crate::process_ctl::terminate(pid as u32);
        eprintln!("[whatsapp] terminated stale listener pid {pid} on port {port}");
    }
}

// ---------------------------------------------------------------------------
// Spawn (hermes `connect`: argv + env + bridge.log + readiness wait)
// ---------------------------------------------------------------------------

/// WHATSAPP_* env vars forwarded verbatim to the bridge child (hermes
/// `connect` forwarding list).
pub const FORWARDED_ENV: &[&str] = &[
    "WHATSAPP_ALLOWED_USERS",
    "WHATSAPP_ALLOW_FROM",
    "WHATSAPP_DM_POLICY",
    "WHATSAPP_GROUP_POLICY",
    "WHATSAPP_GROUP_ALLOWED_USERS",
    "WHATSAPP_GROUP_ALLOW_FROM",
    "WHATSAPP_REQUIRE_MENTION",
    "WHATSAPP_MENTION_PATTERNS",
    "WHATSAPP_FREE_RESPONSE_CHATS",
    "WHATSAPP_DEBUG",
    "WHATSAPP_FORWARD_OWNER_MESSAGES",
    "WHATSAPP_REPLY_PREFIX",
    "WHATSAPP_MAX_MESSAGE_LENGTH",
    "WHATSAPP_CHUNK_DELAY_MS",
    "WHATSAPP_SEND_TIMEOUT_MS",
];

/// Spawn parameters (subset of `[messaging.whatsapp]` resolved config).
#[derive(Debug, Clone)]
pub struct BridgeSpawnConfig {
    pub port: u16,
    pub session_path: PathBuf,
    pub mode: String,
    pub read_receipts: bool,
    pub media_cache_dir: PathBuf,
}

/// Handle to the supervised bridge child.
pub struct BridgeProcess {
    pub child: tokio::process::Child,
    pub pid: i32,
    pub log_path: PathBuf,
}

impl BridgeProcess {
    /// True once the child has exited (hermes `_check_managed_bridge_exit`
    /// polling).
    pub fn exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }
}

/// Build the bridge command (hermes argv `node bridge.js --port P
/// --session S --mode M` + env). stdout/stderr are attached by
/// `spawn_bridge`.
pub fn build_bridge_command(
    node: &Path,
    script: &Path,
    cfg: &BridgeSpawnConfig,
) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(node);
    cmd.args([
        script.to_string_lossy().as_ref(),
        "--port",
        &cfg.port.to_string(),
        "--session",
        &cfg.session_path.to_string_lossy(),
        "--mode",
        &cfg.mode,
    ]);
    cmd.env(
        "WHATSAPP_SEND_READ_RECEIPTS",
        if cfg.read_receipts { "true" } else { "false" },
    );
    cmd.env("WHATSAPP_MODE", &cfg.mode);
    cmd.env("ULNCLAW_MEDIA_CACHE_DIR", &cfg.media_cache_dir);
    for key in FORWARDED_ENV {
        if let Ok(value) = std::env::var(key) {
            cmd.env(key, value);
        }
    }
    cmd.stdin(std::process::Stdio::null());
    cmd
}

/// Spawn the bridge child, append output to `<session>/../bridge.log`,
/// and write the pidfile (hermes `connect` spawn block).
pub fn spawn_bridge(
    node: &Path,
    script: &Path,
    cfg: &BridgeSpawnConfig,
) -> Result<BridgeProcess, String> {
    if let Err(e) = std::fs::create_dir_all(&cfg.session_path) {
        return Err(format!("cannot create session dir: {e}"));
    }
    let log_path = cfg
        .session_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("bridge.log");
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| format!("cannot open bridge log {}: {e}", log_path.display()))?;
    let stderr = log_file
        .try_clone()
        .map_err(|e| format!("cannot clone bridge log handle: {e}"))?;
    let mut cmd = build_bridge_command(node, script, cfg);
    cmd.stdout(std::process::Stdio::from(log_file));
    cmd.stderr(std::process::Stdio::from(stderr));
    let child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn node bridge: {e}"))?;
    let pid = child.id().unwrap_or(0) as i32;
    write_bridge_pidfile(&cfg.session_path, pid);
    eprintln!(
        "[whatsapp] spawned bridge pid={pid} port={} session={} log={}",
        cfg.port,
        cfg.session_path.display(),
        log_path.display()
    );
    Ok(BridgeProcess {
        child,
        pid,
        log_path,
    })
}

/// Two-phase readiness wait (hermes `connect`): HTTP up within 15×1 s
/// (aborting if the child exits), then `status == "connected"` within
/// 15 more probes. Returns the last `/health` payload; a still-connecting
/// WhatsApp is not fatal (the bridge auto-reconnects).
pub async fn wait_until_ready(
    client: &reqwest::Client,
    base_url: &str,
    bridge: &mut BridgeProcess,
) -> Result<Value, String> {
    let url = format!("{base_url}/health");
    let mut data = serde_json::json!({});
    let mut http_ready = false;
    for _ in 0..15 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if bridge.exited() {
            return Err(format!(
                "bridge process died during startup — check log {}",
                bridge.log_path.display()
            ));
        }
        if let Ok(resp) = client.get(&url).send().await {
            if resp.status().is_success() {
                http_ready = true;
                data = resp.json().await.unwrap_or_else(|_| serde_json::json!({}));
                if data.get("status").and_then(|v| v.as_str()) == Some("connected") {
                    eprintln!("[whatsapp] bridge ready (status: connected)");
                    return Ok(data);
                }
                break;
            }
        }
    }
    if !http_ready {
        return Err(format!(
            "bridge HTTP server did not start in 15s — check log {}",
            bridge.log_path.display()
        ));
    }
    eprintln!("[whatsapp] bridge HTTP ready, waiting for WhatsApp connection...");
    for _ in 0..15 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if bridge.exited() {
            return Err(format!(
                "bridge process died during connection — check log {}",
                bridge.log_path.display()
            ));
        }
        if let Ok(resp) = client.get(&url).send().await {
            if resp.status().is_success() {
                data = resp.json().await.unwrap_or_else(|_| serde_json::json!({}));
                if data.get("status").and_then(|v| v.as_str()) == Some("connected") {
                    eprintln!("[whatsapp] bridge ready (status: connected)");
                    return Ok(data);
                }
            }
        }
    }
    eprintln!(
        "[whatsapp] bridge not connected after 30s (status: {}) — proceeding, it may reconnect; check log {}",
        data.get("status").and_then(|v| v.as_str()).unwrap_or("unknown"),
        bridge.log_path.display()
    );
    Ok(data)
}

/// Staleness handshake (hermes reuse check): reason to restart a
/// running bridge, or `None` when it may be adopted. Old bridges that
/// don't report `scriptHash` are stale by definition.
pub fn adoption_blocker(health: &Value, script_path: &Path, read_receipts: bool) -> Option<String> {
    let running_hash = health
        .get("scriptHash")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let disk_hash = file_hash16(script_path);
    if running_hash.is_empty() || disk_hash.is_empty() || running_hash != disk_hash {
        return Some(format!(
            "script hash mismatch (running={}, disk={})",
            if running_hash.is_empty() {
                "unversioned"
            } else {
                running_hash
            },
            disk_hash
        ));
    }
    let running_receipts = health
        .get("sendReadReceipts")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if running_receipts != read_receipts {
        return Some("send_read_receipts config changed".to_string());
    }
    None
}

/// Stop the bridge child: SIGTERM, 5 s grace, SIGKILL (hermes
/// `_terminate_bridge_process`).
pub async fn stop_bridge(mut bridge: BridgeProcess) {
    if bridge.pid > 0 {
        let _ = crate::process_ctl::terminate(bridge.pid as u32);
    }
    match tokio::time::timeout(Duration::from_secs(5), bridge.child.wait()).await {
        Ok(status) => {
            eprintln!(
                "[whatsapp] bridge pid {} terminated (status={:?})",
                bridge.pid, status
            );
        }
        Err(_) => {
            let _ = bridge.child.start_kill();
            eprintln!("[whatsapp] bridge pid {} killed after timeout", bridge.pid);
        }
    }
}

/// Extract `host:port` from an `http://host:port[/...]` bridge URL.
pub fn parse_bridge_url_host_port(url: &str) -> Option<(String, u16)> {
    let trimmed = url.trim().trim_start_matches("http://");
    let trimmed = trimmed.trim_start_matches("https://");
    let authority = trimmed.split('/').next().unwrap_or("");
    let (host, port_part) = authority.rsplit_once(':')?;
    let port = port_part.parse::<u16>().ok()?;
    if host.is_empty() {
        return None;
    }
    Some((host.to_string(), port))
}

/// True when the configured bridge URL targets the local bundled bridge
/// port (auto-spawn eligibility; remote bridge URLs stay external).
pub fn is_local_bridge_target(bridge_url: &str, bridge_port: u16) -> bool {
    match parse_bridge_url_host_port(bridge_url) {
        Some((host, port)) => {
            port == bridge_port && matches!(host.as_str(), "127.0.0.1" | "localhost" | "::1")
        }
        None => false,
    }
}

/// Full supervision pass (hermes `connect`): node resolution, bridge
/// sync, stale cleanup, npm install, spawn, readiness wait. Returns the
/// supervised child, or `None` when no child is managed by us (node
/// missing, an adoptable bridge already running, or spawn failure — the
/// poll loop keeps retrying / using the external bridge).
pub async fn ensure_and_spawn(
    home: &Path,
    port: u16,
    session_path: &Path,
    mode: &str,
    read_receipts: bool,
    bridge_script_override: &str,
) -> Option<BridgeProcess> {
    let node = match find_node_executable("node") {
        Some(node) => node,
        None => {
            eprintln!(
                "[whatsapp] node not found on PATH — start a bridge externally or install Node.js"
            );
            return None;
        }
    };
    let script = if bridge_script_override.is_empty() {
        let dir = resolve_bridge_dir(home);
        dir.join("bridge.js")
    } else {
        PathBuf::from(bridge_script_override)
    };
    if !script.exists() {
        eprintln!("[whatsapp] bridge script missing at {}", script.display());
        return None;
    }
    // Pre-flight pairing notice (hermes checks creds.json and fails
    // fast; ulnclaw keeps the QR flow alive and points at the log).
    let creds = session_path.join("creds.json");
    if !creds.exists() {
        eprintln!(
            "[whatsapp] no session at {} — first start: scan the QR code printed in bridge.log / the gateway console",
            session_path.display()
        );
    }
    // Adopt a healthy, fresh bridge already serving the port.
    let base_url = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    if let Ok(resp) = client.get(format!("{base_url}/health")).send().await {
        if resp.status().is_success() {
            if let Ok(health) = resp.json::<Value>().await {
                let status = health
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                if status == "connected" {
                    match adoption_blocker(&health, &script, read_receipts) {
                        None => {
                            eprintln!("[whatsapp] adopting existing bridge (status: connected)");
                            return None;
                        }
                        Some(reason) => {
                            eprintln!("[whatsapp] running bridge is stale ({reason}); restarting");
                        }
                    }
                } else {
                    eprintln!(
                        "[whatsapp] bridge found but not connected (status: {status}); restarting"
                    );
                }
            }
        }
    }
    kill_stale_bridge_by_pidfile(session_path);
    kill_port_process(port);
    tokio::time::sleep(Duration::from_secs(1)).await;
    if let Err(e) = ensure_dependencies(script.parent().unwrap_or_else(|| Path::new("."))).await {
        eprintln!("[whatsapp] dependency install failed: {e}");
        return None;
    }
    let spawn_cfg = BridgeSpawnConfig {
        port,
        session_path: session_path.to_path_buf(),
        mode: mode.to_string(),
        read_receipts,
        media_cache_dir: home.join("media-cache"),
    };
    let mut bridge = match spawn_bridge(&node, &script, &spawn_cfg) {
        Ok(bridge) => bridge,
        Err(e) => {
            eprintln!("[whatsapp] bridge spawn failed: {e}");
            return None;
        }
    };
    match wait_until_ready(&client, &base_url, &mut bridge).await {
        Ok(_) => Some(bridge),
        Err(e) => {
            eprintln!("[whatsapp] {e}");
            let _ = bridge.child.start_kill();
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash16_matches_hermes_truncation() {
        // sha256("abc") = ba7816bf8f01cfea414140de5dae2223...
        assert_eq!(hash16_bytes(b"abc"), "ba7816bf8f01cfea");
        assert_eq!(hash16_bytes(b"").len(), 16);
    }

    #[test]
    fn file_hash16_missing_is_empty() {
        assert_eq!(file_hash16(Path::new("/nonexistent/xyz")), "");
    }

    #[test]
    fn embedded_sources_are_present_and_stable() {
        assert!(bridge_script_source().contains("/health"));
        assert!(bridge_script_source().contains("scriptHash"));
        assert!(bridge_package_source().contains("@whiskeysockets/baileys"));
        assert_eq!(hash16_bytes(bridge_script_source().as_bytes()).len(), 16);
    }

    #[test]
    fn resolve_bridge_dir_syncs_embedded_sources() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = resolve_bridge_dir(temp.path());
        let script = dir.join("bridge.js");
        let pkg = dir.join("package.json");
        assert_eq!(
            std::fs::read_to_string(&script).unwrap(),
            bridge_script_source()
        );
        assert_eq!(
            std::fs::read_to_string(&pkg).unwrap(),
            bridge_package_source()
        );
        // A drifted on-disk copy is restored on the next resolve.
        std::fs::write(&script, "tampered").unwrap();
        resolve_bridge_dir(temp.path());
        assert_eq!(
            std::fs::read_to_string(&script).unwrap(),
            bridge_script_source()
        );
    }

    #[test]
    fn deps_fresh_stamp_semantics() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path();
        std::fs::write(dir.join("package.json"), r#"{"name":"x"}"#).unwrap();
        assert!(!deps_fresh(dir)); // no node_modules
        let nm = dir.join("node_modules");
        std::fs::create_dir_all(&nm).unwrap();
        assert!(!deps_fresh(dir)); // no stamp
        let stamp = nm.join(".hermes-pkg-hash");
        std::fs::write(&stamp, "bogus").unwrap();
        assert!(!deps_fresh(dir)); // stamp mismatch
        std::fs::write(&stamp, file_hash16(&dir.join("package.json"))).unwrap();
        assert!(deps_fresh(dir)); // stamp matches
        std::fs::write(dir.join("package.json"), r#"{"name":"y"}"#).unwrap();
        assert!(!deps_fresh(dir)); // package.json changed
    }

    #[test]
    fn pidfile_roundtrip_and_legacy() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path();
        assert!(read_bridge_pidfile(dir).is_none());
        write_bridge_pidfile(dir, 4242);
        let (pid, start) = read_bridge_pidfile(dir).unwrap();
        assert_eq!(pid, 4242);
        // Written for a live PID (this test process) → start time present.
        let self_pid = std::process::id() as i32;
        write_bridge_pidfile(dir, self_pid);
        let (pid2, start2) = read_bridge_pidfile(dir).unwrap();
        assert_eq!(pid2, self_pid);
        assert_eq!(start2, pid_start_time(self_pid));
        // Legacy single-line file still parses.
        std::fs::write(dir.join("bridge.pid"), "999\n").unwrap();
        assert_eq!(read_bridge_pidfile(dir), Some((999, None)));
        // Corrupt file → None.
        std::fs::write(dir.join("bridge.pid"), "not-a-pid\n").unwrap();
        assert!(read_bridge_pidfile(dir).is_none());
    }

    #[test]
    fn pid_identity_checks() {
        let self_pid = std::process::id() as i32;
        assert!(pid_exists(self_pid));
        assert!(!pid_exists(0));
        assert!(!pid_exists(-5));
        assert!(pid_start_time(self_pid).is_some());
        let cmdline = read_process_cmdline(self_pid).unwrap();
        assert!(!cmdline.is_empty());
        let session = PathBuf::from("/tmp/some-session");
        // Correct start time → ours.
        let start = pid_start_time(self_pid);
        assert!(bridge_pid_is_ours(self_pid, &session, start));
        // Wrong start time → recycled → not ours.
        assert!(!bridge_pid_is_ours(
            self_pid,
            &session,
            start.map(|s| s.wrapping_add(1))
        ));
        // Legacy path: cmdline lacks "node" + session → not ours.
        assert!(!bridge_pid_is_ours(self_pid, &session, None));
    }

    #[test]
    fn stale_pidfile_kill_skips_recycled_pid() {
        // The current process pid stands in for a recycled PID: the
        // pidfile claims it, but start-time identity fails (we record a
        // bogus start), so nothing is killed and the file is removed.
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().to_path_buf();
        let self_pid = std::process::id() as i32;
        std::fs::write(dir.join("bridge.pid"), format!("{self_pid}\n999999999")).unwrap();
        kill_stale_bridge_by_pidfile(&dir);
        assert!(pid_exists(self_pid)); // untouched
        assert!(!dir.join("bridge.pid").exists());
    }

    #[test]
    fn kill_stale_pidfile_missing_is_noop() {
        let temp = tempfile::tempdir().expect("tempdir");
        kill_stale_bridge_by_pidfile(temp.path()); // must not panic
    }

    #[test]
    fn parse_lsof_output() {
        assert_eq!(parse_lsof_pids("1234\n5678\n"), vec![1234, 5678]);
        assert_eq!(parse_lsof_pids(""), Vec::<i32>::new());
        assert_eq!(parse_lsof_pids("junk\n42\n"), vec![42]);
    }

    #[test]
    fn parse_ss_output() {
        let sample = "State Recv-Q Send-Q Local Address:Port Peer Address:Port Process\n\
                      LISTEN 0 511 127.0.0.1:3000 0.0.0.0:* users:((\"node\",pid=4321,fd=18))\n";
        assert_eq!(parse_ss_listener_pids(sample), vec![4321]);
        let multi = "a pid=10,x pid=10,y pid=20,z";
        assert_eq!(parse_ss_listener_pids(multi), vec![10, 20]);
        assert_eq!(parse_ss_listener_pids("no listeners"), Vec::<i32>::new());
    }

    #[test]
    fn bridge_command_matches_hermes_argv() {
        let _guard = crate::models_dev::test_env_lock();
        std::env::remove_var("WHATSAPP_DEBUG");
        std::env::set_var("WHATSAPP_FORWARD_OWNER_MESSAGES", "true");
        let cfg = BridgeSpawnConfig {
            port: 3001,
            session_path: PathBuf::from("/tmp/wa-session"),
            mode: "self-chat".to_string(),
            read_receipts: true,
            media_cache_dir: PathBuf::from("/tmp/media-cache"),
        };
        let cmd = build_bridge_command(Path::new("/usr/bin/node"), Path::new("/b/bridge.js"), &cfg);
        let std_cmd = cmd.as_std();
        let args: Vec<String> = std_cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(
            args,
            vec![
                "/b/bridge.js",
                "--port",
                "3001",
                "--session",
                "/tmp/wa-session",
                "--mode",
                "self-chat",
            ]
        );
        let env = |key: &str| {
            std_cmd
                .get_envs()
                .find(|(k, _)| *k == key)
                .and_then(|(_, v)| v.map(|s| s.to_string_lossy().to_string()))
        };
        assert_eq!(env("WHATSAPP_SEND_READ_RECEIPTS").as_deref(), Some("true"));
        assert_eq!(env("WHATSAPP_MODE").as_deref(), Some("self-chat"));
        assert_eq!(
            env("ULNCLAW_MEDIA_CACHE_DIR").as_deref(),
            Some("/tmp/media-cache")
        );
        assert_eq!(
            env("WHATSAPP_FORWARD_OWNER_MESSAGES").as_deref(),
            Some("true")
        );
        assert!(env("WHATSAPP_DEBUG").is_none()); // unset → not injected
        std::env::remove_var("WHATSAPP_FORWARD_OWNER_MESSAGES");
    }

    #[test]
    fn parse_bridge_url_host_port_variants() {
        assert_eq!(
            parse_bridge_url_host_port("http://127.0.0.1:3000"),
            Some(("127.0.0.1".into(), 3000))
        );
        assert_eq!(
            parse_bridge_url_host_port("http://127.0.0.1:3000/"),
            Some(("127.0.0.1".into(), 3000))
        );
        assert_eq!(
            parse_bridge_url_host_port("http://10.0.0.5:8080/base"),
            Some(("10.0.0.5".into(), 8080))
        );
        assert_eq!(parse_bridge_url_host_port("http://127.0.0.1"), None);
        assert_eq!(parse_bridge_url_host_port(""), None);
    }

    #[test]
    fn local_bridge_target_detection() {
        assert!(is_local_bridge_target("http://127.0.0.1:3000", 3000));
        assert!(is_local_bridge_target("http://localhost:3000/", 3000));
        assert!(!is_local_bridge_target("http://127.0.0.1:3000", 3001));
        assert!(!is_local_bridge_target("http://10.0.0.5:3000", 3000));
        assert!(!is_local_bridge_target("not-a-url", 3000));
    }

    #[test]
    fn adoption_blocker_semantics() {
        let temp = tempfile::tempdir().expect("tempdir");
        let script = temp.path().join("bridge.js");
        std::fs::write(&script, "console.log('v1');").unwrap();
        let disk_hash = file_hash16(&script);
        let fresh = serde_json::json!({
            "status": "connected",
            "scriptHash": disk_hash,
            "sendReadReceipts": true,
        });
        assert!(adoption_blocker(&fresh, &script, true).is_none());
        // Config mismatch blocks adoption.
        assert!(adoption_blocker(&fresh, &script, false).is_some());
        // Hash mismatch blocks adoption.
        let stale = serde_json::json!({"scriptHash": "0000000000000000", "sendReadReceipts": true});
        assert!(adoption_blocker(&stale, &script, true).is_some());
        // Unversioned bridge is stale by definition.
        let unversioned = serde_json::json!({"sendReadReceipts": true});
        assert!(adoption_blocker(&unversioned, &script, true).is_some());
        // Missing script on disk blocks adoption too.
        assert!(adoption_blocker(&fresh, Path::new("/nonexistent/bridge.js"), true).is_some());
    }

    #[tokio::test]
    async fn spawn_and_stop_fake_node_bridge() {
        let _guard = crate::models_dev::test_env_lock();
        let temp = tempfile::tempdir().expect("tempdir");
        let bin_dir = temp.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let fake_node = bin_dir.join("node");
        std::fs::write(&fake_node, "#!/bin/sh\nsleep 30\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake_node, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let saved_path = std::env::var_os("PATH");
        std::env::set_var("PATH", &bin_dir);
        let resolved = find_node_executable("node");
        assert_eq!(resolved, Some(fake_node.clone()));

        let session = temp.path().join("session");
        let cfg = BridgeSpawnConfig {
            port: 3999,
            session_path: session.clone(),
            mode: "self-chat".to_string(),
            read_receipts: false,
            media_cache_dir: temp.path().join("media-cache"),
        };
        let script = temp.path().join("bridge.js");
        std::fs::write(&script, "// fake").unwrap();
        let mut bridge = spawn_bridge(&fake_node, &script, &cfg).expect("spawn");
        assert!(bridge.pid > 0);
        assert!(!bridge.exited());
        // Pidfile carries pid + start time.
        let (pid, start) = read_bridge_pidfile(&session).unwrap();
        assert_eq!(pid, bridge.pid);
        assert_eq!(start, pid_start_time(bridge.pid));
        // Log file created.
        assert!(bridge.log_path.exists());
        stop_bridge(bridge).await;
        match saved_path {
            Some(p) => std::env::set_var("PATH", p),
            None => std::env::remove_var("PATH"),
        }
    }
}

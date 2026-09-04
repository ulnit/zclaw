//! OSV malware preflight for MCP servers — port of hermes'
//! `tools/osv_check.py`.
//!
//! Before launching an MCP server via `npx`/`uvx`/`pipx`, the package is
//! checked against the OSV (Open Source Vulnerabilities) API for known
//! *malware* advisories (`MAL-*` ids). Regular CVEs are ignored — only
//! confirmed malware blocks the launch.
//!
//! The API is free, public, and maintained by Google. Fail-open: network
//! errors allow the package to proceed (and are deliberately not cached).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const DEFAULT_ENDPOINT: &str = "https://api.osv.dev/v1/query";
const DEFAULT_CACHE_TTL_SECS: u64 = 3600;
const CACHE_MAX_ENTRIES: usize = 256;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

type CacheKey = (String, String, Option<String>);
type CacheEntry = (Instant, Option<String>);

fn cache() -> &'static Mutex<HashMap<CacheKey, CacheEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<CacheKey, CacheEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cache_ttl() -> Duration {
    let seconds = std::env::var("OSV_CHECK_CACHE_TTL")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_CACHE_TTL_SECS);
    Duration::from_secs(seconds)
}

fn osv_endpoint() -> String {
    std::env::var("OSV_ENDPOINT")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_ENDPOINT.to_string())
}

fn cache_get(key: &CacheKey) -> (bool, Option<String>) {
    let mut cache = cache().lock().expect("osv cache lock");
    match cache.get(key) {
        Some((expiry, result)) => {
            if Instant::now() >= *expiry {
                cache.remove(key);
                (false, None)
            } else {
                (true, result.clone())
            }
        }
        None => (false, None),
    }
}

fn cache_put(key: CacheKey, result: Option<String>) {
    let mut cache = cache().lock().expect("osv cache lock");
    if cache.len() >= CACHE_MAX_ENTRIES {
        let now = Instant::now();
        cache.retain(|_, (expiry, _)| *expiry > now);
        if cache.len() >= CACHE_MAX_ENTRIES {
            cache.clear();
        }
    }
    cache.insert(key, (Instant::now() + cache_ttl(), result));
}

/// Infer the package ecosystem from the launcher command
/// (hermes `_infer_ecosystem`).
pub fn infer_ecosystem(command: &str) -> Option<&'static str> {
    let base = std::path::Path::new(command)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(command)
        .to_lowercase();
    match base.as_str() {
        "npx" | "npx.cmd" => Some("npm"),
        "uvx" | "uvx.cmd" | "pipx" => Some("PyPI"),
        _ => None,
    }
}

/// Parse an npm package token: `@scope/name@version` or `name@version`
/// (hermes `_parse_npm_package`; `@latest` is treated as unpinned).
pub fn parse_npm_package(token: &str) -> (String, Option<String>) {
    if let Some(stripped) = token.strip_prefix('@') {
        // Scoped: @scope/name[@version] — the version separator is the '@'
        // *after* the scope slash.
        if let Some(slash) = stripped.find('/') {
            let rest = &stripped[slash + 1..];
            if let Some(at) = rest.find('@') {
                let name = format!("@{}", &stripped[..slash + 1 + at]);
                let version = &rest[at + 1..];
                if version.is_empty() || version == "latest" {
                    return (name, None);
                }
                return (name, Some(version.to_string()));
            }
        }
        return (token.to_string(), None);
    }
    if let Some(at) = token.rfind('@') {
        if at > 0 {
            let name = &token[..at];
            let version = &token[at + 1..];
            if version.is_empty() || version == "latest" {
                return (name.to_string(), None);
            }
            return (name.to_string(), Some(version.to_string()));
        }
    }
    (token.to_string(), None)
}

/// Parse a PyPI token: `name==version`, extras (`name[ext]==v`) stripped
/// (hermes `_parse_pypi_package`).
pub fn parse_pypi_package(token: &str) -> (String, Option<String>) {
    let without_extras = match token.find('[') {
        Some(open) => match token.find(']') {
            Some(close) if close > open => {
                format!("{}{}", &token[..open], &token[close + 1..])
            }
            _ => token.to_string(),
        },
        None => token.to_string(),
    };
    match without_extras.split_once("==") {
        Some((name, version)) if !name.is_empty() && !version.is_empty() => {
            (name.to_string(), Some(version.to_string()))
        }
        _ => (without_extras, None),
    }
}

/// Extract `(package, version)` from launcher args, honoring npx's
/// `--package`/`-p` install-target flags (hermes `_parse_package_from_args`).
pub fn parse_package_from_args(
    args: &[String],
    ecosystem: &str,
) -> Option<(String, Option<String>)> {
    let mut package_token: Option<&str> = None;
    let mut take_next = false;
    for arg in args {
        if take_next {
            package_token = Some(arg.as_str());
            break;
        }
        if arg == "--package" || arg == "-p" {
            take_next = true;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--package=") {
            package_token = Some(value);
            break;
        }
        if arg.starts_with('-') {
            continue;
        }
        package_token = Some(arg.as_str());
        break;
    }
    let token = package_token?;
    if token.is_empty() {
        return None;
    }
    let (name, version) = match ecosystem {
        "npm" => parse_npm_package(token),
        "PyPI" => parse_pypi_package(token),
        _ => (token.to_string(), None),
    };
    if name.is_empty() {
        return None;
    }
    Some((name, version))
}

/// Query the OSV API and return the malware (`MAL-*`) advisories
/// (hermes `_query_osv`).
async fn query_osv(
    endpoint: &str,
    package: &str,
    ecosystem: &str,
    version: Option<&str>,
) -> Result<Vec<(String, String)>, String> {
    let mut payload = serde_json::json!({
        "package": { "name": package, "ecosystem": ecosystem },
    });
    if let Some(version) = version {
        payload["version"] = serde_json::Value::String(version.to_string());
    }
    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())?;
    let response = client
        .post(endpoint)
        .header("User-Agent", "ulnclaw-osv-check/1.0")
        .json(&payload)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("OSV API {}", response.status()));
    }
    let body: serde_json::Value = response.json().await.map_err(|e| e.to_string())?;
    let vulns = body
        .get("vulns")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(vulns
        .iter()
        .filter_map(|vuln| {
            let id = vuln.get("id")?.as_str()?.to_string();
            if !id.starts_with("MAL-") {
                return None;
            }
            let summary = vuln
                .get("summary")
                .and_then(|s| s.as_str())
                .unwrap_or(&id)
                .chars()
                .take(100)
                .collect::<String>();
            Some((id, summary))
        })
        .collect())
}

/// Check a launcher command for known-malware packages against an explicit
/// endpoint (test-friendly core of `check_package_for_malware`).
pub async fn check_with_endpoint(endpoint: &str, command: &str, args: &[String]) -> Option<String> {
    let ecosystem = infer_ecosystem(command)?;
    let (package, version) = parse_package_from_args(args, ecosystem)?;
    let cache_key: CacheKey = (ecosystem.to_string(), package.clone(), version.clone());
    let (hit, cached) = cache_get(&cache_key);
    if hit {
        return cached;
    }
    let malware = match query_osv(endpoint, &package, ecosystem, version.as_deref()).await {
        Ok(malware) => malware,
        Err(error) => {
            // Fail-open: network errors, timeouts, parse failures → allow.
            // Deliberately NOT cached (hermes).
            tracing::debug!(
                "OSV check failed for {}/{} (allowing): {}",
                ecosystem,
                package,
                error
            );
            return None;
        }
    };
    let result = if malware.is_empty() {
        None
    } else {
        let ids = malware
            .iter()
            .take(3)
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let summaries = malware
            .iter()
            .take(3)
            .map(|(_, summary)| summary.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        Some(format!(
            "BLOCKED: Package '{}' ({}) has known malware advisories: {}. Details: {}",
            package, ecosystem, ids, summaries
        ))
    };
    cache_put(cache_key, result.clone());
    result
}

/// Preflight for one MCP launcher command. Returns a block reason when the
/// package has known malware advisories; `None` means clean/unknown/fail-open.
pub async fn check_package_for_malware(command: &str, args: &[String]) -> Option<String> {
    check_with_endpoint(&osv_endpoint(), command, args).await
}

/// Test helper: drop all cached verdicts.
#[doc(hidden)]
pub fn clear_cache_for_tests() {
    cache().lock().expect("osv cache lock").clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ecosystem_inference() {
        assert_eq!(infer_ecosystem("npx"), Some("npm"));
        assert_eq!(infer_ecosystem("/usr/bin/npx"), Some("npm"));
        assert_eq!(infer_ecosystem("NPX.CMD"), Some("npm"));
        assert_eq!(infer_ecosystem("uvx"), Some("PyPI"));
        assert_eq!(infer_ecosystem("pipx"), Some("PyPI"));
        assert_eq!(infer_ecosystem("node"), None);
        assert_eq!(infer_ecosystem("python"), None);
    }

    #[test]
    fn npm_package_parsing() {
        assert_eq!(
            parse_npm_package("@modelcontextprotocol/server-filesystem"),
            ("@modelcontextprotocol/server-filesystem".into(), None)
        );
        assert_eq!(
            parse_npm_package("@scope/pkg@1.2.3"),
            ("@scope/pkg".into(), Some("1.2.3".into()))
        );
        assert_eq!(parse_npm_package("server@latest"), ("server".into(), None));
        assert_eq!(
            parse_npm_package("server@2.0.0"),
            ("server".into(), Some("2.0.0".into()))
        );
        assert_eq!(parse_npm_package("plain"), ("plain".into(), None));
    }

    #[test]
    fn pypi_package_parsing() {
        assert_eq!(
            parse_pypi_package("mcp-server"),
            ("mcp-server".into(), None)
        );
        assert_eq!(
            parse_pypi_package("mcp-server==0.9.1"),
            ("mcp-server".into(), Some("0.9.1".into()))
        );
        assert_eq!(
            parse_pypi_package("pkg[extra1,extra2]==1.0"),
            ("pkg".into(), Some("1.0".into()))
        );
    }

    #[test]
    fn arg_parsing_honors_package_flags() {
        let args: Vec<String> = ["-y", "@scope/server@1.0"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            parse_package_from_args(&args, "npm"),
            Some(("@scope/server".into(), Some("1.0".into())))
        );
        let args: Vec<String> = ["--package", "real-pkg", "fake-bin"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            parse_package_from_args(&args, "npm"),
            Some(("real-pkg".into(), None))
        );
        let args: Vec<String> = ["--package=other-pkg@2.0", "bin"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            parse_package_from_args(&args, "npm"),
            Some(("other-pkg".into(), Some("2.0".into())))
        );
        let args: Vec<String> = ["-p", "short-pkg"].iter().map(|s| s.to_string()).collect();
        assert_eq!(
            parse_package_from_args(&args, "npm"),
            Some(("short-pkg".into(), None))
        );
        let empty: Vec<String> = Vec::new();
        assert_eq!(parse_package_from_args(&empty, "npm"), None);
    }

    /// Minimal blocking HTTP mock for the OSV endpoint.
    fn spawn_osv_mock(response_body: &'static str) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            for stream in listener.incoming().take(4) {
                let Ok(mut stream) = stream else { continue };
                let mut buffer = [0u8; 4096];
                stream.read(&mut buffer).ok();
                let body = response_body;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(response.as_bytes()).ok();
            }
        });
        (endpoint, handle)
    }

    #[tokio::test]
    async fn malware_advisory_blocks() {
        clear_cache_for_tests();
        let (endpoint, handle) = spawn_osv_mock(
            r#"{"vulns":[{"id":"MAL-2025-9999","summary":"evil package"},{"id":"GHSA-xxxx","summary":"regular cve"}]}"#,
        );
        let args: Vec<String> = ["-y", "evil-test-pkg-a"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let result = check_with_endpoint(&endpoint, "npx", &args).await;
        let reason = result.expect("malware must block");
        assert!(reason.contains("BLOCKED"));
        assert!(reason.contains("MAL-2025-9999"));
        assert!(!reason.contains("GHSA"));
        // Cached verdict: second call must not need the network.
        drop(handle);
        let cached = check_with_endpoint("http://127.0.0.1:1", "npx", &args).await;
        assert!(cached.is_some());
    }

    #[tokio::test]
    async fn clean_package_allowed() {
        clear_cache_for_tests();
        let (endpoint, _handle) = spawn_osv_mock(r#"{"vulns":[]}"#);
        let args: Vec<String> = ["clean-test-pkg-b"].iter().map(|s| s.to_string()).collect();
        let result = check_with_endpoint(&endpoint, "npx", &args).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn network_failure_fails_open() {
        clear_cache_for_tests();
        // Unreachable endpoint → allow (fail-open), and nothing cached.
        let args: Vec<String> = ["failopen-test-pkg-c"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let result = check_with_endpoint("http://127.0.0.1:1", "uvx", &args).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn non_launcher_commands_skip() {
        let args: Vec<String> = ["anything"].iter().map(|s| s.to_string()).collect();
        assert!(check_with_endpoint("http://127.0.0.1:1", "node", &args)
            .await
            .is_none());
    }
}

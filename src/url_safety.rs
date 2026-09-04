//! URL safety — SSRF protection for model/user-provided URLs.
//!
//! Port of hermes `tools/url_safety.py` (v2026.8.3). Blocks requests to
//! private/internal network addresses so a malicious prompt or skill cannot
//! trick the agent into fetching internal resources (cloud metadata
//! endpoints like 169.254.169.254, localhost services, private hosts).
//!
//! The SSRF check can be disabled via `[security] allow_private_urls = true`
//! or `ULNCLAW_ALLOW_PRIVATE_URLS=true` (also accepts the hermes-compatible
//! `HERMES_ALLOW_PRIVATE_URLS`). Even when disabled, cloud metadata
//! hostnames/IPs are **always** blocked — they are never legitimate agent
//! targets.
//!
//! Differences vs hermes:
//! - DNS resolution uses tokio/std resolvers instead of `socket.getaddrinfo`
//!   (same semantics, fail-closed).
//! - The httpx transport-level guard (`create_ssrf_safe_client`) is expressed
//!   as a reqwest redirect policy that re-validates each redirect target
//!   (see `tools/builtin/web.rs`), covering the redirect-based bypass.
//! - IDNA host encoding is left to reqwest's URL parser.

use std::net::IpAddr;
use std::sync::OnceLock;

use regex::Regex;

/// Cloud metadata hostnames — always blocked regardless of DNS, routing or
/// the `allow_private_urls` toggle.
const BLOCKED_HOSTNAMES: &[&str] = &["metadata.google.internal", "metadata.goog"];

/// Cloud metadata / credential endpoint IPs — always blocked. IPv4-mapped
/// IPv6 variants included because resolvers may return `::ffff:x.x.x.x`.
const ALWAYS_BLOCKED_IP_STRS: &[&str] = &[
    "169.254.169.254", // AWS/GCP/Azure/DO/Oracle metadata
    "169.254.170.2",   // AWS ECS task metadata (task IAM creds)
    "169.254.169.253", // Azure IMDS wire server
    "fd00:ec2::254",   // AWS metadata (IPv6)
    "100.100.100.200", // Alibaba Cloud metadata
    "::ffff:169.254.169.254",
    "::ffff:169.254.170.2",
    "::ffff:169.254.169.253",
    "::ffff:100.100.100.200",
];

/// Always-blocked networks. After IPv4-mapped canonicalisation the hermes
/// `::ffff:169.254.0.0/112` entry is equivalent to `169.254.0.0/16`.
const ALWAYS_BLOCKED_NETWORKS: &[(&str, u32)] = &[("169.254.0.0", 16)];

/// Exact HTTPS hostnames allowed to resolve to private/benchmark-space IPs
/// (hermes: QQ media downloads behind local benchmark infrastructure).
const TRUSTED_PRIVATE_IP_HOSTS: &[&str] = &["multimedia.nt.qq.com.cn"];

/// Proxy env vars indicating DNS should be delegated to a proxy.
const PROXY_ENV_VARS: &[&str] = &[
    "HTTPS_PROXY",
    "https_proxy",
    "HTTP_PROXY",
    "http_proxy",
    "ALL_PROXY",
    "all_proxy",
];

/// Query parameter names that are unambiguously credential-bearing (hermes
/// `_SENSITIVE_QUERY_PARAM_NAMES` — deliberately narrow).
const SENSITIVE_QUERY_PARAM_NAMES: &[&str] = &[
    "access_token",
    "api_key",
    "apikey",
    "auth_token",
    "authorization",
    "awsaccesskeyid",
    "client_secret",
    "credential",
    "credentials",
    "jwt",
    "password",
    "passwd",
    "secret",
    "session_id",
    "signature",
    "token",
    "x_amz_security_token",
    "x_amz_signature",
    "x-amz-security-token",
    "x-amz-signature",
];

fn always_blocked_ips() -> &'static Vec<IpAddr> {
    static IPS: OnceLock<Vec<IpAddr>> = OnceLock::new();
    IPS.get_or_init(|| {
        ALWAYS_BLOCKED_IP_STRS
            .iter()
            .map(|s| s.parse::<IpAddr>().expect("static IP literal"))
            .collect()
    })
}

// ---------------------------------------------------------------------------
// Proxy detection
// ---------------------------------------------------------------------------

/// True when at least one HTTP proxy env var is set.
pub fn proxy_is_configured() -> bool {
    PROXY_ENV_VARS
        .iter()
        .any(|name| std::env::var(name).map(|v| !v.is_empty()).unwrap_or(false))
}

// ---------------------------------------------------------------------------
// Minimal URL splitting (hermes urlsplit semantics for http/https)
// ---------------------------------------------------------------------------

struct SplitUrl<'a> {
    scheme: &'a str,
    authority: &'a str,
    path: &'a str,
    query: Option<&'a str>,
    fragment: Option<&'a str>,
}

/// Split `scheme://authority/path?query#fragment` without pulling in a URL
/// crate. Returns None when there is no `scheme://` prefix.
fn split_url(url: &str) -> Option<SplitUrl<'_>> {
    let scheme_end = url.find("://")?;
    let (scheme, rest) = url.split_at(scheme_end);
    let rest = &rest[3..];
    let authority_end = rest
        .find(|c| c == '/' || c == '?' || c == '#')
        .unwrap_or(rest.len());
    let (authority, rest) = rest.split_at(authority_end);
    let (rest, fragment) = match rest.find('#') {
        Some(i) => (&rest[..i], Some(&rest[i + 1..])),
        None => (rest, None),
    };
    let (path, query) = match rest.find('?') {
        Some(i) => (&rest[..i], Some(&rest[i + 1..])),
        None => (rest, None),
    };
    Some(SplitUrl {
        scheme,
        authority,
        path,
        query,
        fragment,
    })
}

/// Extract the host from a URL authority (`[userinfo@]host[:port]`, with
/// bracketed IPv6 literals). Lower-cased, trailing dot stripped.
fn host_from_authority(authority: &str) -> String {
    let host_port = match authority.rfind('@') {
        Some(i) => &authority[i + 1..],
        None => authority,
    };
    let host = if let Some(stripped) = host_port.strip_prefix('[') {
        // IPv6 literal: cut at the closing bracket (ignore :port after it).
        match stripped.find(']') {
            Some(i) => &stripped[..i],
            None => host_port,
        }
    } else {
        match host_port.rfind(':') {
            Some(i) => &host_port[..i],
            None => host_port,
        }
    };
    host.trim()
        .to_ascii_lowercase()
        .trim_end_matches('.')
        .to_string()
}

struct ParsedHttpUrl {
    scheme: String,
    host: String,
}

/// Parse an http/https URL into (scheme, host). Mirrors the front of hermes
/// `is_safe_url`: lower-cased host, trailing dot removed.
fn parse_http_url(url: &str) -> Option<ParsedHttpUrl> {
    let split = split_url(url.trim())?;
    let scheme = split.scheme.trim().to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let host = host_from_authority(split.authority);
    if host.is_empty() {
        return None;
    }
    Some(ParsedHttpUrl { scheme, host })
}

// ---------------------------------------------------------------------------
// IP classification (hermes `_is_blocked_ip`)
// ---------------------------------------------------------------------------

/// IPv4-mapped IPv6 addresses (`::ffff:x.x.x.x`) are checked by their
/// embedded IPv4 address, like hermes.
fn canonical_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(v6),
        },
        v4 => v4,
    }
}

fn ip_in_net(ip: IpAddr, base: IpAddr, prefix_len: u32) -> bool {
    match (ip, base) {
        (IpAddr::V4(a), IpAddr::V4(b)) => {
            let mask = if prefix_len == 0 {
                0u32
            } else {
                u32::MAX << (32 - prefix_len)
            };
            (u32::from(a) & mask) == (u32::from(b) & mask)
        }
        (IpAddr::V6(a), IpAddr::V6(b)) => {
            let mask = if prefix_len == 0 {
                0u128
            } else {
                u128::MAX << (128 - prefix_len)
            };
            (u128::from(a) & mask) == (u128::from(b) & mask)
        }
        _ => false,
    }
}

fn v4_in(ip: std::net::Ipv4Addr, net: &str, prefix_len: u32) -> bool {
    let Some(base) = net.parse::<IpAddr>().ok() else {
        return false;
    };
    ip_in_net(IpAddr::V4(ip), base, prefix_len)
}

fn v6_segments(ip: std::net::Ipv6Addr) -> [u16; 8] {
    ip.segments()
}

/// True when the IP must be blocked for SSRF protection. Union of Python
/// `ipaddress` `is_private`/`is_loopback`/`is_link_local`/`is_reserved`/
/// `is_multicast`/`is_unspecified` plus CGNAT (RFC 6598), matching hermes.
pub fn is_blocked_ip(ip: IpAddr) -> bool {
    match canonical_ip(ip) {
        IpAddr::V4(v4) => {
            if v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_multicast()
                || v4.is_unspecified()
            {
                return true;
            }
            // Ranges Python's is_private/is_reserved cover beyond Rust's:
            // 0.0.0.0/8, CGNAT 100.64.0.0/10, 192.0.0.0/29, 192.0.0.170/31,
            // benchmark 198.18.0.0/15, reserved 240.0.0.0/4.
            v4_in(v4, "0.0.0.0", 8)
                || v4_in(v4, "100.64.0.0", 10)
                || v4_in(v4, "192.0.0.0", 29)
                || v4_in(v4, "192.0.0.170", 31)
                || v4_in(v4, "198.18.0.0", 15)
                || v4_in(v4, "240.0.0.0", 4)
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() {
                return true;
            }
            let seg = v6_segments(v6);
            // Link-local fe80::/10 (Python: fe80::/64 — superset is safer).
            if (seg[0] & 0xffc0) == 0xfe80 {
                return true;
            }
            // Unique-local fc00::/7.
            if (seg[0] & 0xfe00) == 0xfc00 {
                return true;
            }
            let ip = IpAddr::V6(v6);
            // Python is_private IPv6 extras.
            ip_in_net(ip, "64:ff9b::".parse::<IpAddr>().unwrap(), 96)
                || ip_in_net(ip, "64:ff9b:1::".parse::<IpAddr>().unwrap(), 48)
                || ip_in_net(ip, "100::".parse::<IpAddr>().unwrap(), 64)
                // 6to4 (Python is_private for IPv6).
                || (seg[0] == 0x2002)
                // IETF protocol assignments 2001::/23 (Python is_reserved).
                || ip_in_net(ip, "2001::".parse::<IpAddr>().unwrap(), 23)
                // Documentation space (Python is_private; outside 2001::/23).
                || ip_in_net(ip, "2001:db8::".parse::<IpAddr>().unwrap(), 32)
        }
    }
}

fn is_always_blocked_ip(ip: IpAddr) -> bool {
    let canon = canonical_ip(ip);
    always_blocked_ips()
        .iter()
        .any(|blocked| canonical_ip(*blocked) == canon)
        || ALWAYS_BLOCKED_NETWORKS.iter().any(|(net, len)| {
            ip_in_net(
                canon,
                net.parse::<IpAddr>().expect("static network base"),
                *len,
            )
        })
}

// ---------------------------------------------------------------------------
// allow_private_urls toggle (hermes `_global_allow_private_urls`)
// ---------------------------------------------------------------------------

static ALLOW_PRIVATE_CACHE: OnceLock<bool> = OnceLock::new();
#[cfg(test)]
static ALLOW_PRIVATE_CACHE_RESET: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Resolve the effective private-URL toggle.
///
/// Priority: `ULNCLAW_ALLOW_PRIVATE_URLS` / `HERMES_ALLOW_PRIVATE_URLS` env
/// var (explicit true/false wins), then `[security] allow_private_urls` in
/// the config file. The config lookup is cached for the process lifetime
/// (hermes `_global_allow_private_urls` semantics).
pub fn allow_private_urls() -> bool {
    for name in ["ULNCLAW_ALLOW_PRIVATE_URLS", "HERMES_ALLOW_PRIVATE_URLS"] {
        if let Some(value) = crate::config::get_env_value(name) {
            match value.trim().to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" => return true,
                "false" | "0" | "no" => return false,
                _ => {}
            }
        }
    }
    *ALLOW_PRIVATE_CACHE.get_or_init(|| {
        crate::config::UlncLawConfig::load(None)
            .map(|c| c.security.allow_private_urls)
            .unwrap_or(false)
    })
}

/// Reset the cached config toggle — tests only.
#[cfg(test)]
pub fn reset_allow_private_cache() {
    let _guard = ALLOW_PRIVATE_CACHE_RESET.lock().unwrap();
    // OnceLock cannot be cleared; tests rely on env-var overrides instead.
}

fn allows_private_ip_resolution(hostname: &str, scheme: &str) -> bool {
    scheme == "https" && TRUSTED_PRIVATE_IP_HOSTS.contains(&hostname)
}

// ---------------------------------------------------------------------------
// DNS resolution
// ---------------------------------------------------------------------------

fn resolve_sync(hostname: &str) -> Result<Vec<IpAddr>, ()> {
    use std::net::ToSocketAddrs;
    let addrs = (hostname, 0u16).to_socket_addrs().map_err(|_| ())?;
    Ok(addrs.map(|sa| sa.ip()).collect())
}

async fn resolve_async(hostname: &str) -> Result<Vec<IpAddr>, ()> {
    let addrs = tokio::net::lookup_host((hostname, 0u16))
        .await
        .map_err(|_| ())?;
    Ok(addrs.map(|sa| sa.ip()).collect())
}

enum Resolution {
    Ips(Vec<IpAddr>),
    /// DNS failure for a non-literal hostname: with a proxy configured the
    /// request is delegated to the proxy (hermes carve-out), otherwise the
    /// caller fails closed.
    DnsFailed {
        proxy_pass: bool,
    },
}

fn resolve_hostname(hostname: &str, resolver: Resolver) -> Resolution {
    // Literal IPs need no DNS.
    if let Ok(ip) = hostname.parse::<IpAddr>() {
        return Resolution::Ips(vec![ip]);
    }
    let result = match resolver {
        Resolver::Sync => resolve_sync(hostname),
        Resolver::Async(ips) => ips,
    };
    match result {
        Ok(ips) if !ips.is_empty() => Resolution::Ips(ips),
        _ => Resolution::DnsFailed {
            proxy_pass: proxy_is_configured(),
        },
    }
}

enum Resolver {
    Sync,
    Async(Result<Vec<IpAddr>, ()>),
}

// ---------------------------------------------------------------------------
// The checks
// ---------------------------------------------------------------------------

fn is_safe_url_inner(parsed: &ParsedHttpUrl, resolution: Resolution) -> bool {
    // Blocked metadata hostnames — ALWAYS, even with the toggle on.
    if BLOCKED_HOSTNAMES.contains(&parsed.host.as_str()) {
        tracing::warn!("Blocked request to internal hostname: {}", parsed.host);
        return false;
    }

    let allow_all_private = allow_private_urls();
    let allow_private_ip = allows_private_ip_resolution(&parsed.host, &parsed.scheme);

    let ips = match resolution {
        Resolution::Ips(ips) => ips,
        Resolution::DnsFailed { proxy_pass: true } => {
            tracing::debug!(
                "DNS resolution failed for {} — proxy configured, allowing through for proxy-side resolution",
                parsed.host
            );
            return true;
        }
        Resolution::DnsFailed { proxy_pass: false } => {
            tracing::warn!(
                "Blocked request — DNS resolution failed for: {}",
                parsed.host
            );
            return false;
        }
    };

    for ip in ips {
        if is_always_blocked_ip(ip) {
            tracing::warn!(
                "Blocked request to cloud metadata address: {} -> {}",
                parsed.host,
                ip
            );
            return false;
        }
        if !allow_all_private && !allow_private_ip && is_blocked_ip(ip) {
            tracing::warn!(
                "Blocked request to private/internal address: {} -> {}",
                parsed.host,
                ip
            );
            return false;
        }
    }
    true
}

fn is_always_blocked_inner(parsed_host: &str, resolution: Resolution) -> bool {
    if BLOCKED_HOSTNAMES.contains(&parsed_host) {
        return true;
    }
    let ips = match resolution {
        Resolution::Ips(ips) => ips,
        // DNS failure is NOT always-blocked (hermes semantics).
        Resolution::DnsFailed { .. } => return false,
    };
    ips.iter().any(|ip| is_always_blocked_ip(*ip))
}

/// Synchronous SSRF check (blocking DNS). Prefer [`is_safe_url`] from async
/// code; this variant is used by the redirect policy.
///
/// Returns true when the URL target is not a private/internal address.
/// Fails closed on parse and DNS errors (except the proxy carve-out).
pub fn is_safe_url_sync(url: &str) -> bool {
    let Some(parsed) = parse_http_url(url) else {
        tracing::warn!("Blocked request — unsupported or unparseable URL: {url}");
        return false;
    };
    let resolution = resolve_hostname(&parsed.host, Resolver::Sync);
    is_safe_url_inner(&parsed, resolution)
}

/// Async SSRF check — same rules as [`is_safe_url_sync`], DNS off the
/// current task via the tokio resolver (hermes `async_is_safe_url`).
pub async fn is_safe_url(url: &str) -> bool {
    let Some(parsed) = parse_http_url(url) else {
        tracing::warn!("Blocked request — unsupported or unparseable URL: {url}");
        return false;
    };
    let resolved = resolve_async(&parsed.host).await;
    let resolution = resolve_hostname(&parsed.host, Resolver::Async(resolved));
    is_safe_url_inner(&parsed, resolution)
}

/// Security floor: true when the URL targets a cloud metadata endpoint
/// (hostname, literal IP, or DNS-resolving to one) — blocked regardless of
/// the `allow_private_urls` toggle. Parse/DNS errors return false (caller
/// decides fail-open/closed; hermes `is_always_blocked_url`).
pub fn is_always_blocked_url_sync(url: &str) -> bool {
    let Some(split) = split_url(url.trim()) else {
        return false;
    };
    let host = host_from_authority(split.authority);
    if host.is_empty() {
        return false;
    }
    if BLOCKED_HOSTNAMES.contains(&host.as_str()) {
        return true;
    }
    let resolution = if let Ok(ip) = host.parse::<IpAddr>() {
        Resolution::Ips(vec![ip])
    } else {
        match resolve_sync(&host) {
            Ok(ips) if !ips.is_empty() => Resolution::Ips(ips),
            _ => Resolution::DnsFailed { proxy_pass: false },
        }
    };
    is_always_blocked_inner(&host, resolution)
}

// ---------------------------------------------------------------------------
// Sensitive query parameters (hermes `sensitive_query_param_name`)
// ---------------------------------------------------------------------------

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) =
                u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16)
            {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// First sensitive query parameter name in `url`, if any. Catches opaque
/// magic links, OAuth codes, signed URL signatures, custom `?token=...`
/// values that have no recognizable vendor prefix.
pub fn sensitive_query_param_name(url: &str) -> Option<String> {
    if !url.contains('?') {
        return None;
    }
    let split = split_url(url.trim())?;
    let scheme = split.scheme.trim().to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let query = split.query.filter(|q| !q.is_empty())?;
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        if !value.is_empty()
            && SENSITIVE_QUERY_PARAM_NAMES
                .contains(&percent_decode(key).to_ascii_lowercase().as_str())
        {
            return Some(key.to_string());
        }
    }
    None
}

/// True when `url` carries likely credential-bearing query params.
pub fn has_sensitive_query_params(url: &str) -> bool {
    sensitive_query_param_name(url).is_some()
}

// ---------------------------------------------------------------------------
// URL normalisation (hermes `normalize_url_for_request`)
// ---------------------------------------------------------------------------

/// Keep-aside set never percent-encoded by Python's `quote`.
fn is_unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')
}

fn quote_component(input: &str, safe: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        if ch.is_ascii() && (is_unreserved(ch as u8) || safe.contains(ch)) {
            out.push(ch);
        } else {
            let mut buf = [0u8; 4];
            for byte in ch.encode_utf8(&mut buf).as_bytes() {
                out.push_str(&format!("%{:02X}", byte));
            }
        }
    }
    out
}

/// Return an ASCII-safe HTTP URL for agent URL tools.
///
/// Users and models often provide IRIs (`https://wttr.in/Köln`) or URLs with
/// stray whitespace after the scheme; browsers/HTTP clients need URIs.
/// Non-ASCII host handling is delegated to reqwest's URL parser (IDNA).
pub fn normalize_url_for_request(url: &str) -> String {
    let raw = url.trim();
    if raw.is_empty() {
        return raw.to_string();
    }
    // Repair `https:// example.com` whitespace artifacts.
    static SCHEME_WS: OnceLock<Regex> = OnceLock::new();
    let re = SCHEME_WS
        .get_or_init(|| Regex::new(r"^([A-Za-z][A-Za-z0-9+.-]*://)\s+").expect("static regex"));
    let raw = re.replace(raw, "$1").into_owned();

    let Some(split) = split_url(&raw) else {
        return raw;
    };
    let scheme = split.scheme.trim().to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return raw;
    }

    let path = quote_component(split.path, "/%:@!$&'()*+,;=");
    let mut rebuilt = format!("{}://{}{}", split.scheme.trim(), split.authority, path);
    if let Some(query) = split.query {
        rebuilt.push('?');
        rebuilt.push_str(&quote_component(query, "/%:@!$&'()*+,;=?"));
    }
    if let Some(fragment) = split.fragment {
        rebuilt.push('#');
        rebuilt.push_str(&quote_component(fragment, "/%:@!$&'()*+,;=?"));
    }
    rebuilt
}

/// HTTP client whose redirect policy re-validates every hop against the
/// SSRF rules (hermes httpx event-hook semantics). Shared by every
/// hermes-owned fetch path (web extract, video download, ...).
pub fn ssrf_guarded_client(timeout: std::time::Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(timeout)
        .user_agent(concat!(
            "Mozilla/5.0 (X11; Linux x86_64) ulnclaw/",
            env!("CARGO_PKG_VERSION")
        ))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            let target = attempt.url().to_string();
            if is_safe_url_sync(&target) {
                attempt.follow()
            } else {
                attempt.error(format!(
                    "blocked redirect to private/internal address: {target}"
                ))
            }
        }))
        .build()
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Env-mutating tests must not interleave.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn set_env(name: &str, value: Option<&str>) {
        match value {
            Some(v) => std::env::set_var(name, v),
            None => std::env::remove_var(name),
        }
    }

    fn clear_toggle_env() {
        set_env("ULNCLAW_ALLOW_PRIVATE_URLS", None);
        set_env("HERMES_ALLOW_PRIVATE_URLS", None);
        set_env("HTTP_PROXY", None);
        set_env("http_proxy", None);
        set_env("HTTPS_PROXY", None);
        set_env("https_proxy", None);
        set_env("ALL_PROXY", None);
        set_env("all_proxy", None);
    }

    #[test]
    fn private_literal_ips_blocked() {
        let _lock = ENV_LOCK.lock().unwrap();
        clear_toggle_env();
        for url in [
            "http://127.0.0.1/",
            "http://10.0.0.1/x",
            "http://172.16.5.5/",
            "http://192.168.1.1:8080/",
            "http://169.254.169.254/latest/meta-data/",
            "http://100.64.0.1/",
            "http://0.0.0.0/",
            "http://198.18.0.1/",
            "http://240.0.0.1/",
            "http://[::1]/",
            "http://[fc00::1]/",
            "http://[fe80::1]/",
        ] {
            assert!(!is_safe_url_sync(url), "should block {url}");
        }
    }

    #[test]
    fn public_literal_ips_allowed() {
        let _lock = ENV_LOCK.lock().unwrap();
        clear_toggle_env();
        assert!(is_safe_url_sync("http://8.8.8.8/"));
        assert!(is_safe_url_sync("https://1.1.1.1/dns"));
        assert!(is_safe_url_sync("http://[2606:4700:4700::1111]/"));
    }

    #[test]
    fn scheme_gate() {
        assert!(!is_safe_url_sync("ftp://example.com/"));
        assert!(!is_safe_url_sync("file:///etc/passwd"));
        assert!(!is_safe_url_sync("gopher://example.com/"));
        assert!(!is_safe_url_sync("not a url"));
        assert!(!is_safe_url_sync(""));
    }

    #[test]
    fn metadata_hostname_blocked_without_dns() {
        let _lock = ENV_LOCK.lock().unwrap();
        clear_toggle_env();
        assert!(!is_safe_url_sync(
            "http://metadata.google.internal/computeMetadata/v1/"
        ));
        assert!(!is_safe_url_sync("https://metadata.goog/"));
        assert!(is_always_blocked_url_sync(
            "http://metadata.google.internal/"
        ));
    }

    #[test]
    fn metadata_always_blocked_even_with_toggle() {
        let _lock = ENV_LOCK.lock().unwrap();
        clear_toggle_env();
        set_env("ULNCLAW_ALLOW_PRIVATE_URLS", Some("true"));
        assert!(!is_safe_url_sync(
            "http://169.254.169.254/latest/meta-data/"
        ));
        assert!(!is_safe_url_sync("http://metadata.google.internal/"));
        assert!(!is_safe_url_sync("http://[::ffff:169.254.169.254]/"));
        // But ordinary private addresses pass with the toggle on.
        assert!(is_safe_url_sync("http://127.0.0.1/"));
        assert!(is_safe_url_sync("http://192.168.0.10/"));
        clear_toggle_env();
    }

    #[test]
    fn toggle_env_variants() {
        let _lock = ENV_LOCK.lock().unwrap();
        clear_toggle_env();
        set_env("HERMES_ALLOW_PRIVATE_URLS", Some("yes"));
        assert!(allow_private_urls());
        set_env("HERMES_ALLOW_PRIVATE_URLS", Some("false"));
        assert!(
            !allow_private_urls(),
            "explicit false must not fall through"
        );
        clear_toggle_env();
    }

    #[test]
    fn ipv4_mapped_always_blocked() {
        assert!(is_always_blocked_url_sync(
            "http://[::ffff:169.254.169.254]/"
        ));
        assert!(is_always_blocked_url_sync("http://169.254.170.2/"));
        assert!(!is_always_blocked_url_sync("http://8.8.8.8/"));
        assert!(!is_always_blocked_url_sync("http://192.168.1.1/"));
        assert!(!is_always_blocked_url_sync("not a url"));
    }

    #[test]
    fn dns_failure_proxy_carveout() {
        let _lock = ENV_LOCK.lock().unwrap();
        clear_toggle_env();
        // .invalid never resolves (RFC 6761).
        let url = "https://probe-host.invalid/page";
        assert!(!is_safe_url_sync(url), "no proxy: fail closed");
        set_env("HTTPS_PROXY", Some("http://proxy.local:3128"));
        assert!(is_safe_url_sync(url), "proxy configured: delegate DNS");
        // Literal IPs never get the proxy carve-out.
        assert!(!is_safe_url_sync("http://10.9.9.9/"));
        clear_toggle_env();
    }

    #[test]
    fn sensitive_query_params() {
        assert_eq!(
            sensitive_query_param_name("https://x.com/cb?token=abc123"),
            Some("token".to_string())
        );
        assert_eq!(
            sensitive_query_param_name("https://x.com/?access_token=a&b=1"),
            Some("access_token".to_string())
        );
        assert_eq!(
            sensitive_query_param_name("https://x.com/?x-amz-signature=abc"),
            Some("x-amz-signature".to_string())
        );
        // Non-sensitive names and value-less params pass.
        assert!(sensitive_query_param_name("https://x.com/?code=abc").is_none());
        assert!(sensitive_query_param_name("https://x.com/?q=token").is_none());
        assert!(sensitive_query_param_name("https://x.com/?token").is_none());
        assert!(sensitive_query_param_name("https://x.com/?token=").is_none());
        assert!(sensitive_query_param_name("https://x.com/no-query").is_none());
        assert!(sensitive_query_param_name("ftp://x.com/?token=a").is_none());
        assert!(has_sensitive_query_params("https://x.com/?jwt=zzz"));
    }

    #[test]
    fn normalize_repairs_scheme_whitespace() {
        assert_eq!(
            normalize_url_for_request("https://   example.com/a"),
            "https://example.com/a"
        );
    }

    #[test]
    fn normalize_percent_encodes_non_ascii_and_spaces() {
        assert_eq!(
            normalize_url_for_request("https://wttr.in/Köln"),
            "https://wttr.in/K%C3%B6ln"
        );
        assert_eq!(
            normalize_url_for_request("https://example.com/a b?c=d e"),
            "https://example.com/a%20b?c=d%20e"
        );
        // Existing escapes preserved (% is in the safe set).
        assert_eq!(
            normalize_url_for_request("https://example.com/a%20b"),
            "https://example.com/a%20b"
        );
    }

    #[test]
    fn normalize_passthrough_cases() {
        assert_eq!(normalize_url_for_request(""), "");
        assert_eq!(normalize_url_for_request("  "), "");
        assert_eq!(
            normalize_url_for_request("ftp://example.com/Köln"),
            "ftp://example.com/Köln"
        );
        assert_eq!(normalize_url_for_request("plain text"), "plain text");
    }

    #[test]
    fn parse_http_url_edges() {
        let p = parse_http_url("https://User:Pass@Example.com.:8443/x").unwrap();
        assert_eq!(p.host, "example.com");
        assert_eq!(p.scheme, "https");
        let p = parse_http_url("http://[::1]:8080/").unwrap();
        assert_eq!(p.host, "::1");
        assert!(parse_http_url("ftp://example.com/").is_none());
        assert!(parse_http_url("http://").is_none());
        assert!(parse_http_url("http:///path").is_none());
    }

    #[test]
    fn trusted_private_host_helper() {
        assert!(allows_private_ip_resolution(
            "multimedia.nt.qq.com.cn",
            "https"
        ));
        assert!(!allows_private_ip_resolution(
            "multimedia.nt.qq.com.cn",
            "http"
        ));
        assert!(!allows_private_ip_resolution("other.example.com", "https"));
    }

    #[test]
    fn blocked_ip_ranges() {
        assert!(is_blocked_ip("100.64.1.1".parse().unwrap()));
        assert!(is_blocked_ip("192.0.0.2".parse().unwrap()));
        assert!(is_blocked_ip("192.0.0.170".parse().unwrap()));
        assert!(is_blocked_ip("198.19.0.1".parse().unwrap()));
        assert!(is_blocked_ip("255.255.255.255".parse().unwrap()));
        assert!(!is_blocked_ip("8.8.8.8".parse().unwrap()));
        assert!(!is_blocked_ip("93.184.216.34".parse().unwrap()));
        assert!(is_blocked_ip("::ffff:10.0.0.1".parse().unwrap()));
        assert!(is_blocked_ip("2002::1".parse().unwrap()));
        assert!(is_blocked_ip("2001:db8::1".parse().unwrap()));
    }

    #[tokio::test]
    async fn async_is_safe_url_matches_sync() {
        let _lock = ENV_LOCK.lock().unwrap();
        clear_toggle_env();
        assert!(!is_safe_url("http://127.0.0.1/").await);
        assert!(!is_safe_url("http://metadata.google.internal/").await);
        assert!(is_safe_url("http://8.8.8.8/").await);
        assert!(!is_safe_url("ftp://example.com/").await);
    }
}

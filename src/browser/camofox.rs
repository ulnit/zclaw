//! Camofox browser backend — anti-detection browser via REST API.
//!
//! Port of hermes `tools/browser_camofox.py` (v2026.8.3). Camofox-browser is
//! a self-hosted Node.js server wrapping Camoufox (Firefox fork with C++
//! fingerprint spoofing) that exposes a REST API mapping 1:1 to the browser
//! tool interface: accessibility snapshots with element refs, click/type/
//! scroll by ref, screenshots, etc.
//!
//! When `CAMOFOX_URL` is set (e.g. `http://localhost:9377`) the browser
//! tools route through this module instead of the local CDP session. A live
//! or configured CDP endpoint takes priority over Camofox (hermes
//! `is_camofox_mode` semantics).
//!
//! Env configuration:
//! - `CAMOFOX_URL` — server base URL (enables the backend)
//! - `CAMOFOX_API_KEY` — bearer token for the control API
//! - `CAMOFOX_USER_ID` / `CAMOFOX_SESSION_KEY` — operate on an existing
//!   browser profile instead of a fresh ephemeral one
//! - `CAMOFOX_ADOPT_EXISTING_TAB` — recover an already-open tab after a
//!   process restart
//! - `CAMOFOX_REWRITE_LOOPBACK_URLS` — rewrite loopback page URLs for
//!   Docker-hosted Camofox (`CAMOFOX_LOOPBACK_HOST_ALIAS`, default
//!   `host.docker.internal`)
//! - `CAMOFOX_MANAGED_PERSISTENCE` — stable profile-scoped userId
//!   (UUIDv5 of the state dir) so the Camofox server reuses the same
//!   persistent browser profile across restarts (hermes
//!   `browser.camofox.managed_persistence`)

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use serde_json::{json, Value};

const DEFAULT_TIMEOUT_SECS: u64 = 30;
const HEALTH_TIMEOUT_SECS: u64 = 5;
const NAVIGATE_TIMEOUT_SECS: u64 = 60;
/// Camofox paginates snapshots at this limit.
const SNAPSHOT_MAX_CHARS: usize = 80_000;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configured Camofox server URL (trailing slash stripped), if any.
pub fn camofox_url() -> Option<String> {
    std::env::var("CAMOFOX_URL")
        .ok()
        .map(|v| v.trim().trim_end_matches('/').to_string())
        .filter(|v| !v.is_empty())
}

/// True when the Camofox backend is configured and no CDP override is
/// active. A CDP endpoint takes priority so the tools operate on the real
/// CDP browser instead of being silently routed to Camofox (hermes
/// `is_camofox_mode`).
pub fn is_camofox_mode() -> bool {
    if let Some(raw) = crate::browser::configured_endpoint_raw() {
        if !crate::browser::is_auto_mode(&raw) {
            return false;
        }
    }
    camofox_url().is_some()
}

fn env_flag(name: &str) -> Option<bool> {
    let raw = std::env::var(name).ok()?.trim().to_lowercase();
    match raw.as_str() {
        "" => None,
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Enable hermes-managed Camofox persistence: sessions use a stable
/// profile-scoped userId so the Camofox server maps it to the same
/// persistent browser profile across restarts. Hermes controls this via
/// `browser.camofox.managed_persistence` in config.yaml; ulnclaw browser
/// configuration is env-based (`ULNCLAW_BROWSER_CDP` precedent), so the
/// knob is `CAMOFOX_MANAGED_PERSISTENCE`.
pub fn managed_persistence_enabled() -> bool {
    env_flag("CAMOFOX_MANAGED_PERSISTENCE").unwrap_or(false)
}

/// Profile-scoped root directory for Camofox persistence (hermes
/// `get_camofox_state_dir`: `<home>/browser_auth/camofox`).
pub fn state_dir() -> std::path::PathBuf {
    crate::config::ulnclaw_home()
        .join("browser_auth")
        .join("camofox")
}

/// Stable managed Camofox identity for this profile (hermes
/// `get_camofox_identity`). The user identity is profile-scoped (same
/// home = same userId across restarts); the session key is scoped to the
/// logical browser task so tabs within the same profile reuse the same
/// identity contract.
pub fn managed_identity(task_id: &str) -> (String, String) {
    let scope_root = state_dir().display().to_string();
    let logical_scope = if task_id.is_empty() {
        "default"
    } else {
        task_id
    };
    let user_digest = uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_URL,
        format!("camofox-user:{scope_root}").as_bytes(),
    )
    .simple()
    .to_string();
    let session_digest = uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_URL,
        format!("camofox-session:{scope_root}:{logical_scope}").as_bytes(),
    )
    .simple()
    .to_string();
    (
        format!("ulnclaw_{}", &user_digest[..10]),
        format!("task_{}", &session_digest[..16]),
    )
}

fn auth_header() -> Option<String> {
    std::env::var("CAMOFOX_API_KEY")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .map(|key| format!("Bearer {key}"))
}

fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(HEALTH_TIMEOUT_SECS))
            .build()
            .expect("reqwest client builds")
    })
}

// ---------------------------------------------------------------------------
// Health / VNC
// ---------------------------------------------------------------------------

fn vnc_slot() -> MutexGuard<'static, (bool, Option<String>)> {
    static SLOT: OnceLock<Mutex<(bool, Option<String>)>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new((false, None)))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Verify the Camofox server is reachable; caches the VNC URL from the
/// `/health` response (hermes `check_camofox_available`).
pub async fn check_available() -> bool {
    let Some(url) = camofox_url() else {
        return false;
    };
    let result = http_client()
        .get(format!("{url}/health"))
        .timeout(Duration::from_secs(HEALTH_TIMEOUT_SECS))
        .send()
        .await;
    match result {
        Ok(resp) if resp.status() == 200 => {
            // Parse before locking: the guard must not be held across await.
            let data = resp.json::<Value>().await.ok();
            let mut slot = vnc_slot();
            if !slot.0 {
                if let Some(data) = data {
                    if let Some(port) = data.get("vncPort").and_then(|v| v.as_u64()) {
                        if (1..=65535).contains(&port) {
                            let host = url_host(&url).unwrap_or_else(|| "localhost".into());
                            slot.1 = Some(format!("http://{host}:{port}"));
                        }
                    }
                }
                slot.0 = true;
            }
            true
        }
        _ => false,
    }
}

/// VNC URL if the Camofox server exposes one (probes `/health` once).
pub async fn vnc_url() -> Option<String> {
    let checked = vnc_slot().0;
    if !checked {
        check_available().await;
    }
    vnc_slot().1.clone()
}

/// Host portion of a base URL (manual parse; no `url` crate dependency).
fn url_host(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split('/').next()?;
    let hostport = authority.rsplit('@').next()?;
    let host = if hostport.starts_with('[') {
        hostport
            .split(']')
            .next()?
            .trim_start_matches('[')
            .to_string()
    } else {
        hostport.split(':').next()?.to_string()
    };
    if host.is_empty() {
        None
    } else {
        Some(host)
    }
}

// ---------------------------------------------------------------------------
// Loopback URL rewriting (Docker-hosted Camofox)
// ---------------------------------------------------------------------------

fn loopback_rewrite_enabled() -> bool {
    env_flag("CAMOFOX_REWRITE_LOOPBACK_URLS").unwrap_or(false)
}

fn loopback_host_alias() -> String {
    std::env::var("CAMOFOX_LOOPBACK_HOST_ALIAS")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "host.docker.internal".into())
}

fn is_loopback_hostname(host: &str) -> bool {
    let host = host
        .trim()
        .trim_matches(|c| c == '[' || c == ']')
        .to_lowercase();
    if matches!(host.as_str(), "localhost" | "localhost.localdomain") {
        return true;
    }
    host.parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

/// Rewrite loopback page URLs for Docker-hosted Camofox when enabled.
/// Returns `(rewritten_url, metadata)`; metadata is present only when a
/// rewrite happened so the tool result can disclose the change (hermes
/// `_rewrite_loopback_url_for_camofox`).
pub fn rewrite_loopback_url(url: &str) -> (String, Option<Value>) {
    if !loopback_rewrite_enabled() {
        return (url.to_string(), None);
    }
    let Some((scheme, rest)) = url.split_once("://") else {
        return (url.to_string(), None);
    };
    if !matches!(scheme, "http" | "https") {
        return (url.to_string(), None);
    }
    let (authority, tail) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let (userinfo, hostport) = match authority.rfind('@') {
        Some(i) => (&authority[..=i], &authority[i + 1..]),
        None => ("", authority),
    };
    let (host, port) = if hostport.starts_with('[') {
        let host = hostport
            .split(']')
            .next()
            .unwrap_or("")
            .trim_start_matches('[');
        let port = hostport
            .rsplit(':')
            .next()
            .filter(|p| !p.is_empty() && p != &hostport[1..].split(']').nth(1).unwrap_or(""))
            .map(String::from);
        (
            host.to_string(),
            port.filter(|p| p.chars().all(|c| c.is_ascii_digit())),
        )
    } else {
        match hostport.rfind(':') {
            Some(i) => (
                hostport[..i].to_string(),
                Some(hostport[i + 1..].to_string()),
            ),
            None => (hostport.to_string(), None),
        }
    };
    if !is_loopback_hostname(&host) {
        return (url.to_string(), None);
    }
    let alias = loopback_host_alias();
    if alias.is_empty() {
        return (url.to_string(), None);
    }
    let host_part = if alias.contains(':') && !alias.starts_with('[') {
        format!("[{alias}]")
    } else {
        alias.clone()
    };
    let port_part = port.map(|p| format!(":{p}")).unwrap_or_default();
    let rewritten = format!("{scheme}://{userinfo}{host_part}{port_part}{tail}");
    (
        rewritten.clone(),
        Some(json!({
            "from": host,
            "to": alias,
            "original_url": url,
            "rewritten_url": rewritten,
        })),
    )
}

// ---------------------------------------------------------------------------
// Session management (hermes `_sessions` map)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct CamofoxSession {
    user_id: String,
    tab_id: Option<String>,
    session_key: String,
    adopt_existing_tab: bool,
}

fn sessions() -> MutexGuard<'static, HashMap<String, CamofoxSession>> {
    static SESSIONS: OnceLock<Mutex<HashMap<String, CamofoxSession>>> = OnceLock::new();
    SESSIONS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn identity_override(task_id: &str) -> Option<(String, String)> {
    let user_id = std::env::var("CAMOFOX_USER_ID")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())?;
    let session_key = std::env::var("CAMOFOX_SESSION_KEY")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| format!("task_{}", &task_id[..task_id.len().min(16)]));
    Some((user_id, session_key))
}

/// Get or create a camofox session for the given task (hermes
/// `_get_session`). Without `CAMOFOX_USER_ID` each session is ephemeral
/// (random userId).
fn get_session(task_id: &str) -> CamofoxSession {
    let task_id = if task_id.is_empty() {
        "default"
    } else {
        task_id
    };
    let mut map = sessions();
    if let Some(existing) = map.get(task_id) {
        return existing.clone();
    }
    let session = if let Some((user_id, session_key)) = identity_override(task_id) {
        CamofoxSession {
            user_id,
            tab_id: None,
            session_key,
            adopt_existing_tab: env_flag("CAMOFOX_ADOPT_EXISTING_TAB").unwrap_or(false),
        }
    } else if managed_persistence_enabled() {
        let (user_id, session_key) = managed_identity(task_id);
        CamofoxSession {
            user_id,
            tab_id: None,
            session_key,
            adopt_existing_tab: env_flag("CAMOFOX_ADOPT_EXISTING_TAB").unwrap_or(false),
        }
    } else {
        CamofoxSession {
            user_id: format!(
                "ulnclaw_{}",
                &uuid::Uuid::new_v4().simple().to_string()[..10]
            ),
            tab_id: None,
            session_key: format!("task_{}", &task_id[..task_id.len().min(16)]),
            adopt_existing_tab: false,
        }
    };
    map.insert(task_id.to_string(), session.clone());
    session
}

fn store_session(task_id: &str, session: &CamofoxSession) {
    let task_id = if task_id.is_empty() {
        "default"
    } else {
        task_id
    };
    sessions().insert(task_id.to_string(), session.clone());
}

fn drop_session(task_id: &str) -> Option<CamofoxSession> {
    let task_id = if task_id.is_empty() {
        "default"
    } else {
        task_id
    };
    sessions().remove(task_id)
}

/// Release the in-memory session without destroying the server-side context
/// (hermes `camofox_soft_cleanup`): only meaningful with an identity
/// override or managed persistence, where the browser profile (and its
/// cookies) must survive across tasks.
pub fn soft_cleanup(task_id: &str) -> bool {
    if identity_override(task_id).is_some() || managed_persistence_enabled() {
        drop_session(task_id);
        true
    } else {
        false
    }
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum CamofoxError {
    Connection(String),
    Http(u16, String),
    Parse(String),
}

impl std::fmt::Display for CamofoxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CamofoxError::Connection(e) => write!(f, "connection: {e}"),
            CamofoxError::Http(code, e) => write!(f, "http {code}: {e}"),
            CamofoxError::Parse(e) => write!(f, "parse: {e}"),
        }
    }
}

fn request_timeout() -> Duration {
    Duration::from_secs(DEFAULT_TIMEOUT_SECS)
}

async fn post(path: &str, body: Value, timeout: Option<Duration>) -> Result<Value, CamofoxError> {
    let Some(url) = camofox_url() else {
        return Err(CamofoxError::Connection("CAMOFOX_URL not set".into()));
    };
    let mut req = http_client()
        .post(format!("{url}{path}"))
        .timeout(timeout.unwrap_or_else(request_timeout))
        .json(&body);
    if let Some(auth) = auth_header() {
        req = req.header("Authorization", auth);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| CamofoxError::Connection(e.to_string()))?;
    let status = resp.status().as_u16();
    if !resp.status().is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(CamofoxError::Http(status, text));
    }
    resp.json::<Value>()
        .await
        .map_err(|e| CamofoxError::Parse(e.to_string()))
}

async fn get(
    path: &str,
    query: &[(&str, String)],
    timeout: Option<Duration>,
) -> Result<Value, CamofoxError> {
    let Some(url) = camofox_url() else {
        return Err(CamofoxError::Connection("CAMOFOX_URL not set".into()));
    };
    let mut req = http_client()
        .get(format!("{url}{path}"))
        .timeout(timeout.unwrap_or_else(request_timeout))
        .query(query);
    if let Some(auth) = auth_header() {
        req = req.header("Authorization", auth);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| CamofoxError::Connection(e.to_string()))?;
    let status = resp.status().as_u16();
    if !resp.status().is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(CamofoxError::Http(status, text));
    }
    resp.json::<Value>()
        .await
        .map_err(|e| CamofoxError::Parse(e.to_string()))
}

async fn get_bytes(path: &str, query: &[(&str, String)]) -> Result<bytes::Bytes, CamofoxError> {
    let Some(url) = camofox_url() else {
        return Err(CamofoxError::Connection("CAMOFOX_URL not set".into()));
    };
    let mut req = http_client()
        .get(format!("{url}{path}"))
        .timeout(request_timeout())
        .query(query);
    if let Some(auth) = auth_header() {
        req = req.header("Authorization", auth);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| CamofoxError::Connection(e.to_string()))?;
    let status = resp.status().as_u16();
    if !resp.status().is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(CamofoxError::Http(status, text));
    }
    resp.bytes()
        .await
        .map_err(|e| CamofoxError::Parse(e.to_string()))
}

async fn delete(path: &str) -> Result<Value, CamofoxError> {
    let Some(url) = camofox_url() else {
        return Err(CamofoxError::Connection("CAMOFOX_URL not set".into()));
    };
    let mut req = http_client()
        .delete(format!("{url}{path}"))
        .timeout(request_timeout());
    if let Some(auth) = auth_header() {
        req = req.header("Authorization", auth);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| CamofoxError::Connection(e.to_string()))?;
    let status = resp.status().as_u16();
    if !resp.status().is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(CamofoxError::Http(status, text));
    }
    resp.json::<Value>()
        .await
        .map_err(|e| CamofoxError::Parse(e.to_string()))
}

fn error_value(message: String) -> Value {
    json!({"success": false, "error": message})
}

fn connection_error_value(e: &CamofoxError) -> Value {
    if matches!(e, CamofoxError::Connection(_)) {
        error_value(format!(
            "Cannot connect to Camofox at {}. Is the server running? Start with: npm start (in camofox-browser dir) or: docker run -p 9377:9377 -e CAMOFOX_PORT=9377 jo-inc/camofox-browser",
            camofox_url().unwrap_or_default()
        ))
    } else {
        error_value(e.to_string())
    }
}

fn no_session_error() -> Value {
    error_value("No browser session. Call browser_navigate first.".into())
}

// ---------------------------------------------------------------------------
// Tab lifecycle
// ---------------------------------------------------------------------------

/// Attach to an already-open managed tab when adoption is enabled (hermes
/// `_adopt_existing_tab`): gateway restarts leave the in-memory cache empty
/// even though Camofox still has the tab.
async fn adopt_existing_tab(mut session: CamofoxSession) -> CamofoxSession {
    if session.tab_id.is_some() || !session.adopt_existing_tab {
        return session;
    }
    let query = vec![("userId", session.user_id.clone())];
    let Ok(data) = get(
        "/tabs",
        &query,
        Some(Duration::from_secs(HEALTH_TIMEOUT_SECS)),
    )
    .await
    else {
        return session;
    };
    let Some(tabs) = data.get("tabs").and_then(|v| v.as_array()) else {
        return session;
    };
    if tabs.is_empty() {
        return session;
    }
    let matching: Vec<&Value> = tabs
        .iter()
        .filter(|tab| {
            tab.get("listItemId").and_then(|v| v.as_str()) == Some(session.session_key.as_str())
        })
        .collect();
    let candidates: Vec<&Value> = if !matching.is_empty() {
        matching
    } else {
        tabs.iter().filter(|tab| tab.is_object()).collect()
    };
    if let Some(latest) = candidates.last() {
        if let Some(tab_id) = latest
            .get("tabId")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            session.tab_id = Some(tab_id.to_string());
        }
    }
    session
}

/// Ensure a tab exists for the session, creating one if needed (hermes
/// `_ensure_tab`).
async fn ensure_tab(task_id: &str, url: &str) -> Result<CamofoxSession, CamofoxError> {
    let mut session = get_session(task_id);
    if session.tab_id.is_none() && session.adopt_existing_tab {
        session = adopt_existing_tab(session).await;
        store_session(task_id, &session);
    }
    if session.tab_id.is_some() {
        return Ok(session);
    }
    let data = post(
        "/tabs",
        json!({
            "userId": session.user_id,
            "listItemId": session.session_key,
            "url": url,
        }),
        None,
    )
    .await?;
    session.tab_id = data.get("tabId").and_then(|v| v.as_str()).map(String::from);
    store_session(task_id, &session);
    Ok(session)
}

// ---------------------------------------------------------------------------
// SSRF guard for Camofox pages
// ---------------------------------------------------------------------------

/// Blocked payload when the current Camofox page is private/internal
/// (hermes `_camofox_private_page_block`). Snapshot/vision/image extraction
/// read current page state, so on a non-local backend they can leak an
/// intranet/metadata page the terminal itself can't reach. Fail-open on
/// probe failure, matching the sibling guards.
async fn private_page_block(session: &CamofoxSession, guard: bool, action: &str) -> Option<Value> {
    if !guard {
        return None;
    }
    let tab_id = session.tab_id.as_deref()?;
    let probe = post(
        &format!("/tabs/{tab_id}/evaluate"),
        json!({"expression": "window.location.href", "userId": session.user_id}),
        None,
    )
    .await;
    let Ok(data) = probe else {
        return None;
    };
    let current = data
        .get("result")
        .map(|v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .unwrap_or_default();
    let current = current
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .trim()
        .to_string();
    if current.is_empty() {
        return None;
    }
    if crate::url_safety::is_always_blocked_url_sync(&current)
        || !crate::url_safety::is_safe_url_sync(&current)
    {
        return Some(json!({
            "success": false,
            "error": format!(
                "Blocked: page URL targets a private or internal address ({current}). Refusing to {action} on this page in this browser mode."
            ),
        }));
    }
    None
}

// ---------------------------------------------------------------------------
// Tool implementations
// ---------------------------------------------------------------------------

/// Navigate to a URL via Camofox (hermes `camofox_navigate`).
pub async fn navigate(task_id: &str, url: &str, guard: bool) -> Value {
    let (browser_url, rewrite_info) = rewrite_loopback_url(url);

    let session = get_session(task_id);
    let (session, data) = if session.tab_id.is_none() {
        match ensure_tab(task_id, &browser_url).await {
            Ok(session) => (session, json!({"ok": true, "url": browser_url})),
            Err(e) => return connection_error_value(&e),
        }
    } else {
        let tab_id = session.tab_id.clone().unwrap_or_default();
        match post(
            &format!("/tabs/{tab_id}/navigate"),
            json!({"userId": session.user_id, "url": browser_url}),
            Some(Duration::from_secs(NAVIGATE_TIMEOUT_SECS)),
        )
        .await
        {
            Ok(data) => (session, data),
            Err(CamofoxError::Http(404, _)) => {
                // Tab was garbage collected — create a fresh one.
                let mut stale = session.clone();
                stale.tab_id = None;
                store_session(task_id, &stale);
                match ensure_tab(task_id, &browser_url).await {
                    Ok(session) => (session, json!({"ok": true, "url": browser_url})),
                    Err(e) => return connection_error_value(&e),
                }
            }
            Err(e) => return connection_error_value(&e),
        }
    };
    let _ = guard; // navigate pre-checks happen in the dispatcher

    let mut result = json!({
        "success": true,
        "url": data.get("url").cloned().unwrap_or_else(|| json!(browser_url)),
        "title": data.get("title").cloned().unwrap_or(json!("")),
    });
    if let Some(info) = rewrite_info {
        result["requested_url"] = json!(url);
        result["url_rewrite"] = info.clone();
        result["warning"] = json!(format!(
            "Rewrote loopback URL for Docker-hosted Camofox: {} -> {}",
            info.get("from").and_then(|v| v.as_str()).unwrap_or(""),
            info.get("to").and_then(|v| v.as_str()).unwrap_or(""),
        ));
    }
    if let Some(vnc) = vnc_url().await {
        result["vnc_url"] = json!(vnc);
        result["vnc_hint"] = json!(
            "Browser is visible via VNC. Share this link with the user so they can watch the browser live."
        );
    }

    // Auto-take a compact snapshot so the model can act immediately.
    if let Some(tab_id) = session.tab_id.as_deref() {
        let query = vec![("userId", session.user_id.clone())];
        if let Ok(snap) = get(&format!("/tabs/{tab_id}/snapshot"), &query, None).await {
            let mut text = snap
                .get("snapshot")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if text.len() > SNAPSHOT_MAX_CHARS {
                text.truncate(SNAPSHOT_MAX_CHARS);
                text.push_str("\n[snapshot truncated]");
            }
            result["snapshot"] = crate::browser::guard::redact_value(json!(text));
            result["element_count"] = snap.get("refsCount").cloned().unwrap_or(json!(0));
        }
    }
    result
}

/// Accessibility tree snapshot (hermes `camofox_snapshot`).
pub async fn snapshot(task_id: &str, guard: bool) -> Value {
    let session = get_session(task_id);
    if session.tab_id.is_none() {
        return no_session_error();
    }
    if let Some(blocked) = private_page_block(&session, guard, "read a page snapshot").await {
        return blocked;
    }
    let tab_id = session.tab_id.clone().unwrap_or_default();
    let query = vec![("userId", session.user_id.clone())];
    match get(&format!("/tabs/{tab_id}/snapshot"), &query, None).await {
        Ok(data) => {
            let mut text = data
                .get("snapshot")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if text.len() > SNAPSHOT_MAX_CHARS {
                text.truncate(SNAPSHOT_MAX_CHARS);
                text.push_str("\n[snapshot truncated]");
            }
            json!({
                "success": true,
                "snapshot": crate::browser::guard::redact_value(json!(text)),
                "element_count": data.get("refsCount").cloned().unwrap_or(json!(0)),
                "hint": "interact via browser_click/browser_type with element refs like \"3\"",
            })
        }
        Err(e) => connection_error_value(&e),
    }
}

/// Click an element by ref (hermes `camofox_click`).
pub async fn click(task_id: &str, element: &str, guard: bool) -> Value {
    let session = get_session(task_id);
    if session.tab_id.is_none() {
        return no_session_error();
    }
    if let Some(blocked) = private_page_block(&session, guard, "click").await {
        return blocked;
    }
    let clean_ref = element.trim_start_matches('@').to_string();
    let tab_id = session.tab_id.clone().unwrap_or_default();
    match post(
        &format!("/tabs/{tab_id}/click"),
        json!({"userId": session.user_id, "ref": clean_ref}),
        None,
    )
    .await
    {
        Ok(data) => json!({
            "success": true,
            "clicked": clean_ref,
            "url": data.get("url").cloned().unwrap_or(json!("")),
        }),
        Err(e) => connection_error_value(&e),
    }
}

/// Type text into an element by ref (hermes `camofox_type`); the returned
/// display value is redacted so secrets don't leak into history.
pub async fn type_text(task_id: &str, element: &str, text: &str, guard: bool) -> Value {
    let session = get_session(task_id);
    if session.tab_id.is_none() {
        return no_session_error();
    }
    if let Some(blocked) = private_page_block(&session, guard, "type").await {
        return blocked;
    }
    let clean_ref = element.trim_start_matches('@').to_string();
    let tab_id = session.tab_id.clone().unwrap_or_default();
    match post(
        &format!("/tabs/{tab_id}/type"),
        json!({"userId": session.user_id, "ref": clean_ref, "text": text}),
        None,
    )
    .await
    {
        Ok(_) => json!({
            "success": true,
            "typed": crate::browser::guard::redact_value(json!(text)),
            "element": clean_ref,
        }),
        Err(e) => connection_error_value(&e),
    }
}

/// Scroll the page (hermes `camofox_scroll`; Camofox scrolls by direction).
pub async fn scroll(task_id: &str, direction: &str) -> Value {
    let session = get_session(task_id);
    if session.tab_id.is_none() {
        return no_session_error();
    }
    let tab_id = session.tab_id.clone().unwrap_or_default();
    match post(
        &format!("/tabs/{tab_id}/scroll"),
        json!({"userId": session.user_id, "direction": direction}),
        None,
    )
    .await
    {
        Ok(_) => json!({"success": true, "scrolled": direction}),
        Err(e) => connection_error_value(&e),
    }
}

/// Navigate back (hermes `camofox_back`).
pub async fn back(task_id: &str) -> Value {
    let session = get_session(task_id);
    if session.tab_id.is_none() {
        return no_session_error();
    }
    let tab_id = session.tab_id.clone().unwrap_or_default();
    match post(
        &format!("/tabs/{tab_id}/back"),
        json!({"userId": session.user_id}),
        None,
    )
    .await
    {
        Ok(data) => json!({"success": true, "url": data.get("url").cloned().unwrap_or(json!(""))}),
        Err(e) => connection_error_value(&e),
    }
}

/// Press a keyboard key (hermes `camofox_press`).
pub async fn press(task_id: &str, key: &str, guard: bool) -> Value {
    let session = get_session(task_id);
    if session.tab_id.is_none() {
        return no_session_error();
    }
    if let Some(blocked) = private_page_block(&session, guard, "press").await {
        return blocked;
    }
    let tab_id = session.tab_id.clone().unwrap_or_default();
    match post(
        &format!("/tabs/{tab_id}/press"),
        json!({"userId": session.user_id, "key": key}),
        None,
    )
    .await
    {
        Ok(_) => json!({"success": true, "pressed": key}),
        Err(e) => connection_error_value(&e),
    }
}

/// Close the session (hermes `camofox_close`).
pub async fn close(task_id: &str) -> Value {
    let Some(session) = drop_session(task_id) else {
        return json!({"success": true, "closed": true});
    };
    match delete(&format!("/sessions/{}", session.user_id)).await {
        Ok(_) => json!({"success": true, "closed": true}),
        Err(e) => json!({"success": true, "closed": true, "warning": e.to_string()}),
    }
}

/// Extract images from the snapshot text (Camofox has no /images endpoint;
/// hermes parses `img` nodes + following `/url:` lines).
pub fn extract_images_from_snapshot(snapshot: &str) -> Vec<Value> {
    let mut images = Vec::new();
    let lines: Vec<&str> = snapshot.lines().collect();
    let img_re = regex::Regex::new(r#"img\s+"([^"]*)""#).expect("static regex compiles");
    let url_re = regex::Regex::new(r"/url:\s*(\S+)").expect("static regex compiles");
    for (i, line) in lines.iter().enumerate() {
        let stripped = line.trim();
        if stripped.starts_with("- img ") || stripped.starts_with("img ") {
            let alt = img_re
                .captures(stripped)
                .and_then(|c| c.get(1))
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            let mut src = String::new();
            if i + 1 < lines.len() {
                if let Some(m) = url_re.captures(lines[i + 1].trim()) {
                    if let Some(group) = m.get(1) {
                        src = group.as_str().to_string();
                    }
                }
            }
            if !alt.is_empty() || !src.is_empty() {
                images.push(json!({"src": src, "alt": alt}));
            }
        }
    }
    images
}

/// List images on the current page (hermes `camofox_get_images`).
pub async fn get_images(task_id: &str, guard: bool) -> Value {
    let session = get_session(task_id);
    if session.tab_id.is_none() {
        return no_session_error();
    }
    if let Some(blocked) = private_page_block(&session, guard, "extract page images").await {
        return blocked;
    }
    let tab_id = session.tab_id.clone().unwrap_or_default();
    let query = vec![("userId", session.user_id.clone())];
    match get(&format!("/tabs/{tab_id}/snapshot"), &query, None).await {
        Ok(data) => {
            let text = data.get("snapshot").and_then(|v| v.as_str()).unwrap_or("");
            let images = extract_images_from_snapshot(text);
            json!({"success": true, "images": images, "count": images.len()})
        }
        Err(e) => connection_error_value(&e),
    }
}

/// Screenshot + vision analysis via Camofox (hermes `camofox_vision`).
pub async fn vision(
    task_id: &str,
    prompt: &str,
    provider: std::sync::Arc<dyn crate::provider::Provider>,
    guard: bool,
) -> Value {
    let session = get_session(task_id);
    if session.tab_id.is_none() {
        return no_session_error();
    }
    if let Some(blocked) = private_page_block(&session, guard, "capture a screenshot").await {
        return blocked;
    }
    let tab_id = session.tab_id.clone().unwrap_or_default();
    let query = vec![("userId", session.user_id.clone())];
    let bytes = match get_bytes(&format!("/tabs/{tab_id}/screenshot"), &query).await {
        Ok(bytes) => bytes,
        Err(e) => return connection_error_value(&e),
    };
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    let image_url = format!("data:image/png;base64,{b64}");
    match provider.analyze_image(prompt, &image_url).await {
        Ok(analysis) => json!({"success": true, "analysis": analysis}),
        Err(e) => error_value(format!("vision provider: {e}")),
    }
}

/// Console capture is not available on the Camofox backend (hermes
/// `camofox_console`).
pub fn console_unavailable() -> Value {
    json!({
        "success": true,
        "console_messages": [],
        "js_errors": [],
        "total_messages": 0,
        "total_errors": 0,
        "note": "Console log capture is not available with the Camofox backend. Use browser_snapshot or browser_vision to inspect page state.",
    })
}

/// Unsupported-by-Camofox payload (raw CDP, dialogs).
pub fn unsupported(feature: &str) -> Value {
    error_value(format!(
        "{feature} is not supported by the Camofox backend (REST anti-detect browser). Use snapshot/click/type/scroll/press/vision instead."
    ))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests that mutate Camofox env vars share this lock (process-global).
    fn env_lock() -> MutexGuard<'static, ()> {
        // Share the process-wide env lock so env-mutating tests across
        // modules (models_dev, banner, browser::connect, camofox) never
        // interleave.
        crate::models_dev::test_env_lock()
    }

    fn clear_env() {
        for var in [
            "CAMOFOX_URL",
            "CAMOFOX_API_KEY",
            "CAMOFOX_USER_ID",
            "CAMOFOX_SESSION_KEY",
            "CAMOFOX_ADOPT_EXISTING_TAB",
            "CAMOFOX_REWRITE_LOOPBACK_URLS",
            "CAMOFOX_LOOPBACK_HOST_ALIAS",
            "CAMOFOX_MANAGED_PERSISTENCE",
            "ULNCLAW_BROWSER_CDP",
        ] {
            std::env::remove_var(var);
        }
    }

    #[test]
    fn loopback_hostname_detection() {
        assert!(is_loopback_hostname("localhost"));
        assert!(is_loopback_hostname("127.0.0.1"));
        assert!(is_loopback_hostname("127.8.9.10"));
        assert!(is_loopback_hostname("::1"));
        assert!(is_loopback_hostname("[::1]"));
        assert!(!is_loopback_hostname("example.com"));
        assert!(!is_loopback_hostname("192.168.1.1"));
        assert!(!is_loopback_hostname(""));
    }

    #[test]
    fn rewrite_disabled_by_default() {
        let _guard = env_lock();
        clear_env();
        std::env::set_var("CAMOFOX_URL", "http://127.0.0.1:9377");
        let (url, info) = rewrite_loopback_url("http://127.0.0.1:3000/app");
        assert_eq!(url, "http://127.0.0.1:3000/app");
        assert!(info.is_none());
        clear_env();
    }

    #[test]
    fn rewrite_loopback_for_docker() {
        let _guard = env_lock();
        clear_env();
        std::env::set_var("CAMOFOX_URL", "http://127.0.0.1:9377");
        std::env::set_var("CAMOFOX_REWRITE_LOOPBACK_URLS", "true");

        let (url, info) = rewrite_loopback_url("http://127.0.0.1:3000/app?x=1");
        assert_eq!(url, "http://host.docker.internal:3000/app?x=1");
        let info = info.unwrap();
        assert_eq!(info["from"], "127.0.0.1");
        assert_eq!(info["to"], "host.docker.internal");

        // localhost + custom alias
        std::env::set_var("CAMOFOX_LOOPBACK_HOST_ALIAS", "host.lan");
        let (url, _) = rewrite_loopback_url("https://localhost/x");
        assert_eq!(url, "https://host.lan/x");

        // userinfo preserved
        let (url, _) = rewrite_loopback_url("http://user:pw@127.0.0.1:8080/p");
        assert_eq!(url, "http://user:pw@host.lan:8080/p");

        // public URLs untouched
        let (url, info) = rewrite_loopback_url("https://example.com/");
        assert_eq!(url, "https://example.com/");
        assert!(info.is_none());
        // non-http schemes untouched
        let (url, info) = rewrite_loopback_url("ftp://127.0.0.1/x");
        assert_eq!(url, "ftp://127.0.0.1/x");
        assert!(info.is_none());
        clear_env();
    }

    #[test]
    fn camofox_mode_gating() {
        let _guard = env_lock();
        clear_env();
        crate::browser::clear_cdp_override();
        assert!(!is_camofox_mode());
        std::env::set_var("CAMOFOX_URL", "http://127.0.0.1:9377/");
        assert!(is_camofox_mode());
        assert_eq!(camofox_url().as_deref(), Some("http://127.0.0.1:9377"));
        // A configured CDP endpoint takes priority over Camofox.
        std::env::set_var("ULNCLAW_BROWSER_CDP", "http://127.0.0.1:9222");
        assert!(!is_camofox_mode());
        // auto/managed CDP values do not suppress Camofox.
        std::env::set_var("ULNCLAW_BROWSER_CDP", "auto");
        assert!(is_camofox_mode());
        clear_env();
    }

    #[test]
    fn session_identity_override_and_cleanup() {
        let _guard = env_lock();
        clear_env();
        sessions().clear();
        std::env::set_var("CAMOFOX_URL", "http://127.0.0.1:9377");
        std::env::set_var("CAMOFOX_USER_ID", "shared-profile");
        std::env::set_var("CAMOFOX_SESSION_KEY", "my-key");

        let session = get_session("task-abc");
        assert_eq!(session.user_id, "shared-profile");
        assert_eq!(session.session_key, "my-key");
        assert!(session.tab_id.is_none());

        // Ephemeral sessions get random user ids.
        clear_env();
        std::env::set_var("CAMOFOX_URL", "http://127.0.0.1:9377");
        let ephemeral = get_session("other");
        assert!(ephemeral.user_id.starts_with("ulnclaw_"));
        assert_eq!(ephemeral.session_key, "task_other");

        // Soft cleanup only releases identity-override sessions.
        assert!(!soft_cleanup("other"));
        std::env::set_var("CAMOFOX_USER_ID", "shared-profile");
        assert!(soft_cleanup("task-abc"));
        assert!(sessions().get("task-abc").is_none());
        clear_env();
        sessions().clear();
    }

    #[test]
    fn managed_identity_is_deterministic_and_scoped() {
        // Identity derives from $ULNCLAW_HOME — hold the env lock so no
        // other test mutates it mid-assertion.
        let _guard = env_lock();
        let (user_a, session_a) = managed_identity("task-1");
        let (user_a2, session_a2) = managed_identity("task-1");
        assert_eq!(user_a, user_a2, "same profile + task = same identity");
        assert_eq!(session_a, session_a2);
        assert!(user_a.starts_with("ulnclaw_"));
        assert!(session_a.starts_with("task_"));
        assert_eq!(user_a.len(), "ulnclaw_".len() + 10);
        assert_eq!(session_a.len(), "task_".len() + 16);

        // Same profile, different task: same user, different session key.
        let (user_b, session_b) = managed_identity("task-2");
        assert_eq!(user_a, user_b);
        assert_ne!(session_a, session_b);

        // Empty task id maps to the "default" scope.
        assert_eq!(managed_identity("").1, managed_identity("default").1);
    }

    #[test]
    fn managed_persistence_sessions_and_soft_cleanup() {
        let _guard = env_lock();
        clear_env();
        sessions().clear();
        std::env::set_var("CAMOFOX_URL", "http://127.0.0.1:9377");
        assert!(!managed_persistence_enabled());
        assert!(!soft_cleanup("mp-task"));

        std::env::set_var("CAMOFOX_MANAGED_PERSISTENCE", "true");
        assert!(managed_persistence_enabled());

        let session = get_session("mp-task");
        let (expected_user, expected_key) = managed_identity("mp-task");
        assert_eq!(session.user_id, expected_user);
        assert_eq!(session.session_key, expected_key);

        // Soft cleanup releases the local entry but keeps the profile.
        assert!(soft_cleanup("mp-task"));
        assert!(sessions().get("mp-task").is_none());
        // Recreating the session yields the SAME stable identity.
        let again = get_session("mp-task");
        assert_eq!(again.user_id, expected_user);

        clear_env();
        sessions().clear();
    }

    #[test]
    fn image_extraction_from_snapshot() {
        let snapshot = "\n- heading \"Gallery\" [level=2]\n- img \"logo\"\n  /url: https://cdn.example/logo.png\n- img \"\"\n  /url: https://cdn.example/banner.jpg\n- button \"Submit\"";
        let images = extract_images_from_snapshot(snapshot);
        assert_eq!(images.len(), 2);
        assert_eq!(images[0]["alt"], "logo");
        assert_eq!(images[0]["src"], "https://cdn.example/logo.png");
        assert_eq!(images[1]["src"], "https://cdn.example/banner.jpg");
        assert!(extract_images_from_snapshot("- button \"x\"").is_empty());
    }

    #[test]
    fn console_and_unsupported_payloads() {
        let console = console_unavailable();
        assert_eq!(console["success"], true);
        assert!(console["note"].as_str().unwrap().contains("not available"));
        let cdp = unsupported("browser_cdp");
        assert_eq!(cdp["success"], false);
        assert!(cdp["error"].as_str().unwrap().contains("browser_cdp"));
    }

    async fn mock_health() -> axum::Json<Value> {
        axum::Json(json!({"status": "ok", "vncPort": 5901}))
    }

    /// Minimal in-process Camofox server for REST-flow tests.
    async fn spawn_mock_camofox(evaluate_result: &'static str) -> String {
        let app = axum::Router::new()
            .route("/health", axum::routing::get(mock_health))
            .route(
                "/tabs",
                axum::routing::post(
                    |axum::Json(body): axum::Json<Value>| async move {
                        axum::Json(json!({"tabId": "tab-1", "echo_user": body.get("userId").cloned()}))
                    },
                ),
            )
            .route(
                "/tabs/:tab/navigate",
                axum::routing::post(
                    |axum::extract::Path(_tab): axum::extract::Path<String>,
                     axum::Json(body): axum::Json<Value>| async move {
                        axum::Json(json!({
                            "ok": true,
                            "url": body.get("url").cloned().unwrap_or(json!("")),
                            "title": "Mock"
                        }))
                    },
                ),
            )
            .route(
                "/tabs/:tab/snapshot",
                axum::routing::get(|| async {
                    axum::Json(json!({
                        "snapshot": "- heading \"Hi\"\n- img \"logo\"\n  /url: https://cdn.example/logo.png\n- button \"Go\"",
                        "refsCount": 3
                    }))
                }),
            )
            .route(
                "/tabs/:tab/click",
                axum::routing::post(|| async {
                    axum::Json(json!({"url": "https://example.com/"}))
                }),
            )
            .route(
                "/tabs/:tab/evaluate",
                axum::routing::post(move || async move {
                    axum::Json(json!({"result": evaluate_result}))
                }),
            )
            .route(
                "/sessions/:user",
                axum::routing::delete(|| async { axum::Json(json!({})) }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn camofox_rest_flow_against_mock_server() {
        let _guard = env_lock();
        clear_env();
        sessions().clear();
        *vnc_slot() = (false, None);

        let base = spawn_mock_camofox("https://example.com/").await;
        std::env::set_var("CAMOFOX_URL", &base);

        // Health + VNC discovery.
        assert!(check_available().await);
        assert_eq!(
            vnc_url().await.as_deref(),
            Some(format!("http://{}:5901", url_host(&base).unwrap()).as_str())
        );

        // First navigate creates a tab (hermes: fresh-tab results carry no
        // title) and auto-snapshots so the model can act immediately.
        let nav = navigate("flow-task", "https://example.com/", false).await;
        assert_eq!(nav["success"], true, "navigate failed: {nav}");
        assert_eq!(nav["url"], "https://example.com/");
        assert_eq!(nav["element_count"], 3);

        // Second navigate reuses the tab via /tabs/:id/navigate.
        let nav = navigate("flow-task", "https://example.com/two", false).await;
        assert_eq!(nav["success"], true);
        assert_eq!(nav["title"], "Mock");
        assert_eq!(nav["url"], "https://example.com/two");
        assert_eq!(nav["element_count"], 3);
        assert!(nav["snapshot"].as_str().unwrap().contains("heading"));
        assert!(nav["vnc_url"].as_str().unwrap().contains(":5901"));

        // Subsequent calls reuse the tab; snapshot/click/images work.
        let snap = snapshot("flow-task", false).await;
        assert_eq!(snap["success"], true);
        assert_eq!(snap["element_count"], 3);

        let clicked = click("flow-task", "@5", false).await;
        assert_eq!(clicked["success"], true);
        assert_eq!(clicked["clicked"], "5");

        let images = get_images("flow-task", false).await;
        assert_eq!(images["count"], 1);
        assert_eq!(images["images"][0]["src"], "https://cdn.example/logo.png");

        let typed = type_text("flow-task", "5", "hello", false).await;
        // no /type route on the mock → error surfaces cleanly
        assert_eq!(typed["success"], false);

        // No session without a navigate in this task.
        assert_eq!(snapshot("other-task", false).await["success"], false);

        // Close drops the local session and DELETEs server-side.
        let closed = close("flow-task").await;
        assert_eq!(closed["success"], true);
        assert!(sessions().get("flow-task").is_none());

        clear_env();
        sessions().clear();
        *vnc_slot() = (false, None);
    }

    #[tokio::test]
    async fn camofox_private_page_guard_blocks_reads() {
        let _guard = env_lock();
        clear_env();
        sessions().clear();

        // The mock page sits on the cloud-metadata floor; guarded reads block.
        let base = spawn_mock_camofox("http://169.254.169.254/latest/meta-data").await;
        std::env::set_var("CAMOFOX_URL", &base);

        let nav = navigate("guard-task", "http://169.254.169.254/", false).await;
        assert_eq!(nav["success"], true);

        let snap = snapshot("guard-task", true).await;
        assert_eq!(snap["success"], false, "snapshot must be blocked: {snap}");
        assert!(snap["error"]
            .as_str()
            .unwrap()
            .contains("private or internal"));

        // Guard off → reads pass through.
        let snap = snapshot("guard-task", false).await;
        assert_eq!(snap["success"], true);

        clear_env();
        sessions().clear();
        *vnc_slot() = (false, None);
    }

    #[test]
    fn url_host_parsing() {
        assert_eq!(
            url_host("http://127.0.0.1:9377/x").as_deref(),
            Some("127.0.0.1")
        );
        assert_eq!(
            url_host("https://camofox.example").as_deref(),
            Some("camofox.example")
        );
        assert_eq!(url_host("http://[::1]:9377").as_deref(), Some("::1"));
    }
}

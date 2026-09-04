//! Cloud browser providers — port of hermes `agent/browser_provider.py` +
//! `agent/browser_registry.py` + `plugins/browser/{browserbase,browser_use,
//! firecrawl}`.
//!
//! Pluggable cloud browser backends service the `browser_*` tools when no
//! local CDP endpoint is configured: the active provider (selected via
//! `[browser] cloud_provider` in config.toml, or the hermes legacy
//! availability walk) creates a remote browser session and hands back a CDP
//! websocket URL that the shared browser session layer
//! (`super::with_session`) consumes like any other endpoint.
//!
//! Resolution rules mirror hermes `browser_registry._resolve`:
//!   1. Explicit `"local"` disables cloud mode entirely.
//!   2. An explicit provider name wins regardless of availability, so the
//!      user gets a precise "missing credentials" error instead of a silent
//!      backend switch.
//!   3. Otherwise walk the legacy preference order (`browser-use` →
//!      `browserbase`) filtered by availability. Firecrawl is deliberately
//!      absent from the walk — its API key is shared with web extract, so a
//!      fresh install with `FIRECRAWL_API_KEY` must not be silently routed
//!      to a paid cloud browser.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::{json, Value};

/// Boxed future type used by the provider trait methods.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Session metadata returned by a cloud browser provider (hermes session
/// metadata contract; `bb_session_id` kept verbatim as the provider session
/// ID key regardless of which provider is in use).
#[derive(Debug, Clone, serde::Serialize)]
pub struct CloudSessionInfo {
    /// Unique session name (hermes `hermes_<task>_<hex>`; ulnclaw prefix).
    pub session_name: String,
    /// Provider session ID (for close/cleanup).
    pub bb_session_id: String,
    /// CDP websocket URL.
    pub cdp_url: String,
    /// Optional provider-authoritative expiry (ISO timestamp).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    /// Feature flags that were enabled for this session.
    pub features: BTreeMap<String, bool>,
    /// Optional managed-gateway billing key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_call_id: Option<String>,
}

/// A cloud browser backend (hermes `BrowserProvider` ABC).
pub trait CloudBrowserProvider: Send + Sync {
    /// Stable short identifier used in `[browser] cloud_provider`.
    fn name(&self) -> &'static str;

    /// Human-readable label (hermes `display_name`).
    fn display_name(&self) -> &'static str {
        self.name()
    }

    /// True when this provider can service calls. Must be a cheap check
    /// (env var present, token readable) — no network calls.
    fn is_available(&self) -> bool;

    /// Create a cloud browser session and return its metadata.
    fn create_session(&self, task_id: &str) -> BoxFuture<'_, Result<CloudSessionInfo, String>>;

    /// Release / terminate a session by its provider session ID. Returns
    /// false on failure; should not raise.
    fn close_session(&self, session_id: &str) -> BoxFuture<'_, bool>;

    /// Best-effort cleanup for shutdown paths; never raises.
    fn emergency_cleanup(&self, session_id: &str) -> BoxFuture<'_, ()>;
}

// ---------------------------------------------------------------------------
// Registry (hermes agent/browser_registry.py)
// ---------------------------------------------------------------------------

fn registry_slot() -> &'static Mutex<HashMap<String, Arc<dyn CloudBrowserProvider>>> {
    static SLOT: OnceLock<Mutex<HashMap<String, Arc<dyn CloudBrowserProvider>>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(HashMap::new()))
}

static BUILTINS_REGISTERED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// The built-in cloud backends (hermes `plugins/browser/*`, auto-loaded as
/// `kind: backend`).
fn ensure_builtins() {
    if BUILTINS_REGISTERED
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        )
        .is_ok()
    {
        register_provider(Arc::new(BrowserUseProvider));
        register_provider(Arc::new(BrowserbaseProvider));
        register_provider(Arc::new(FirecrawlProvider));
    }
}

/// Register (or replace) a provider (hermes `register_provider`).
pub fn register_provider(provider: Arc<dyn CloudBrowserProvider>) {
    let name = provider.name().to_string();
    registry_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(name, provider);
}

/// All registered providers, sorted by name (hermes `list_providers`).
pub fn list_providers() -> Vec<Arc<dyn CloudBrowserProvider>> {
    ensure_builtins();
    let mut providers: Vec<Arc<dyn CloudBrowserProvider>> = registry_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values()
        .cloned()
        .collect();
    providers.sort_by_key(|p| p.name().to_string());
    providers
}

/// Look up a provider by name (hermes `get_provider`).
pub fn get_provider(name: &str) -> Option<Arc<dyn CloudBrowserProvider>> {
    ensure_builtins();
    registry_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(name)
        .cloned()
}

/// Legacy auto-detect order (hermes `_LEGACY_PREFERENCE`). Firecrawl is
/// deliberately absent: it shares its API key with web extract, so users who
/// set `FIRECRAWL_API_KEY` for extraction must not be silently routed to a
/// paid cloud browser.
const LEGACY_PREFERENCE: &[&str] = &["browser-use", "browserbase"];

/// Resolve the active browser provider (hermes `browser_registry._resolve`).
///
/// `configured` is the raw `[browser] cloud_provider` value: `"local"`
/// disables cloud mode; an explicit name wins regardless of availability;
/// otherwise the legacy preference walk filtered by availability runs.
pub fn resolve_cloud_provider(configured: Option<&str>) -> Option<Arc<dyn CloudBrowserProvider>> {
    ensure_builtins();
    let configured = configured
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty());

    // 1. Explicit "local" short-circuit.
    if configured.as_deref() == Some("local") {
        return None;
    }

    // 2. Explicit config wins — returned regardless of is_available() so the
    //    user gets a precise downstream error instead of a silent switch.
    if let Some(name) = configured.as_deref() {
        if let Some(provider) = get_provider(name) {
            return Some(provider);
        }
        tracing::debug!(
            "browser cloud_provider '{name}' configured but not registered; \
             falling back to auto-detect"
        );
    }

    // 3. Legacy preference walk, filtered by availability.
    for name in LEGACY_PREFERENCE {
        if let Some(provider) = get_provider(name) {
            if is_available_safe(provider.as_ref()) {
                return Some(provider);
            }
        }
    }

    None
}

/// Wrap `is_available()` so a buggy provider cannot kill resolution
/// (hermes `_is_available_safe`).
fn is_available_safe(provider: &dyn CloudBrowserProvider) -> bool {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| provider.is_available()))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Session expiry (hermes browser_tool._session_expiry_timestamp /
// _session_has_expired)
// ---------------------------------------------------------------------------

/// Provider-authoritative session expiry as epoch seconds. Unknown or
/// malformed values are treated as having no known expiry, preserving the
/// lifecycle for providers without an expiry contract.
pub fn session_expiry_timestamp(expires_at: Option<&str>) -> Option<f64> {
    let raw = expires_at?.trim();
    if raw.is_empty() {
        return None;
    }
    // Hermes accepts numeric epoch values too.
    if let Ok(epoch) = raw.parse::<f64>() {
        return Some(epoch);
    }
    let normalized = if raw.ends_with('Z') || raw.ends_with('z') {
        format!("{}+00:00", &raw[..raw.len() - 1])
    } else {
        raw.to_string()
    };
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&normalized) {
        return Some(dt.timestamp() as f64);
    }
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(&normalized, "%Y-%m-%dT%H:%M:%S%.f") {
        // Naive timestamps are treated as UTC (hermes semantics).
        return Some(naive.and_utc().timestamp() as f64);
    }
    tracing::warn!("ignoring invalid cloud browser session expiry timestamp");
    None
}

/// Whether a cached cloud session crossed its provider deadline.
pub fn session_has_expired(info: &CloudSessionInfo) -> bool {
    match session_expiry_timestamp(info.expires_at.as_deref()) {
        None => false,
        Some(deadline) => {
            let now = chrono::Utc::now().timestamp() as f64;
            now >= deadline
        }
    }
}

fn session_name(task_id: &str) -> String {
    let suffix = &uuid::Uuid::new_v4().simple().to_string()[..8];
    format!("ulnclaw_{task_id}_{suffix}")
}

fn http_client(timeout: Duration) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| format!("HTTP client init failed: {e}"))
}

fn env_trimmed(name: &str) -> Option<String> {
    crate::config::get_env_value(name)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

// ---------------------------------------------------------------------------
// Browserbase (hermes plugins/browser/browserbase)
// ---------------------------------------------------------------------------

/// Browserbase (https://browserbase.com) cloud browser backend. Direct
/// credentials only — managed-Nous-gateway support lives on the Browser Use
/// provider (hermes parity).
pub struct BrowserbaseProvider;

impl BrowserbaseProvider {
    fn config_or_none() -> Option<(String, String, String)> {
        let api_key = env_trimmed("BROWSERBASE_API_KEY")?;
        let project_id = env_trimmed("BROWSERBASE_PROJECT_ID")?;
        let base_url = env_trimmed("BROWSERBASE_BASE_URL")
            .unwrap_or_else(|| "https://api.browserbase.com".to_string());
        Some((
            api_key,
            project_id,
            base_url.trim_end_matches('/').to_string(),
        ))
    }

    fn config() -> Result<(String, String, String), String> {
        Self::config_or_none().ok_or_else(|| {
            "Browserbase requires BROWSERBASE_API_KEY and BROWSERBASE_PROJECT_ID \
             environment variables."
                .to_string()
        })
    }

    async fn post_release(
        base_url: &str,
        api_key: &str,
        project_id: &str,
        session_id: &str,
        timeout: Duration,
    ) -> bool {
        let Ok(client) = http_client(timeout) else {
            return false;
        };
        match client
            .post(format!("{base_url}/v1/sessions/{session_id}"))
            .header("X-BB-API-Key", api_key)
            .header("Content-Type", "application/json")
            .json(&json!({"projectId": project_id, "status": "REQUEST_RELEASE"}))
            .send()
            .await
        {
            Ok(resp) if matches!(resp.status().as_u16(), 200 | 201 | 204) => true,
            Ok(resp) => {
                let status = resp.status().as_u16();
                let body = resp.text().await.unwrap_or_default();
                tracing::warn!(
                    "failed to close Browserbase session {session_id}: HTTP {status} - {}",
                    &body.chars().take(200).collect::<String>()
                );
                false
            }
            Err(e) => {
                tracing::error!("exception closing Browserbase session {session_id}: {e}");
                false
            }
        }
    }
}

impl CloudBrowserProvider for BrowserbaseProvider {
    fn name(&self) -> &'static str {
        "browserbase"
    }

    fn display_name(&self) -> &'static str {
        "Browserbase"
    }

    fn is_available(&self) -> bool {
        Self::config_or_none().is_some()
    }

    fn create_session(&self, task_id: &str) -> BoxFuture<'_, Result<CloudSessionInfo, String>> {
        let task_id = task_id.to_string();
        Box::pin(async move {
            let (api_key, project_id, base_url) = Self::config()?;

            // Optional env-var knobs (hermes defaults).
            let env_flag = |name: &str, default: &str| -> String {
                env_trimmed(name)
                    .unwrap_or_else(|| default.to_string())
                    .to_ascii_lowercase()
            };
            let enable_proxies = env_flag("BROWSERBASE_PROXIES", "true") != "false";
            let enable_advanced_stealth =
                env_flag("BROWSERBASE_ADVANCED_STEALTH", "false") == "true";
            let enable_keep_alive = env_flag("BROWSERBASE_KEEP_ALIVE", "true") != "false";
            let custom_timeout = env_trimmed("BROWSERBASE_SESSION_TIMEOUT");

            let mut features_enabled: BTreeMap<String, bool> = BTreeMap::new();
            features_enabled.insert("basic_stealth".to_string(), true);
            features_enabled.insert("proxies".to_string(), false);
            features_enabled.insert("advanced_stealth".to_string(), false);
            features_enabled.insert("keep_alive".to_string(), false);
            features_enabled.insert("custom_timeout".to_string(), false);

            let mut session_config = json!({"projectId": project_id});
            if enable_keep_alive {
                session_config["keepAlive"] = json!(true);
            }
            if let Some(raw) = custom_timeout.as_deref() {
                match raw.parse::<i64>() {
                    Ok(v) if v > 0 => session_config["timeout"] = json!(v),
                    _ => tracing::warn!("invalid BROWSERBASE_SESSION_TIMEOUT value: {raw}"),
                }
            }
            if enable_proxies {
                session_config["proxies"] = json!(true);
            }
            if enable_advanced_stealth {
                session_config["browserSettings"] = json!({"advancedStealth": true});
            }

            let client = http_client(Duration::from_secs(30))?;
            let url = format!("{base_url}/v1/sessions");
            let post = |body: &Value| {
                client
                    .post(url.clone())
                    .header("X-BB-API-Key", api_key.clone())
                    .header("Content-Type", "application/json")
                    .json(body)
                    .send()
            };

            let mut resp = post(&session_config)
                .await
                .map_err(|e| format!("Browserbase API connection failed: {e}"))?;

            // Handle 402 — paid features unavailable (hermes fallback chain:
            // drop keepAlive first, then proxies).
            let mut keepalive_fallback = false;
            let mut proxies_fallback = false;
            if resp.status().as_u16() == 402
                && enable_keep_alive
                && session_config.get("keepAlive").is_some()
            {
                keepalive_fallback = true;
                tracing::warn!(
                    "keepAlive may require paid plan (402), retrying without it. \
                     Sessions may timeout during long operations."
                );
                session_config.as_object_mut().unwrap().remove("keepAlive");
                resp = post(&session_config)
                    .await
                    .map_err(|e| format!("Browserbase API connection failed: {e}"))?;
            }
            if resp.status().as_u16() == 402
                && enable_proxies
                && session_config.get("proxies").is_some()
            {
                proxies_fallback = true;
                tracing::warn!(
                    "proxies unavailable (402), retrying without proxies. \
                     Bot detection may be less effective."
                );
                session_config.as_object_mut().unwrap().remove("proxies");
                resp = post(&session_config)
                    .await
                    .map_err(|e| format!("Browserbase API connection failed: {e}"))?;
            }

            let status = resp.status().as_u16();
            if !resp.status().is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(format!(
                    "Failed to create Browserbase session: {status} {body}"
                ));
            }

            let session_data: Value = resp
                .json()
                .await
                .map_err(|e| format!("Failed to parse Browserbase session response: {e}"))?;
            let id = session_data
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| "Browserbase session response missing 'id'".to_string())?
                .to_string();
            let cdp_url = session_data
                .get("connectUrl")
                .and_then(Value::as_str)
                .ok_or_else(|| "Browserbase session response missing 'connectUrl'".to_string())?
                .to_string();

            if enable_proxies && !proxies_fallback {
                features_enabled.insert("proxies".to_string(), true);
            }
            if enable_advanced_stealth {
                features_enabled.insert("advanced_stealth".to_string(), true);
            }
            if enable_keep_alive && !keepalive_fallback {
                features_enabled.insert("keep_alive".to_string(), true);
            }
            if custom_timeout.is_some() && session_config.get("timeout").is_some() {
                features_enabled.insert("custom_timeout".to_string(), true);
            }

            let name = session_name(&task_id);
            let feature_str = features_enabled
                .iter()
                .filter(|(_, v)| **v)
                .map(|(k, _)| k.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            tracing::info!("created Browserbase session {name} with features: {feature_str}");

            Ok(CloudSessionInfo {
                session_name: name,
                bb_session_id: id,
                cdp_url,
                expires_at: None,
                features: features_enabled,
                external_call_id: None,
            })
        })
    }

    fn close_session(&self, session_id: &str) -> BoxFuture<'_, bool> {
        let session_id = session_id.to_string();
        Box::pin(async move {
            let Ok((api_key, project_id, base_url)) = Self::config() else {
                tracing::warn!(
                    "cannot close Browserbase session {session_id} — missing credentials"
                );
                return false;
            };
            Self::post_release(
                &base_url,
                &api_key,
                &project_id,
                &session_id,
                Duration::from_secs(10),
            )
            .await
        })
    }

    fn emergency_cleanup(&self, session_id: &str) -> BoxFuture<'_, ()> {
        let session_id = session_id.to_string();
        Box::pin(async move {
            let Some((api_key, project_id, base_url)) = Self::config_or_none() else {
                tracing::warn!(
                    "cannot emergency-cleanup Browserbase session {session_id} — missing credentials"
                );
                return;
            };
            Self::post_release(
                &base_url,
                &api_key,
                &project_id,
                &session_id,
                Duration::from_secs(5),
            )
            .await;
        })
    }
}

// ---------------------------------------------------------------------------
// Browser Use (hermes plugins/browser/browser_use)
// ---------------------------------------------------------------------------

const BROWSER_USE_BASE_URL: &str = "https://api.browser-use.com/api/v3";
const DEFAULT_MANAGED_TIMEOUT_MINUTES: u32 = 5;
const DEFAULT_MANAGED_PROXY_COUNTRY_CODE: &str = "us";

fn pending_create_keys() -> &'static Mutex<HashMap<String, String>> {
    static SLOT: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Idempotency tracking for managed-mode session creation (hermes
/// `_get_or_create_pending_create_key`): the managed gateway returns 409
/// "already in progress" on retried POSTs, so the original key is forwarded
/// for deduplication.
fn get_or_create_pending_create_key(task_id: &str) -> String {
    let mut map = pending_create_keys()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = map.get(task_id) {
        return existing.clone();
    }
    let created = format!(
        "browser-use-session-create:{}",
        uuid::Uuid::new_v4().simple()
    );
    map.insert(task_id.to_string(), created.clone());
    created
}

fn clear_pending_create_key(task_id: &str) {
    pending_create_keys()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(task_id);
}

/// Decide whether to keep the idempotency key after a failed create
/// (hermes `_should_preserve_pending_create_key`): preserve on 5xx or on a
/// 409 "already in progress" (retryable); drop on any other 4xx.
fn should_preserve_pending_create_key(status: u16, body: &str) -> bool {
    if status >= 500 {
        return true;
    }
    if status != 409 {
        return false;
    }
    let Ok(payload) = serde_json::from_str::<Value>(body) else {
        return false;
    };
    let message = payload
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    message.contains("already in progress")
}

/// Browser Use (https://browser-use.com) cloud browser backend. Dual auth:
/// prefers a direct `BROWSER_USE_API_KEY`, falling back to the managed Nous
/// tool gateway; `[browser] use_gateway` flips the order (hermes
/// `tool_gateway.browser: gateway`).
pub struct BrowserUseProvider;

struct BrowserUseConfig {
    api_key: String,
    base_url: String,
    managed_mode: bool,
}

impl BrowserUseProvider {
    fn prefers_gateway() -> bool {
        crate::config::UlncLawConfig::load(None)
            .map(|c| {
                c.browser
                    .use_gateway
                    .as_ref()
                    .map(|t| t.resolve(false))
                    .unwrap_or(false)
            })
            .unwrap_or(false)
    }

    fn config_or_none(refresh_token: bool) -> Option<BrowserUseConfig> {
        // Direct API key wins unless the user opted into the managed gateway.
        if let Some(api_key) = env_trimmed("BROWSER_USE_API_KEY") {
            if !Self::prefers_gateway() {
                let base_url = env_trimmed("BROWSER_USE_BASE_URL")
                    .unwrap_or_else(|| BROWSER_USE_BASE_URL.to_string());
                return Some(BrowserUseConfig {
                    api_key,
                    base_url: base_url.trim_end_matches('/').to_string(),
                    managed_mode: false,
                });
            }
        }

        // Managed Nous tool gateway. Keep availability scans off the
        // synchronous OAuth refresh path (peek vs read).
        if !crate::managed_gateway::managed_nous_tools_enabled() {
            return None;
        }
        let origin = crate::managed_gateway::build_vendor_gateway_url("browser-use").ok()?;
        let token = if refresh_token {
            crate::managed_gateway::read_nous_access_token()
        } else {
            crate::managed_gateway::peek_nous_access_token()
        }?;
        Some(BrowserUseConfig {
            api_key: token,
            base_url: origin.trim_end_matches('/').to_string(),
            managed_mode: true,
        })
    }

    fn config() -> Result<BrowserUseConfig, String> {
        Self::config_or_none(true).ok_or_else(|| {
            if crate::managed_gateway::managed_nous_tools_enabled() {
                "Browser Use requires either a direct BROWSER_USE_API_KEY credential \
                 or a managed Browser Use gateway configuration."
                    .to_string()
            } else {
                "Browser Use requires a direct BROWSER_USE_API_KEY credential.".to_string()
            }
        })
    }
}

impl CloudBrowserProvider for BrowserUseProvider {
    fn name(&self) -> &'static str {
        "browser-use"
    }

    fn display_name(&self) -> &'static str {
        "Browser Use"
    }

    fn is_available(&self) -> bool {
        Self::config_or_none(false).is_some()
    }

    fn create_session(&self, task_id: &str) -> BoxFuture<'_, Result<CloudSessionInfo, String>> {
        let task_id = task_id.to_string();
        Box::pin(async move {
            let config = Self::config()?;
            let managed_mode = config.managed_mode;

            let client = http_client(Duration::from_secs(30))?;
            let mut request = client
                .post(format!("{}/browsers", config.base_url))
                .header("Content-Type", "application/json")
                .header("X-Browser-Use-API-Key", config.api_key.clone());
            if managed_mode {
                request = request.header(
                    "X-Idempotency-Key",
                    get_or_create_pending_create_key(&task_id),
                );
            }

            // Keep gateway-backed sessions short so billing authorization does
            // not default to a long Browser-Use timeout when only a task-scoped
            // ephemeral browser is needed.
            let payload = if managed_mode {
                json!({
                    "timeout": DEFAULT_MANAGED_TIMEOUT_MINUTES,
                    "proxyCountryCode": DEFAULT_MANAGED_PROXY_COUNTRY_CODE,
                })
            } else {
                json!({})
            };

            let resp = match request.json(&payload).send().await {
                Ok(resp) => resp,
                Err(e) => {
                    // Managed mode: propagate raw so callers can retry with the
                    // preserved idempotency key. Direct mode: wrap into a clean
                    // error for end users.
                    return Err(if managed_mode {
                        format!("Browser Use gateway connection failed: {e}")
                    } else {
                        format!("Browser Use API connection failed: {e}")
                    });
                }
            };

            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            if !(200..300).contains(&status) {
                if managed_mode && !should_preserve_pending_create_key(status, &body) {
                    clear_pending_create_key(&task_id);
                }
                return Err(format!(
                    "Failed to create Browser Use session: {status} {body}"
                ));
            }
            if managed_mode {
                clear_pending_create_key(&task_id);
            }

            let session_data: Value = serde_json::from_str(&body)
                .map_err(|e| format!("Failed to parse Browser Use session response: {e}"))?;
            let id = session_data
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| "Browser Use session response missing 'id'".to_string())?
                .to_string();
            let cdp_url = session_data
                .get("cdpUrl")
                .or_else(|| session_data.get("connectUrl"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            // Browser Use sessions have a fixed server-side lifetime; preserve
            // the provider authority so expired endpoints get retired instead
            // of reconnected indefinitely.
            let expires_at = session_data
                .get("timeoutAt")
                .and_then(Value::as_str)
                .map(|s| s.to_string());

            let name = session_name(&task_id);
            tracing::info!("created Browser Use session {name}");

            Ok(CloudSessionInfo {
                session_name: name,
                bb_session_id: id,
                cdp_url,
                expires_at,
                features: BTreeMap::from([("browser_use".to_string(), true)]),
                external_call_id: None,
            })
        })
    }

    fn close_session(&self, session_id: &str) -> BoxFuture<'_, bool> {
        let session_id = session_id.to_string();
        Box::pin(async move {
            let Ok(config) = Self::config() else {
                tracing::warn!(
                    "cannot close Browser Use session {session_id} — missing credentials"
                );
                return false;
            };
            let Ok(client) = http_client(Duration::from_secs(10)) else {
                return false;
            };
            match client
                .patch(format!("{}/browsers/{session_id}", config.base_url))
                .header("Content-Type", "application/json")
                .header("X-Browser-Use-API-Key", config.api_key)
                .json(&json!({"action": "stop"}))
                .send()
                .await
            {
                Ok(resp) if matches!(resp.status().as_u16(), 200 | 201 | 204) => true,
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    let body = resp.text().await.unwrap_or_default();
                    tracing::warn!(
                        "failed to close Browser Use session {session_id}: HTTP {status} - {}",
                        &body.chars().take(200).collect::<String>()
                    );
                    false
                }
                Err(e) => {
                    tracing::error!("exception closing Browser Use session {session_id}: {e}");
                    false
                }
            }
        })
    }

    fn emergency_cleanup(&self, session_id: &str) -> BoxFuture<'_, ()> {
        let session_id = session_id.to_string();
        Box::pin(async move {
            let Some(config) = Self::config_or_none(false) else {
                tracing::warn!(
                    "cannot emergency-cleanup Browser Use session {session_id} — missing credentials"
                );
                return;
            };
            let Ok(client) = http_client(Duration::from_secs(5)) else {
                return;
            };
            if let Err(e) = client
                .patch(format!("{}/browsers/{session_id}", config.base_url))
                .header("Content-Type", "application/json")
                .header("X-Browser-Use-API-Key", config.api_key)
                .json(&json!({"action": "stop"}))
                .send()
                .await
            {
                tracing::debug!(
                    "emergency cleanup failed for Browser Use session {session_id}: {e}"
                );
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Firecrawl (hermes plugins/browser/firecrawl)
// ---------------------------------------------------------------------------

const FIRECRAWL_BASE_URL: &str = "https://api.firecrawl.dev";

/// Firecrawl (https://firecrawl.dev) cloud browser backend. Cloud-browser
/// path only (`/v2/browser`) — search/extract/crawl live in the web plugin.
pub struct FirecrawlProvider;

impl FirecrawlProvider {
    fn api_url() -> String {
        env_trimmed("FIRECRAWL_API_URL").unwrap_or_else(|| FIRECRAWL_BASE_URL.to_string())
    }

    fn api_key() -> Result<String, String> {
        env_trimmed("FIRECRAWL_API_KEY").ok_or_else(|| {
            "FIRECRAWL_API_KEY environment variable is required. \
             Get your key at https://firecrawl.dev"
                .to_string()
        })
    }
}

impl CloudBrowserProvider for FirecrawlProvider {
    fn name(&self) -> &'static str {
        "firecrawl"
    }

    fn display_name(&self) -> &'static str {
        "Firecrawl"
    }

    fn is_available(&self) -> bool {
        env_trimmed("FIRECRAWL_API_KEY").is_some()
    }

    fn create_session(&self, task_id: &str) -> BoxFuture<'_, Result<CloudSessionInfo, String>> {
        let task_id = task_id.to_string();
        Box::pin(async move {
            let api_key = Self::api_key()?;
            let ttl: i64 = env_trimmed("FIRECRAWL_BROWSER_TTL")
                .and_then(|raw| raw.parse().ok())
                .unwrap_or(300);

            let client = http_client(Duration::from_secs(30))?;
            let resp = client
                .post(format!("{}/v2/browser", Self::api_url()))
                .header("Content-Type", "application/json")
                .header("Authorization", format!("Bearer {api_key}"))
                .json(&json!({"ttl": ttl}))
                .send()
                .await
                .map_err(|e| format!("Firecrawl API connection failed: {e}"))?;

            let status = resp.status().as_u16();
            if !resp.status().is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(format!(
                    "Failed to create Firecrawl browser session: {status} {body}"
                ));
            }

            let data: Value = resp
                .json()
                .await
                .map_err(|e| format!("Failed to parse Firecrawl session response: {e}"))?;
            let id = data
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| "Firecrawl session response missing 'id'".to_string())?
                .to_string();
            let cdp_url = data
                .get("cdpUrl")
                .and_then(Value::as_str)
                .ok_or_else(|| "Firecrawl session response missing 'cdpUrl'".to_string())?
                .to_string();

            let name = session_name(&task_id);
            tracing::info!("created Firecrawl browser session {name}");

            Ok(CloudSessionInfo {
                session_name: name,
                bb_session_id: id,
                cdp_url,
                expires_at: None,
                features: BTreeMap::from([("firecrawl".to_string(), true)]),
                external_call_id: None,
            })
        })
    }

    fn close_session(&self, session_id: &str) -> BoxFuture<'_, bool> {
        let session_id = session_id.to_string();
        Box::pin(async move {
            let Ok(api_key) = Self::api_key() else {
                tracing::warn!("cannot close Firecrawl session {session_id} — missing credentials");
                return false;
            };
            let Ok(client) = http_client(Duration::from_secs(10)) else {
                return false;
            };
            match client
                .delete(format!("{}/v2/browser/{session_id}", Self::api_url()))
                .header("Content-Type", "application/json")
                .header("Authorization", format!("Bearer {api_key}"))
                .send()
                .await
            {
                Ok(resp) if matches!(resp.status().as_u16(), 200 | 201 | 204) => true,
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    let body = resp.text().await.unwrap_or_default();
                    tracing::warn!(
                        "failed to close Firecrawl session {session_id}: HTTP {status} - {}",
                        &body.chars().take(200).collect::<String>()
                    );
                    false
                }
                Err(e) => {
                    tracing::error!("exception closing Firecrawl session {session_id}: {e}");
                    false
                }
            }
        })
    }

    fn emergency_cleanup(&self, session_id: &str) -> BoxFuture<'_, ()> {
        let session_id = session_id.to_string();
        Box::pin(async move {
            if env_trimmed("FIRECRAWL_API_KEY").is_none() {
                tracing::warn!(
                    "cannot emergency-cleanup Firecrawl session {session_id} — missing credentials"
                );
                return;
            }
            let Ok(client) = http_client(Duration::from_secs(5)) else {
                return;
            };
            if let Ok(api_key) = Self::api_key() {
                if let Err(e) = client
                    .delete(format!("{}/v2/browser/{session_id}", Self::api_url()))
                    .header("Content-Type", "application/json")
                    .header("Authorization", format!("Bearer {api_key}"))
                    .send()
                    .await
                {
                    tracing::debug!(
                        "emergency cleanup failed for Firecrawl session {session_id}: {e}"
                    );
                }
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Cloud session lifecycle for the shared browser session layer
// ---------------------------------------------------------------------------

fn session_slot() -> &'static tokio::sync::RwLock<Option<(String, CloudSessionInfo)>> {
    static SLOT: OnceLock<tokio::sync::RwLock<Option<(String, CloudSessionInfo)>>> =
        OnceLock::new();
    SLOT.get_or_init(|| tokio::sync::RwLock::new(None))
}

/// `[browser] cloud_provider` from config.toml, if set.
pub fn configured_provider_name() -> Option<String> {
    crate::config::UlncLawConfig::load(None)
        .ok()
        .and_then(|c| c.browser.cloud_provider.clone())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// CDP endpoint of the active cloud session, creating (or re-creating an
/// expired) session on demand — hermes browser_tool cloud-mode semantics.
/// Returns `Ok(None)` when no provider resolves (local mode).
pub async fn cloud_endpoint() -> Result<Option<String>, String> {
    let configured = configured_provider_name();
    let Some(provider) = resolve_cloud_provider(configured.as_deref()) else {
        return Ok(None);
    };

    let mut slot = session_slot().write().await;
    if let Some((name, info)) = slot.as_ref() {
        if name == provider.name() && !info.cdp_url.is_empty() && !session_has_expired(info) {
            return Ok(Some(info.cdp_url.clone()));
        }
        // Provider changed or the session crossed its provider deadline:
        // retire the old session (best-effort, in the background) so an
        // expired endpoint is not reconnected indefinitely.
        if let Some((old_name, old_info)) = slot.take() {
            if !old_info.bb_session_id.is_empty() {
                if let Some(old_provider) = get_provider(&old_name) {
                    let old_id = old_info.bb_session_id.clone();
                    tokio::spawn(async move {
                        old_provider.close_session(&old_id).await;
                    });
                }
            }
        }
    }

    let info = provider.create_session("main").await?;
    if info.cdp_url.is_empty() {
        return Err(format!(
            "cloud browser provider '{}' returned no CDP URL",
            provider.name()
        ));
    }
    let cdp_url = info.cdp_url.clone();
    *slot = Some((provider.name().to_string(), info));
    Ok(Some(cdp_url))
}

/// Active cloud session metadata (for status displays), if any.
pub async fn active_session_info() -> Option<(String, CloudSessionInfo)> {
    session_slot().read().await.clone()
}

/// Close the active cloud session on shutdown (hermes atexit
/// `_emergency_cleanup_all_sessions` for the cloud half). Best-effort.
pub async fn shutdown_cloud_sessions() {
    let Some((name, info)) = session_slot().write().await.take() else {
        return;
    };
    if info.bb_session_id.is_empty() {
        return;
    }
    if let Some(provider) = get_provider(&name) {
        provider.close_session(&info.bb_session_id).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Env-var guard: holds the process-wide test lock, snapshots the named
    /// vars, clears them for the test, restores on drop.
    struct EnvGuard {
        saved: Vec<(&'static str, Option<String>)>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    const ENV_VARS: &[&str] = &[
        "BROWSERBASE_API_KEY",
        "BROWSERBASE_PROJECT_ID",
        "BROWSERBASE_BASE_URL",
        "BROWSERBASE_PROXIES",
        "BROWSERBASE_ADVANCED_STEALTH",
        "BROWSERBASE_KEEP_ALIVE",
        "BROWSERBASE_SESSION_TIMEOUT",
        "BROWSER_USE_API_KEY",
        "BROWSER_USE_BASE_URL",
        "BROWSER_USE_GATEWAY_URL",
        "FIRECRAWL_API_KEY",
        "FIRECRAWL_API_URL",
        "FIRECRAWL_BROWSER_TTL",
        "ULNCLAW_HOME",
    ];

    impl EnvGuard {
        fn acquire() -> Self {
            let lock = crate::models_dev::test_env_lock();
            let mut saved = Vec::new();
            for name in ENV_VARS.iter().copied() {
                saved.push((name, std::env::var(name).ok()));
                std::env::remove_var(name);
            }
            // Isolate config/auth reads (managed gateway token, .env) in an
            // empty scratch home for the duration of the test.
            let scratch =
                std::env::temp_dir().join(format!("ulnclaw-cloud-{}", std::process::id()));
            std::fs::create_dir_all(&scratch).ok();
            std::env::set_var("ULNCLAW_HOME", &scratch);
            Self { saved, _lock: lock }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (name, value) in self.saved.drain(..) {
                match value {
                    Some(v) => std::env::set_var(name, v),
                    None => std::env::remove_var(name),
                }
            }
        }
    }

    async fn clear_cloud_slot() {
        *session_slot().write().await = None;
    }

    // -- resolution rules ------------------------------------------------

    #[test]
    fn resolve_explicit_local_disables_cloud_mode() {
        let _guard = EnvGuard::acquire();
        std::env::set_var("BROWSER_USE_API_KEY", "bu-key");
        std::env::set_var("BROWSERBASE_API_KEY", "bb-key");
        std::env::set_var("BROWSERBASE_PROJECT_ID", "proj");
        assert!(resolve_cloud_provider(Some("local")).is_none());
        assert!(resolve_cloud_provider(Some(" LOCAL ")).is_none());
    }

    #[test]
    fn resolve_explicit_provider_wins_even_when_unavailable() {
        let _guard = EnvGuard::acquire();
        // No credentials anywhere: explicit selection still resolves so the
        // dispatcher surfaces a precise missing-credentials error.
        let provider = resolve_cloud_provider(Some("browserbase")).expect("explicit provider");
        assert_eq!(provider.name(), "browserbase");
        assert!(!provider.is_available());
    }

    #[test]
    fn resolve_unknown_configured_falls_back_to_autodetect() {
        let _guard = EnvGuard::acquire();
        std::env::set_var("BROWSERBASE_API_KEY", "bb-key");
        std::env::set_var("BROWSERBASE_PROJECT_ID", "proj");
        let provider = resolve_cloud_provider(Some("not-a-provider")).expect("fallback walk");
        assert_eq!(provider.name(), "browserbase");
    }

    #[test]
    fn resolve_legacy_walk_prefers_browser_use_over_browserbase() {
        let _guard = EnvGuard::acquire();
        std::env::set_var("BROWSER_USE_API_KEY", "bu-key");
        std::env::set_var("BROWSERBASE_API_KEY", "bb-key");
        std::env::set_var("BROWSERBASE_PROJECT_ID", "proj");
        let provider = resolve_cloud_provider(None).expect("auto-detect");
        assert_eq!(provider.name(), "browser-use");
    }

    #[test]
    fn resolve_autodetect_skips_firecrawl() {
        let _guard = EnvGuard::acquire();
        // FIRECRAWL_API_KEY alone must NOT route to the paid cloud browser.
        std::env::set_var("FIRECRAWL_API_KEY", "fc-key");
        assert!(resolve_cloud_provider(None).is_none());
        // Explicit opt-in still works.
        let provider = resolve_cloud_provider(Some("firecrawl")).expect("explicit firecrawl");
        assert_eq!(provider.name(), "firecrawl");
        assert!(provider.is_available());
    }

    #[test]
    fn list_providers_includes_builtins() {
        let _guard = EnvGuard::acquire();
        let names: Vec<String> = list_providers()
            .iter()
            .map(|p| p.name().to_string())
            .collect();
        assert!(names.contains(&"browser-use".to_string()));
        assert!(names.contains(&"browserbase".to_string()));
        assert!(names.contains(&"firecrawl".to_string()));
    }

    // -- expiry parsing ---------------------------------------------------

    #[test]
    fn session_expiry_timestamp_parses_hermes_variants() {
        let z = session_expiry_timestamp(Some("2026-08-07T10:00:00Z")).unwrap();
        let offset = session_expiry_timestamp(Some("2026-08-07T12:00:00+02:00")).unwrap();
        assert!((z - offset).abs() < f64::EPSILON);

        // Naive timestamps are treated as UTC.
        let naive = session_expiry_timestamp(Some("2026-08-07T10:00:00")).unwrap();
        assert!((naive - z).abs() < f64::EPSILON);

        // Numeric epoch passthrough.
        let epoch = session_expiry_timestamp(Some("1786125600")).unwrap();
        assert!((epoch - 1_786_125_600.0).abs() < f64::EPSILON);

        assert!(session_expiry_timestamp(None).is_none());
        assert!(session_expiry_timestamp(Some("   ")).is_none());
        assert!(session_expiry_timestamp(Some("not-a-date")).is_none());
    }

    #[test]
    fn session_has_expired_follows_deadline() {
        let mut info = CloudSessionInfo {
            session_name: "s".into(),
            bb_session_id: "id".into(),
            cdp_url: "ws://x".into(),
            expires_at: None,
            features: BTreeMap::new(),
            external_call_id: None,
        };
        assert!(!session_has_expired(&info));
        info.expires_at = Some("2000-01-01T00:00:00Z".into());
        assert!(session_has_expired(&info));
        info.expires_at = Some("2999-01-01T00:00:00Z".into());
        assert!(!session_has_expired(&info));
        info.expires_at = Some("garbage".into());
        assert!(!session_has_expired(&info));
    }

    // -- HTTP lifecycle against a mock server -----------------------------

    async fn spawn_mock(app: axum::Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn browserbase_create_session_402_fallback_chain() {
        let _guard = EnvGuard::acquire();
        clear_cloud_slot().await;

        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_for_route = attempts.clone();
        let app = axum::Router::new().route(
            "/v1/sessions",
            axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
                let attempts = attempts_for_route.clone();
                async move {
                    let attempt = attempts.fetch_add(1, Ordering::SeqCst) + 1;
                    let has_keepalive = body.get("keepAlive").is_some();
                    let has_proxies = body.get("proxies").is_some();
                    if attempt == 1 && has_keepalive {
                        // Paid keepAlive unavailable.
                        (
                            axum::http::StatusCode::PAYMENT_REQUIRED,
                            axum::Json(json!({"error": "keepAlive requires paid plan"})),
                        )
                    } else if attempt == 2 && has_proxies {
                        // Paid proxies unavailable.
                        (
                            axum::http::StatusCode::PAYMENT_REQUIRED,
                            axum::Json(json!({"error": "proxies require paid plan"})),
                        )
                    } else {
                        assert_eq!(
                            body.get("projectId").and_then(Value::as_str),
                            Some("proj-1")
                        );
                        (
                            axum::http::StatusCode::OK,
                            axum::Json(json!({
                                "id": "sess-123",
                                "connectUrl": "wss://cloud.browserbase.dev/sess-123",
                            })),
                        )
                    }
                }
            }),
        );
        let base = spawn_mock(app).await;

        std::env::set_var("BROWSERBASE_API_KEY", "bb-key");
        std::env::set_var("BROWSERBASE_PROJECT_ID", "proj-1");
        std::env::set_var("BROWSERBASE_BASE_URL", &base);

        let provider = get_provider("browserbase").unwrap();
        assert!(provider.is_available());
        let info = provider
            .create_session("task1")
            .await
            .expect("session created");
        assert_eq!(info.bb_session_id, "sess-123");
        assert_eq!(info.cdp_url, "wss://cloud.browserbase.dev/sess-123");
        assert!(info.session_name.starts_with("ulnclaw_task1_"));
        // Both paid features fell back; basic stealth stays on.
        assert_eq!(info.features.get("keep_alive"), Some(&false));
        assert_eq!(info.features.get("proxies"), Some(&false));
        assert_eq!(info.features.get("basic_stealth"), Some(&true));
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn browserbase_close_session_posts_release() {
        let _guard = EnvGuard::acquire();
        let seen = Arc::new(std::sync::Mutex::new(Value::Null));
        let seen_for_route = seen.clone();
        let app = axum::Router::new().route(
            "/v1/sessions/:id",
            axum::routing::post(
                move |axum::extract::Path(id): axum::extract::Path<String>,
                      axum::Json(body): axum::Json<Value>| {
                    let seen = seen_for_route.clone();
                    async move {
                        *seen.lock().unwrap() = json!({"id": id, "body": body});
                        axum::Json(json!({}))
                    }
                },
            ),
        );
        let base = spawn_mock(app).await;

        std::env::set_var("BROWSERBASE_API_KEY", "bb-key");
        std::env::set_var("BROWSERBASE_PROJECT_ID", "proj-1");
        std::env::set_var("BROWSERBASE_BASE_URL", &base);

        let provider = get_provider("browserbase").unwrap();
        assert!(provider.close_session("sess-123").await);
        let recorded = seen.lock().unwrap().clone();
        assert_eq!(recorded["id"], "sess-123");
        assert_eq!(recorded["body"]["status"], "REQUEST_RELEASE");
        assert_eq!(recorded["body"]["projectId"], "proj-1");
    }

    #[tokio::test]
    async fn browser_use_direct_create_session() {
        let _guard = EnvGuard::acquire();
        clear_cloud_slot().await;

        let app = axum::Router::new().route(
            "/browsers",
            axum::routing::post(|axum::Json(body): axum::Json<Value>| async move {
                assert!(body.as_object().unwrap().is_empty());
                axum::Json(json!({
                    "id": "bu-1",
                    "cdpUrl": "wss://proxy.browser-use.com/bu-1",
                    "timeoutAt": "2999-01-01T00:00:00.000Z",
                }))
            }),
        );
        let base = spawn_mock(app).await;

        std::env::set_var("BROWSER_USE_API_KEY", "bu-key");
        std::env::set_var("BROWSER_USE_BASE_URL", &base);

        let provider = get_provider("browser-use").unwrap();
        assert!(provider.is_available());
        let info = provider
            .create_session("task2")
            .await
            .expect("session created");
        assert_eq!(info.bb_session_id, "bu-1");
        assert_eq!(info.cdp_url, "wss://proxy.browser-use.com/bu-1");
        assert_eq!(info.expires_at.as_deref(), Some("2999-01-01T00:00:00.000Z"));
        assert!(!session_has_expired(&info));
        assert_eq!(info.features.get("browser_use"), Some(&true));
        assert!(info.external_call_id.is_none());
    }

    #[tokio::test]
    async fn firecrawl_create_and_close_session() {
        let _guard = EnvGuard::acquire();
        clear_cloud_slot().await;

        let closed = Arc::new(AtomicUsize::new(0));
        let closed_for_route = closed.clone();
        let app = axum::Router::new()
            .route(
                "/v2/browser",
                axum::routing::post(|axum::Json(body): axum::Json<Value>| async move {
                    assert_eq!(body.get("ttl").and_then(Value::as_i64), Some(600));
                    axum::Json(json!({
                        "id": "fc-1",
                        "cdpUrl": "wss://browser.firecrawl.dev/fc-1",
                    }))
                }),
            )
            .route(
                "/v2/browser/:id",
                axum::routing::delete(
                    move |axum::extract::Path(id): axum::extract::Path<String>| {
                        let closed = closed_for_route.clone();
                        async move {
                            assert_eq!(id, "fc-1");
                            closed.fetch_add(1, Ordering::SeqCst);
                            axum::Json(json!({}))
                        }
                    },
                ),
            );
        let base = spawn_mock(app).await;

        std::env::set_var("FIRECRAWL_API_KEY", "fc-key");
        std::env::set_var("FIRECRAWL_API_URL", &base);
        std::env::set_var("FIRECRAWL_BROWSER_TTL", "600");

        let provider = get_provider("firecrawl").unwrap();
        let info = provider
            .create_session("task3")
            .await
            .expect("session created");
        assert_eq!(info.bb_session_id, "fc-1");
        assert_eq!(info.cdp_url, "wss://browser.firecrawl.dev/fc-1");
        assert!(provider.close_session("fc-1").await);
        assert_eq!(closed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cloud_endpoint_caches_and_recreates_after_expiry() {
        let _guard = EnvGuard::acquire();
        clear_cloud_slot().await;

        let creates = Arc::new(AtomicUsize::new(0));
        let creates_for_route = creates.clone();
        let closes = Arc::new(AtomicUsize::new(0));
        let closes_for_route = closes.clone();
        let app = axum::Router::new()
            .route(
                "/v1/sessions",
                axum::routing::post(move || {
                    let creates = creates_for_route.clone();
                    async move {
                        let n = creates.fetch_add(1, Ordering::SeqCst) + 1;
                        axum::Json(json!({
                            "id": format!("sess-{n}"),
                            "connectUrl": format!("wss://cloud.browserbase.dev/sess-{n}"),
                        }))
                    }
                }),
            )
            .route(
                "/v1/sessions/:id",
                axum::routing::post(
                    move |axum::extract::Path(_id): axum::extract::Path<String>| {
                        let closes = closes_for_route.clone();
                        async move {
                            closes.fetch_add(1, Ordering::SeqCst);
                            axum::Json(json!({}))
                        }
                    },
                ),
            );
        let base = spawn_mock(app).await;

        std::env::set_var("BROWSERBASE_API_KEY", "bb-key");
        std::env::set_var("BROWSERBASE_PROJECT_ID", "proj-1");
        std::env::set_var("BROWSERBASE_BASE_URL", &base);

        // Auto-detect walk resolves browserbase; first call creates.
        let first = cloud_endpoint().await.expect("endpoint").expect("some");
        assert_eq!(first, "wss://cloud.browserbase.dev/sess-1");
        assert_eq!(creates.load(Ordering::SeqCst), 1);

        // Cached on the second call.
        let second = cloud_endpoint().await.expect("endpoint").expect("some");
        assert_eq!(second, first);
        assert_eq!(creates.load(Ordering::SeqCst), 1);

        // Force expiry: the expired session is retired (closed in the
        // background) and a fresh one is created.
        session_slot().write().await.as_mut().unwrap().1.expires_at =
            Some("2000-01-01T00:00:00Z".to_string());
        let third = cloud_endpoint().await.expect("endpoint").expect("some");
        assert_eq!(third, "wss://cloud.browserbase.dev/sess-2");
        assert_eq!(creates.load(Ordering::SeqCst), 2);

        // Shutdown closes the active session (hermes atexit semantics).
        shutdown_cloud_sessions().await;
        assert!(session_slot().read().await.is_none());
        // Background close of sess-1 + shutdown close of sess-2.
        for _ in 0..50 {
            if closes.load(Ordering::SeqCst) >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(closes.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn cloud_endpoint_none_without_credentials() {
        let _guard = EnvGuard::acquire();
        clear_cloud_slot().await;
        // No credentials and no explicit provider: local mode.
        assert!(cloud_endpoint().await.expect("no error").is_none());
    }
}

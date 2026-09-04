//! Dashboard-mediated OAuth callback bridge (hermes
//! `tools/mcp_dashboard_oauth.py` + `web_server.py` flow registry).
//!
//! The OAuth machinery in [`crate::mcp::oauth`] keeps doing discovery,
//! dynamic client registration, PKCE, state validation and token
//! exchange. This module only moves the two human/browser callbacks —
//! "show the authorization URL" and "receive the redirect" — out of a
//! loopback listener and into the already-authenticated gateway
//! (dashboard) session:
//!
//! * `POST /api/mcp/servers/:name/auth` starts a [`DashboardOAuthFlow`]
//!   worker and returns once the authorization URL is known;
//! * the dashboard renders the URL for the operator to open;
//! * the provider redirects to `GET /api/mcp/oauth/callback/:server`
//!   (exempt from the bearer gate — the `state` parameter is the
//!   protection), which delivers the code to the waiting flow.
//!
//! The active flow is exposed to the OAuth code path through a
//! `tokio::task_local` (hermes uses a `contextvars.ContextVar`), so
//! [`crate::mcp::oauth::get_access_token`] publishes the URL / waits for
//! the callback through the flow instead of binding a loopback port.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::error::AgentError;

/// Maximum concurrently pending dashboard OAuth flows (hermes
/// `_MAX_PENDING_MCP_OAUTH_FLOWS`).
pub const MAX_PENDING_FLOWS: usize = 8;

/// Flows older than this are garbage-collected (hermes
/// `_MCP_DASHBOARD_OAUTH_TTL` = 15 minutes).
pub const FLOW_TTL_SECS: u64 = 15 * 60;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Constant-time string comparison for OAuth `state` validation (hermes
/// `secrets.compare_digest`).
pub fn constant_time_eq(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FlowStatus {
    #[default]
    Starting,
    AuthorizationRequired,
    Approved,
    Error,
}

impl FlowStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            FlowStatus::Starting => "starting",
            FlowStatus::AuthorizationRequired => "authorization_required",
            FlowStatus::Approved => "approved",
            FlowStatus::Error => "error",
        }
    }
}

#[derive(Debug, Default)]
struct FlowInner {
    status: FlowStatus,
    authorization_url: Option<String>,
    error: Option<String>,
    expected_state: Option<String>,
    callback: Option<(String, Option<String>)>,
    callback_error: Option<String>,
    tools: Vec<Value>,
    auth_ready: bool,
    callback_ready: bool,
}

/// One dashboard-mediated OAuth authorization attempt.
///
/// Mirrors hermes `DashboardOAuthFlow` field-for-field (minus the
/// threading primitives, which become a mutex + `tokio::sync::Notify`).
pub struct DashboardOAuthFlow {
    pub flow_id: String,
    pub server_name: String,
    pub profile: Option<String>,
    pub home: String,
    pub redirect_uri: String,
    pub created_at: u64,
    inner: Mutex<FlowInner>,
    auth_ready: tokio::sync::Notify,
    callback_ready: tokio::sync::Notify,
    worker_done: std::sync::atomic::AtomicBool,
}

impl DashboardOAuthFlow {
    pub fn new(
        flow_id: String,
        server_name: String,
        profile: Option<String>,
        home: String,
        redirect_uri: String,
    ) -> Arc<Self> {
        Arc::new(Self {
            flow_id,
            server_name,
            profile,
            home,
            redirect_uri,
            created_at: now_secs(),
            inner: Mutex::new(FlowInner {
                status: FlowStatus::Starting,
                ..Default::default()
            }),
            auth_ready: tokio::sync::Notify::new(),
            callback_ready: tokio::sync::Notify::new(),
            worker_done: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Publish the authorization URL (hermes `publish_authorization_url`).
    /// Extracts `state` from the URL query; the flow moves to
    /// `authorization_required` and waiters are released.
    pub fn publish_authorization_url(&self, url: &str) -> Result<(), AgentError> {
        let state = extract_query_param(url, "state").ok_or_else(|| {
            AgentError::Tool("OAuth authorization URL did not include state".to_string())
        })?;
        let mut inner = self.inner.lock().unwrap();
        if matches!(inner.status, FlowStatus::Approved | FlowStatus::Error) {
            return Err(AgentError::Tool("OAuth flow already ended".to_string()));
        }
        inner.expected_state = Some(state);
        inner.authorization_url = Some(url.to_string());
        inner.status = FlowStatus::AuthorizationRequired;
        inner.auth_ready = true;
        drop(inner);
        self.auth_ready.notify_waiters();
        Ok(())
    }

    /// Wait until the authorization URL is published (hermes
    /// `wait_for_authorization_url`, default timeout 30 s).
    pub async fn wait_for_authorization_url(
        &self,
        timeout: Duration,
    ) -> Result<String, AgentError> {
        let deadline = Instant::now() + timeout;
        loop {
            let notified = self.auth_ready.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let inner = self.inner.lock().unwrap();
                if inner.auth_ready {
                    if let Some(url) = inner.authorization_url.clone() {
                        return Ok(url);
                    }
                    let msg = inner
                        .error
                        .clone()
                        .unwrap_or_else(|| "MCP OAuth flow ended before authorization".into());
                    return Err(AgentError::Tool(msg));
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if tokio::time::timeout(remaining, notified).await.is_err() {
                return Err(AgentError::Tool(
                    "Timed out waiting for MCP authorization URL".to_string(),
                ));
            }
        }
    }

    /// Deliver the browser redirect (hermes `deliver_callback`): validates
    /// the `state` parameter in constant time, stores either the code or
    /// the provider error, and releases the worker.
    pub fn deliver_callback(
        &self,
        code: Option<&str>,
        state: Option<&str>,
        error: Option<&str>,
    ) -> Result<(), AgentError> {
        let mut inner = self.inner.lock().unwrap();
        if inner.callback_ready {
            return Err(AgentError::Tool(
                "OAuth callback already received".to_string(),
            ));
        }
        let state_ok = match (&inner.expected_state, state) {
            (Some(expected), Some(actual)) => constant_time_eq(expected, actual),
            _ => false,
        };
        if !state_ok {
            return Err(AgentError::Tool(
                "OAuth callback state mismatch".to_string(),
            ));
        }
        if let Some(error) = error {
            inner.callback_error = Some(error.to_string());
        } else if let Some(code) = code {
            inner.callback = Some((code.to_string(), state.map(str::to_string)));
        } else {
            inner.callback_error = Some("OAuth callback did not include code or error".to_string());
        }
        inner.callback_ready = true;
        drop(inner);
        self.callback_ready.notify_waiters();
        Ok(())
    }

    /// Wait for the delivered callback (hermes `wait_for_callback`,
    /// default timeout 300 s). Returns `(code, state)`.
    pub async fn wait_for_callback(
        &self,
        timeout: Duration,
    ) -> Result<(String, Option<String>), AgentError> {
        let deadline = Instant::now() + timeout;
        loop {
            let notified = self.callback_ready.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let inner = self.inner.lock().unwrap();
                if inner.callback_ready {
                    if let Some(error) = inner.callback_error.clone() {
                        return Err(AgentError::Tool(format!(
                            "OAuth authorization failed: {}",
                            error
                        )));
                    }
                    if let Some(callback) = inner.callback.clone() {
                        return Ok(callback);
                    }
                    return Err(AgentError::Tool(
                        "OAuth callback did not include an authorization code".to_string(),
                    ));
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if tokio::time::timeout(remaining, notified).await.is_err() {
                return Err(AgentError::Tool(
                    "Timed out waiting for MCP OAuth callback".to_string(),
                ));
            }
        }
    }

    /// Mark the flow approved after token exchange (hermes
    /// `mark_approved`).
    pub fn mark_approved(&self) -> Result<(), AgentError> {
        let mut inner = self.inner.lock().unwrap();
        if inner.status == FlowStatus::Error {
            return Err(AgentError::Tool("OAuth flow already ended".to_string()));
        }
        inner.status = FlowStatus::Approved;
        inner.error = None;
        Ok(())
    }

    /// Mark the flow failed (hermes `mark_error`): releases BOTH waiters
    /// so nobody blocks on a dead flow. A no-op once approved.
    pub fn mark_error(&self, error: &str) {
        let mut inner = self.inner.lock().unwrap();
        if inner.status == FlowStatus::Approved {
            return;
        }
        inner.status = FlowStatus::Error;
        inner.error = Some(error.to_string());
        inner.auth_ready = true;
        inner.callback_ready = true;
        drop(inner);
        self.auth_ready.notify_waiters();
        self.callback_ready.notify_waiters();
    }

    /// Record the tool list discovered by the post-auth probe.
    pub fn set_tools(&self, tools: Vec<Value>) {
        self.inner.lock().unwrap().tools = tools;
    }

    /// API snapshot (hermes `snapshot`).
    pub fn snapshot(&self) -> Value {
        let inner = self.inner.lock().unwrap();
        json!({
            "flow_id": self.flow_id,
            "server_name": self.server_name,
            "status": inner.status.as_str(),
            "authorization_url": inner.authorization_url,
            "error": inner.error,
        })
    }

    /// Snapshot plus the discovered tools (status endpoint).
    pub fn snapshot_with_tools(&self) -> Value {
        let inner = self.inner.lock().unwrap();
        json!({
            "flow_id": self.flow_id,
            "server_name": self.server_name,
            "status": inner.status.as_str(),
            "authorization_url": inner.authorization_url,
            "error": inner.error,
            "tools": inner.tools,
        })
    }

    pub fn status(&self) -> FlowStatus {
        self.inner.lock().unwrap().status
    }

    pub fn expected_state(&self) -> Option<String> {
        self.inner.lock().unwrap().expected_state.clone()
    }

    pub fn mark_worker_done(&self) {
        self.worker_done
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn worker_done(&self) -> bool {
        self.worker_done.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Extract one query parameter from a URL without pulling in a parser
/// dependency: splits on `?`, then on `&`/`=`. Percent-decoding of the
/// value covers `%XX` escapes (state values are urlsafe random strings,
/// but providers may encode them).
fn extract_query_param(url: &str, name: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    for pair in query.split('#').next().unwrap_or("").split('&') {
        let (key, value) = pair.split_once('=')?;
        if key == name {
            return Some(percent_decode(value));
        }
    }
    None
}

/// Percent-decode one URL component (`%XX` escapes, `+` as space).
pub fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&value[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------------------------------------------------------------------------
// Task-local: the active dashboard flow (hermes `_current_dashboard_flow`
// ContextVar)
// ---------------------------------------------------------------------------

tokio::task_local! {
    static CURRENT_FLOW: Arc<DashboardOAuthFlow>;
}

/// Run `future` with `flow` installed as the active dashboard OAuth flow.
/// Inside the scope, [`current_flow`] returns it and
/// [`crate::mcp::oauth::get_access_token`] routes its authorization URL /
/// callback through the flow instead of a loopback listener.
pub fn scope_flow<F: std::future::Future>(
    flow: Arc<DashboardOAuthFlow>,
    future: F,
) -> impl std::future::Future<Output = F::Output> {
    CURRENT_FLOW.scope(flow, future)
}

/// The active dashboard flow on this task, if any.
pub fn current_flow() -> Option<Arc<DashboardOAuthFlow>> {
    CURRENT_FLOW.try_with(|flow| flow.clone()).ok()
}

// ---------------------------------------------------------------------------
// Flow registry (hermes `web_server._mcp_oauth_flows` + lock + pending cap
// + TTL GC)
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
pub enum RegistryError {
    /// 429 — too many pending flows.
    TooManyPending,
    /// 409 — this server already has a flow in progress.
    AlreadyInProgress,
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegistryError::TooManyPending => {
                write!(f, "Too many MCP OAuth flows are already in progress")
            }
            RegistryError::AlreadyInProgress => {
                write!(f, "already in progress")
            }
        }
    }
}

pub struct FlowRegistry {
    flows: Mutex<HashMap<String, Arc<DashboardOAuthFlow>>>,
}

impl Default for FlowRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl FlowRegistry {
    pub fn new() -> Self {
        Self {
            flows: Mutex::new(HashMap::new()),
        }
    }

    /// Drop flows older than [`FLOW_TTL_SECS`] (hermes
    /// `_gc_mcp_oauth_flows`).
    pub fn gc(&self) {
        let cutoff = now_secs().saturating_sub(FLOW_TTL_SECS);
        let mut flows = self.flows.lock().unwrap();
        flows.retain(|_, flow| flow.created_at >= cutoff);
    }

    /// Insert a new flow with hermes' guard rails: a hard cap on pending
    /// (worker still running) flows and one flow per (server, home).
    pub fn insert(&self, flow: Arc<DashboardOAuthFlow>) -> Result<(), RegistryError> {
        let mut flows = self.flows.lock().unwrap();
        let pending = flows.values().filter(|f| !f.worker_done()).count();
        if pending >= MAX_PENDING_FLOWS {
            return Err(RegistryError::TooManyPending);
        }
        let duplicate = flows
            .values()
            .any(|f| f.server_name == flow.server_name && f.home == flow.home && !f.worker_done());
        if duplicate {
            return Err(RegistryError::AlreadyInProgress);
        }
        flows.insert(flow.flow_id.clone(), flow);
        Ok(())
    }

    pub fn get(&self, flow_id: &str) -> Option<Arc<DashboardOAuthFlow>> {
        self.flows.lock().unwrap().get(flow_id).cloned()
    }

    /// Find the flow a browser callback belongs to (hermes callback route):
    /// same server, awaiting authorization, and a constant-time `state`
    /// match.
    pub fn find_for_callback(
        &self,
        server_name: &str,
        state: Option<&str>,
    ) -> Option<Arc<DashboardOAuthFlow>> {
        let flows = self.flows.lock().unwrap();
        flows
            .values()
            .filter(|f| {
                f.server_name == server_name && f.status() == FlowStatus::AuthorizationRequired
            })
            .find(|f| {
                let expected = f.expected_state();
                match (expected, state) {
                    (Some(expected), Some(actual)) => constant_time_eq(&expected, actual),
                    _ => false,
                }
            })
            .cloned()
    }

    pub fn len(&self) -> usize {
        self.flows.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Process-wide registry used by the gateway routes (hermes module-level
/// `_mcp_oauth_flows`).
pub fn registry() -> &'static FlowRegistry {
    static REGISTRY: OnceLock<FlowRegistry> = OnceLock::new();
    REGISTRY.get_or_init(FlowRegistry::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flow(name: &str) -> Arc<DashboardOAuthFlow> {
        DashboardOAuthFlow::new(
            format!("flow-{}", name),
            name.to_string(),
            None,
            "/home/test".to_string(),
            format!("http://127.0.0.1:8642/api/mcp/oauth/callback/{}", name),
        )
    }

    #[test]
    fn extract_query_param_finds_state() {
        let url = "https://as.example/authorize?client_id=x&state=abc-123&scope=read";
        assert_eq!(extract_query_param(url, "state"), Some("abc-123".into()));
        assert_eq!(extract_query_param(url, "client_id"), Some("x".into()));
        assert_eq!(extract_query_param(url, "missing"), None);
        assert_eq!(
            extract_query_param("https://as.example/authorize", "state"),
            None
        );
        // Percent-encoded value.
        let url = "https://as.example/authorize?state=ab%20cd";
        assert_eq!(extract_query_param(url, "state"), Some("ab cd".into()));
    }

    #[test]
    fn publish_requires_state_and_rejects_after_end() {
        let flow = flow("srv-a");
        let err = flow
            .publish_authorization_url("https://as.example/authorize?client_id=x")
            .unwrap_err();
        assert!(err.to_string().contains("did not include state"), "{}", err);

        flow.publish_authorization_url("https://as.example/authorize?state=s1")
            .unwrap();
        assert_eq!(flow.status(), FlowStatus::AuthorizationRequired);
        assert_eq!(flow.expected_state(), Some("s1".into()));

        flow.mark_approved().unwrap();
        let err = flow
            .publish_authorization_url("https://as.example/authorize?state=s2")
            .unwrap_err();
        assert!(err.to_string().contains("already ended"), "{}", err);
    }

    #[tokio::test]
    async fn deliver_validates_state_and_rejects_replay() {
        let flow = flow("srv-b");
        flow.publish_authorization_url("https://as.example/authorize?state=good-state")
            .unwrap();

        let err = flow
            .deliver_callback(Some("code"), Some("bad-state"), None)
            .unwrap_err();
        assert!(err.to_string().contains("state mismatch"), "{}", err);

        flow.deliver_callback(Some("code-1"), Some("good-state"), None)
            .unwrap();
        let (code, state) = flow
            .wait_for_callback(Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(code, "code-1");
        assert_eq!(state, Some("good-state".into()));

        let err = flow
            .deliver_callback(Some("code-2"), Some("good-state"), None)
            .unwrap_err();
        assert!(err.to_string().contains("already received"), "{}", err);
    }

    #[tokio::test]
    async fn deliver_error_propagates_to_waiter() {
        let flow = flow("srv-c");
        flow.publish_authorization_url("https://as.example/authorize?state=s")
            .unwrap();
        flow.deliver_callback(None, Some("s"), Some("access_denied"))
            .unwrap();
        let err = flow
            .wait_for_callback(Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("access_denied"), "{}", err);
    }

    #[tokio::test]
    async fn mark_error_releases_both_waiters() {
        let flow = flow("srv-d");
        let waiter_flow = flow.clone();
        let waiter = tokio::spawn(async move {
            (
                waiter_flow
                    .wait_for_authorization_url(Duration::from_secs(5))
                    .await,
                waiter_flow.wait_for_callback(Duration::from_secs(5)).await,
            )
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        flow.mark_error("registration exploded");
        let (auth, callback) = waiter.await.unwrap();
        assert!(auth
            .unwrap_err()
            .to_string()
            .contains("registration exploded"));
        assert!(callback.is_err());
        assert_eq!(flow.status(), FlowStatus::Error);
    }

    #[tokio::test]
    async fn wait_times_out_when_nothing_published() {
        let flow = flow("srv-e");
        let err = flow
            .wait_for_authorization_url(Duration::from_millis(50))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Timed out"), "{}", err);
        let err = flow
            .wait_for_callback(Duration::from_millis(50))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Timed out"), "{}", err);
    }

    #[tokio::test]
    async fn scope_flow_makes_current_flow_visible() {
        let flow = flow("srv-f");
        assert!(current_flow().is_none());
        let inside = flow.clone();
        let seen = scope_flow(flow.clone(), async move {
            current_flow().map(|f| f.flow_id.clone())
        })
        .await;
        assert_eq!(seen, Some("flow-srv-f".into()));
        assert!(current_flow().is_none());
        drop(inside);
    }

    #[test]
    fn registry_enforces_pending_cap_and_server_dedup() {
        let reg = FlowRegistry::new();
        // Below the cap: same server while its flow is pending →
        // 409-equivalent (dedup).
        reg.insert(flow("srv-0")).unwrap();
        let dup = DashboardOAuthFlow::new(
            "dup".into(),
            "srv-0".into(),
            None,
            "/home/test".into(),
            "http://x/cb".into(),
        );
        assert_eq!(reg.insert(dup), Err(RegistryError::AlreadyInProgress));

        // Fill to the pending cap (8 worker-running flows).
        for i in 1..MAX_PENDING_FLOWS {
            let flow = DashboardOAuthFlow::new(
                format!("f{}", i),
                format!("srv-{}", i),
                None,
                "/home/test".into(),
                "http://x/cb".into(),
            );
            reg.insert(flow).unwrap();
        }
        // Cap reached → 429-equivalent (cap precedes dedup, hermes
        // order).
        let overflow = DashboardOAuthFlow::new(
            "overflow".into(),
            "srv-overflow".into(),
            None,
            "/home/test".into(),
            "http://x/cb".into(),
        );
        assert_eq!(reg.insert(overflow), Err(RegistryError::TooManyPending));

        // A finished flow frees its slot AND its server (hermes dedup
        // only counts worker-running flows).
        reg.get("flow-srv-0").unwrap().mark_worker_done();
        let again = DashboardOAuthFlow::new(
            "again".into(),
            "srv-0".into(),
            None,
            "/home/test".into(),
            "http://x/cb".into(),
        );
        reg.insert(again).unwrap();
    }

    #[test]
    fn registry_find_for_callback_matches_server_and_state() {
        let reg = FlowRegistry::new();
        let flow = flow("srv-g");
        reg.insert(flow.clone()).unwrap();
        // Not yet awaiting authorization.
        assert!(reg.find_for_callback("srv-g", Some("s")).is_none());
        flow.publish_authorization_url("https://as.example/authorize?state=s1")
            .unwrap();
        assert!(reg.find_for_callback("srv-g", Some("wrong")).is_none());
        assert!(reg.find_for_callback("srv-g", None).is_none());
        let found = reg.find_for_callback("srv-g", Some("s1")).unwrap();
        assert_eq!(found.flow_id, "flow-srv-g");
    }
}

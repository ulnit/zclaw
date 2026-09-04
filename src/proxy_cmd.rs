//! Local OpenAI-compatible proxy to OAuth-authenticated upstreams — port
//! of hermes `hermes_cli/proxy/` (server.py + adapters). External apps
//! (Open WebUI and friends) can ride the user's already-logged-in OAuth
//! subscription instead of a static API key: the proxy listens on
//! `127.0.0.1:<port>/v1/...`, discards whatever bearer the client sends,
//! and attaches a freshly-resolved upstream credential (refreshed near
//! expiry). Responses stream back unmodified (SSE preserved).
//!
//! Divergence from hermes: the adapter is provider-agnostic (`oauth`,
//! driven by `[oauth]` config + `oauth_tokens.json`) instead of the
//! Nous-Portal subscription resolver and the xAI credential-pool
//! adapter (which need hermes' `auth add` credential-pool surface).

use crate::error::AgentError;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

/// hermes `DEFAULT_HOST` / `DEFAULT_PORT`.
pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 8645;
/// hermes `MAX_REQUEST_BYTES` mirror (10 MB).
pub const DEFAULT_MAX_REQUEST_BYTES: usize = 10 * 1024 * 1024;
/// Refresh the access token this many seconds before expiry.
const REFRESH_SKEW_SECS: u64 = 60;
/// Upstream connect timeout (reads stream without a total deadline).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Headers dropped before forwarding (hermes `_HOP_BY_HOP_HEADERS`).
const HOP_BY_HOP_HEADERS: &[&str] = &[
    "host",
    "content-length",
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "transfer-encoding",
    "upgrade",
    "authorization", // replaced with the resolved bearer
    // Keep the byte stream identity-preserving: the client's
    // accept-encoding is not forwarded, so upstream won't compress.
    "accept-encoding",
];

/// `[proxy]` config block.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProxyConfig {
    pub host: String,
    pub port: u16,
    /// Upstream OpenAI-compatible base URL the bearer is attached to
    /// (e.g. `https://inference-api.nousresearch.com/v1`). Required.
    pub upstream_url: String,
    /// Relative paths under `/v1` the upstream accepts.
    pub allowed_paths: Vec<String>,
    /// Request body cap in bytes.
    pub max_request_bytes: usize,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            host: DEFAULT_HOST.into(),
            port: DEFAULT_PORT,
            upstream_url: String::new(),
            allowed_paths: default_allowed_paths(),
            max_request_bytes: DEFAULT_MAX_REQUEST_BYTES,
        }
    }
}

/// hermes adapter `allowed_paths` defaults (xAI set ∪ /responses).
pub fn default_allowed_paths() -> Vec<String> {
    vec![
        "/chat/completions".into(),
        "/completions".into(),
        "/embeddings".into(),
        "/models".into(),
        "/responses".into(),
    ]
}

/// A resolved bearer + base URL ready to forward to (hermes
/// `UpstreamCredential`).
#[derive(Debug, Clone)]
pub struct UpstreamCredential {
    /// Token only — no `Bearer` prefix.
    pub bearer: String,
    pub base_url: String,
    /// Human-readable expiry for `proxy status` (empty = unknown).
    pub expires_at: String,
}

/// Cheap auth check (hermes `is_authenticated`): stored access token.
pub fn is_authenticated(home: &std::path::Path) -> bool {
    crate::oauth::load_tokens(home).logged_in()
}

/// Resolve a credential, refreshing when near expiry (hermes
/// `get_credential`). Persists refreshed tokens.
pub async fn get_credential(
    oauth_cfg: &crate::oauth::OAuthConfig,
    proxy_cfg: &ProxyConfig,
    home: &std::path::Path,
) -> Result<UpstreamCredential, AgentError> {
    let tokens = crate::oauth::load_tokens(home);
    if !tokens.logged_in() {
        return Err(AgentError::config(
            "not logged in — run `ulnclaw auth login` first",
        ));
    }
    let near_expiry = tokens.expires_at > 0
        && std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() + REFRESH_SKEW_SECS >= tokens.expires_at)
            .unwrap_or(false);
    let tokens = if near_expiry && !tokens.refresh_token.is_empty() {
        match crate::oauth::refresh(oauth_cfg, home).await {
            Ok(refreshed) => refreshed,
            Err(e) => {
                // Stale-but-present token: let the upstream decide.
                tracing::warn!("proxy: token refresh failed ({e}); using stored token");
                tokens
            }
        }
    } else {
        tokens
    };
    Ok(credential_from_tokens(&tokens, proxy_cfg))
}

/// Force-refresh path for the one-shot 401/429 retry (hermes
/// `get_retry_credential`).
pub async fn force_refresh_credential(
    oauth_cfg: &crate::oauth::OAuthConfig,
    proxy_cfg: &ProxyConfig,
    home: &std::path::Path,
) -> Result<UpstreamCredential, AgentError> {
    let tokens = crate::oauth::refresh(oauth_cfg, home).await?;
    Ok(credential_from_tokens(&tokens, proxy_cfg))
}

fn credential_from_tokens(
    tokens: &crate::oauth::StoredTokens,
    proxy_cfg: &ProxyConfig,
) -> UpstreamCredential {
    let expires_at = if tokens.expires_at > 0 {
        chrono::DateTime::from_timestamp(tokens.expires_at as i64, 0)
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_default()
    } else {
        String::new()
    };
    UpstreamCredential {
        bearer: tokens.access_token.clone(),
        base_url: proxy_cfg
            .upstream_url
            .trim()
            .trim_end_matches('/')
            .to_string(),
        expires_at,
    }
}

/// True when `path` (with leading slash) is on the allowlist.
pub fn path_allowed(allowed_paths: &[String], path: &str) -> bool {
    allowed_paths.iter().any(|p| p == path)
}

/// Filter request headers for forwarding (hermes hop-by-hop strip).
pub fn filter_request_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    headers
        .iter()
        .filter(|(name, _)| {
            !HOP_BY_HOP_HEADERS
                .iter()
                .any(|h| h.eq_ignore_ascii_case(name))
        })
        .cloned()
        .collect()
}

/// Filter upstream response headers before streaming back to the
/// client: drop hop-by-hop plus framing headers whose values no longer
/// match the re-chunked body.
pub fn filter_response_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    const DROP: &[&str] = &[
        "connection",
        "keep-alive",
        "transfer-encoding",
        "upgrade",
        "content-length",
        "content-encoding",
    ];
    headers
        .iter()
        .filter(|(name, _)| !DROP.iter().any(|h| h.eq_ignore_ascii_case(name)))
        .cloned()
        .collect()
}

struct ProxyState {
    oauth_cfg: crate::oauth::OAuthConfig,
    proxy_cfg: ProxyConfig,
    home: std::path::PathBuf,
    client: reqwest::Client,
}

/// Run the proxy until the listener fails (Ctrl+C is handled by the
/// CLI caller via process exit). hermes `run_server`.
pub async fn run_server(
    oauth_cfg: crate::oauth::OAuthConfig,
    proxy_cfg: ProxyConfig,
    home: std::path::PathBuf,
) -> Result<(), AgentError> {
    use axum::{
        body::Body,
        extract::State,
        http::StatusCode,
        response::{IntoResponse, Response},
        routing::get,
        Router,
    };

    let client = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| AgentError::config(format!("proxy client: {e}")))?;
    let state = Arc::new(ProxyState {
        oauth_cfg,
        proxy_cfg: proxy_cfg.clone(),
        home,
        client,
    });

    async fn health() -> impl IntoResponse {
        axum::Json(json!({"status": "ok"}))
    }

    async fn proxy_forward(
        State(state): State<Arc<ProxyState>>,
        req: axum::extract::Request,
    ) -> Response {
        use futures::TryStreamExt;

        let path = req.uri().path().to_string();
        let tail = path
            .strip_prefix("/v1")
            .unwrap_or("")
            .trim_start_matches('/')
            .to_string();
        let rel_path = format!("/{tail}");
        if !path_allowed(&state.proxy_cfg.allowed_paths, &rel_path) {
            return (
                StatusCode::NOT_FOUND,
                axum::Json(json!({
                    "error": format!(
                        "path {rel_path} is not served by this proxy (allowed: {})",
                        state.proxy_cfg.allowed_paths.join(", ")
                    )
                })),
            )
                .into_response();
        }

        let (parts, body) = req.into_parts();
        let body_bytes = match axum::body::to_bytes(body, state.proxy_cfg.max_request_bytes).await {
            Ok(b) => b,
            Err(_) => {
                return (
                    StatusCode::PAYLOAD_TOO_LARGE,
                    axum::Json(json!({"error": "request body too large"})),
                )
                    .into_response()
            }
        };

        let cred = match get_credential(&state.oauth_cfg, &state.proxy_cfg, &state.home).await {
            Ok(c) => c,
            Err(e) => {
                return (
                    StatusCode::UNAUTHORIZED,
                    axum::Json(json!({"error": e.to_string()})),
                )
                    .into_response()
            }
        };

        let fwd_headers: Vec<(String, String)> = filter_request_headers(
            &parts
                .headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                .collect::<Vec<_>>(),
        );

        let send_upstream = |cred: &UpstreamCredential| {
            let url = format!("{}/{}", cred.base_url, tail);
            let mut upstream = state.client.request(parts.method.clone(), url);
            for (name, value) in &fwd_headers {
                upstream = upstream.header(name.as_str(), value.as_str());
            }
            upstream = upstream.header("authorization", format!("Bearer {}", cred.bearer));
            if !body_bytes.is_empty() {
                upstream = upstream.body(body_bytes.to_vec());
            }
            upstream.send()
        };

        let response = match send_upstream(&cred).await {
            Ok(resp) => resp,
            Err(e) => {
                let status = if e.is_timeout() {
                    StatusCode::GATEWAY_TIMEOUT
                } else {
                    StatusCode::BAD_GATEWAY
                };
                return (
                    status,
                    axum::Json(json!({"error": format!("upstream request failed: {e}")})),
                )
                    .into_response();
            }
        };

        // hermes one-shot retry on 401/429 with a refreshed credential.
        let response = if matches!(response.status().as_u16(), 401 | 429) {
            match force_refresh_credential(&state.oauth_cfg, &state.proxy_cfg, &state.home).await {
                Ok(retry_cred) if retry_cred.bearer != cred.bearer => {
                    match send_upstream(&retry_cred).await {
                        Ok(retried) => retried,
                        Err(_) => response,
                    }
                }
                _ => response,
            }
        } else {
            response
        };

        let status =
            StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        let resp_headers: Vec<(String, String)> = filter_response_headers(
            &response
                .headers()
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                .collect::<Vec<_>>(),
        );
        let stream = response.bytes_stream().map_err(std::io::Error::other);
        let mut builder = Response::builder().status(status);
        if let Some(headers) = builder.headers_mut() {
            for (name, value) in resp_headers {
                if let (Ok(name), Ok(value)) = (
                    axum::http::HeaderName::from_bytes(name.as_bytes()),
                    axum::http::HeaderValue::from_str(&value),
                ) {
                    headers.insert(name, value);
                }
            }
        }
        builder.body(Body::from_stream(stream)).unwrap_or_else(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "proxy response build failed",
            )
                .into_response()
        })
    }

    let app = Router::new()
        .route("/health", get(health))
        .route("/v1/*tail", axum::routing::any(proxy_forward))
        .with_state(state);

    let addr = format!("{}:{}", proxy_cfg.host, proxy_cfg.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| AgentError::config(format!("proxy bind {addr}: {e}")))?;
    tracing::info!("proxy listening on http://{addr}/v1");
    axum::serve(listener, app)
        .await
        .map_err(|e| AgentError::config(format!("proxy serve: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_hermes() {
        let cfg = ProxyConfig::default();
        assert_eq!(cfg.host, "127.0.0.1");
        assert_eq!(cfg.port, 8645);
        assert_eq!(cfg.max_request_bytes, 10 * 1024 * 1024);
        assert!(path_allowed(&cfg.allowed_paths, "/chat/completions"));
        assert!(path_allowed(&cfg.allowed_paths, "/responses"));
        assert!(path_allowed(&cfg.allowed_paths, "/models"));
        assert!(!path_allowed(&cfg.allowed_paths, "/files"));
    }

    #[test]
    fn request_header_filter_strips_hop_by_hop_and_auth() {
        let headers = vec![
            ("content-type".to_string(), "application/json".to_string()),
            ("authorization".to_string(), "Bearer junk".to_string()),
            ("Host".to_string(), "127.0.0.1:8645".to_string()),
            ("x-request-id".to_string(), "abc".to_string()),
            ("Accept-Encoding".to_string(), "gzip".to_string()),
        ];
        let kept = filter_request_headers(&headers);
        let names: Vec<&str> = kept.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"content-type"));
        assert!(names.contains(&"x-request-id"));
        assert!(!names.contains(&"authorization"));
        assert!(!names.iter().any(|n| n.eq_ignore_ascii_case("host")));
        assert!(!names
            .iter()
            .any(|n| n.eq_ignore_ascii_case("accept-encoding")));
    }

    #[test]
    fn response_header_filter_drops_framing() {
        let headers = vec![
            ("content-type".to_string(), "text/event-stream".to_string()),
            ("content-length".to_string(), "100".to_string()),
            ("content-encoding".to_string(), "gzip".to_string()),
            ("transfer-encoding".to_string(), "chunked".to_string()),
            ("x-trace".to_string(), "t".to_string()),
        ];
        let kept = filter_response_headers(&headers);
        let names: Vec<&str> = kept.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"content-type"));
        assert!(names.contains(&"x-trace"));
        assert!(!names.contains(&"content-length"));
        assert!(!names.contains(&"content-encoding"));
        assert!(!names.contains(&"transfer-encoding"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn server_forwards_with_resolved_bearer() {
        use axum::routing::{post, Router};
        use serde_json::Value;

        // Mock upstream: echoes the received Authorization header.
        async fn capture(headers: axum::http::HeaderMap) -> axum::Json<Value> {
            let auth = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            axum::Json(json!({"auth": auth}))
        }
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_port = upstream_listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(
                upstream_listener,
                Router::new().route("/v1/chat/completions", post(capture)),
            )
            .await
            .unwrap();
        });

        // Temp home with a stored (non-expiring) token.
        let home = tempfile::tempdir().unwrap();
        let tokens = crate::oauth::StoredTokens {
            access_token: "real-token".into(),
            refresh_token: String::new(),
            expires_at: 0,
            scope: String::new(),
        };
        crate::oauth::save_tokens(home.path(), &tokens).unwrap();

        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_port = probe.local_addr().unwrap().port();
        drop(probe);

        let proxy_cfg = ProxyConfig {
            upstream_url: format!("http://127.0.0.1:{upstream_port}/v1"),
            port: proxy_port,
            ..ProxyConfig::default()
        };
        let server_cfg = proxy_cfg.clone();
        let home_path = home.path().to_path_buf();
        tokio::spawn(async move {
            if let Err(e) =
                run_server(crate::oauth::OAuthConfig::default(), server_cfg, home_path).await
            {
                eprintln!("proxy server exited: {e}");
            }
        });

        let client = reqwest::Client::new();
        let mut up = false;
        for _ in 0..50 {
            let ok = client
                .get(format!("http://127.0.0.1:{proxy_port}/health"))
                .send()
                .await
                .map(|r| r.status().is_success())
                .unwrap_or(false);
            if ok {
                up = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(up, "proxy did not come up");

        // The client's junk bearer is replaced by the stored token.
        let resp = client
            .post(format!("http://127.0.0.1:{proxy_port}/v1/chat/completions"))
            .bearer_auth("junk")
            .json(&json!({"model": "x"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: Value = resp.json().await.unwrap();
        assert_eq!(
            body.get("auth").and_then(|v| v.as_str()),
            Some("Bearer real-token")
        );

        // Disallowed paths 404.
        let resp = client
            .post(format!("http://127.0.0.1:{proxy_port}/v1/files"))
            .bearer_auth("junk")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
    }

    #[test]
    fn credential_expiry_rendering() {
        let cfg = ProxyConfig {
            upstream_url: "https://example.com/v1/".into(),
            ..ProxyConfig::default()
        };
        let tokens = crate::oauth::StoredTokens {
            access_token: "tok".into(),
            refresh_token: String::new(),
            expires_at: 0,
            scope: String::new(),
        };
        let cred = credential_from_tokens(&tokens, &cfg);
        assert_eq!(cred.bearer, "tok");
        assert_eq!(cred.base_url, "https://example.com/v1");
        assert_eq!(cred.expires_at, "");
    }
}

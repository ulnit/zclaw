//! MCP OAuth 2.1 client — port of hermes' `tools/mcp_oauth.py` +
//! `tools/mcp_oauth_manager.py` core (the MCP SDK's `OAuthClientProvider`
//! is replaced by a hand-rolled implementation: metadata discovery,
//! dynamic client registration, PKCE authorization-code flow with a
//! loopback callback server, token exchange + refresh, disk storage).
//!
//! Token layout mirrors hermes (`HERMES_HOME/mcp-tokens/` →
//! `ULNCLAW_HOME/mcp-tokens/`):
//!
//! ```text
//! mcp-tokens/<server>.json         -- tokens (access/refresh/expires_at)
//! mcp-tokens/<server>.client.json  -- dynamic client registration info
//! mcp-tokens/<server>.meta.json    -- authorization-server metadata
//! ```

use crate::error::{AgentError, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long the loopback callback server waits for the browser redirect
/// (hermes `_wait_for_callback` default).
const CALLBACK_TIMEOUT_SECS: u64 = 300;
/// Refresh tokens this many seconds before actual expiry.
const EXPIRY_SKEW_SECS: u64 = 30;
const HTTP_TIMEOUT_SECS: u64 = 15;

/// Per-server OAuth settings (hermes `mcp_servers.<name>.oauth`; every
/// field optional).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct McpOAuthConfig {
    /// Pre-registered client id — skips dynamic client registration.
    pub client_id: Option<String>,
    /// Client secret (confidential clients only).
    pub client_secret: Option<String>,
    /// Requested scope (default: server-provided).
    pub scope: Option<String>,
    /// Loopback callback port; 0 = auto-pick a free port.
    pub redirect_port: u16,
    /// Full redirect URI override (default: loopback callback).
    pub redirect_uri: Option<String>,
    /// Loopback hostname used in the default redirect URI (WAF-safe
    /// `localhost` default, hermes `redirect_host`).
    pub redirect_host: String,
    /// Client name advertised during registration (default "ulnclaw").
    pub client_name: String,
}

impl McpOAuthConfig {
    pub fn redirect_host_or_default(&self) -> &str {
        if self.redirect_host.is_empty() {
            "localhost"
        } else {
            &self.redirect_host
        }
    }
    pub fn client_name_or_default(&self) -> &str {
        if self.client_name.is_empty() {
            "ulnclaw"
        } else {
            &self.client_name
        }
    }
}

/// Authorization-server metadata (the endpoints we need).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OAuthMetadata {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    #[serde(default)]
    pub registration_endpoint: Option<String>,
}

/// Client registration info (dynamic or pre-configured).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ClientInfo {
    pub client_id: String,
    #[serde(default)]
    pub client_secret: Option<String>,
}

/// Persisted token set.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StoredTokens {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Absolute wall-clock expiry (unix seconds) — hermes `expires_at`.
    #[serde(default)]
    pub expires_at: u64,
}

impl StoredTokens {
    pub fn is_valid(&self) -> bool {
        self.expires_at == 0 || now_secs() + EXPIRY_SKEW_SECS < self.expires_at
    }
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Sanitize a server name for use as a filename (hermes `_safe_filename`).
pub fn safe_filename(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = sanitized.trim_matches('_');
    let truncated: String = trimmed.chars().take(128).collect();
    if truncated.is_empty() {
        "default".to_string()
    } else {
        truncated
    }
}

pub fn token_dir(home: &Path) -> PathBuf {
    home.join("mcp-tokens")
}

fn write_json_private(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| AgentError::Tool(format!("create {}: {}", parent.display(), e)))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).ok();
        }
    }
    let body = serde_json::to_string(value)
        .map_err(|e| AgentError::Tool(format!("serialize {}: {}", path.display(), e)))?;
    std::fs::write(path, body)
        .map_err(|e| AgentError::Tool(format!("write {}: {}", path.display(), e)))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).ok();
    }
    Ok(())
}

fn read_json(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

pub fn load_tokens(home: &Path, server_name: &str) -> Option<StoredTokens> {
    let path = token_dir(home).join(format!("{}.json", safe_filename(server_name)));
    serde_json::from_value(read_json(&path)?).ok()
}

pub fn save_tokens(home: &Path, server_name: &str, tokens: &StoredTokens) -> Result<()> {
    let path = token_dir(home).join(format!("{}.json", safe_filename(server_name)));
    write_json_private(&path, &serde_json::to_value(tokens).unwrap_or_default())
}

/// Delete only the stored tokens for a server (client registration and
/// metadata stay put — they remain valid across re-authorization). Used
/// by the dashboard OAuth worker to force a fresh authorization (hermes
/// `manager.remove` ahead of the probe).
pub fn remove_tokens(home: &Path, server_name: &str) {
    let path = token_dir(home).join(format!("{}.json", safe_filename(server_name)));
    std::fs::remove_file(path).ok();
}

pub fn load_client_info(home: &Path, server_name: &str) -> Option<ClientInfo> {
    let path = token_dir(home).join(format!("{}.client.json", safe_filename(server_name)));
    serde_json::from_value(read_json(&path)?).ok()
}

fn save_client_info(home: &Path, server_name: &str, client: &ClientInfo) -> Result<()> {
    let path = token_dir(home).join(format!("{}.client.json", safe_filename(server_name)));
    write_json_private(&path, &serde_json::to_value(client).unwrap_or_default())
}

fn load_metadata(home: &Path, server_name: &str) -> Option<OAuthMetadata> {
    let path = token_dir(home).join(format!("{}.meta.json", safe_filename(server_name)));
    serde_json::from_value(read_json(&path)?).ok()
}

fn save_metadata(home: &Path, server_name: &str, metadata: &OAuthMetadata) -> Result<()> {
    let path = token_dir(home).join(format!("{}.meta.json", safe_filename(server_name)));
    write_json_private(&path, &serde_json::to_value(metadata).unwrap_or_default())
}

/// Remove all stored OAuth state for a server (hermes `remove_oauth_tokens`).
pub fn remove_oauth_state(home: &Path, server_name: &str) {
    let dir = token_dir(home);
    let base = safe_filename(server_name);
    for suffix in [".json", ".client.json", ".meta.json"] {
        std::fs::remove_file(dir.join(format!("{base}{suffix}"))).ok();
    }
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
        .build()
        .map_err(|e| AgentError::Tool(format!("oauth http client: {}", e)))
}

fn origin_of(url: &str) -> String {
    reqwest::Url::parse(url)
        .map(|u| {
            format!(
                "{}://{}{}",
                u.scheme(),
                u.host_str().unwrap_or("localhost"),
                u.port().map(|p| format!(":{}", p)).unwrap_or_default()
            )
        })
        .unwrap_or_else(|_| url.to_string())
}

// ---------------------------------------------------------------------------
// Discovery (MCP auth spec: protected-resource → authorization-server)
// ---------------------------------------------------------------------------

/// Discover the authorization-server metadata for an MCP server URL.
///
/// Order (MCP spec 2025-03-26 + RFC 8414):
/// 1. protected-resource metadata at the server origin → first
///    `authorization_servers` entry → its oauth-authorization-server doc;
/// 2. oauth-authorization-server at the server origin directly.
pub async fn discover_metadata(server_url: &str) -> Result<OAuthMetadata> {
    let client = http_client()?;
    let origin = origin_of(server_url);

    // 1. Protected resource metadata.
    let prm_url = format!("{}/.well-known/oauth-protected-resource", origin);
    if let Ok(response) = client.get(&prm_url).send().await {
        if response.status().is_success() {
            if let Ok(prm) = response.json::<Value>().await {
                if let Some(issuer) = prm
                    .get("authorization_servers")
                    .and_then(|v| v.as_array())
                    .and_then(|a| a.first())
                    .and_then(|v| v.as_str())
                {
                    if let Some(metadata) = fetch_as_metadata(&client, issuer).await {
                        return Ok(metadata);
                    }
                }
            }
        }
    }

    // 2. Authorization-server metadata at the origin.
    if let Some(metadata) = fetch_as_metadata(&client, &origin).await {
        return Ok(metadata);
    }
    Err(AgentError::Tool(format!(
        "MCP OAuth: no authorization-server metadata discoverable for {}",
        server_url
    )))
}

async fn fetch_as_metadata(client: &reqwest::Client, issuer: &str) -> Option<OAuthMetadata> {
    let issuer = issuer.trim_end_matches('/');
    let url = format!("{}/.well-known/oauth-authorization-server", issuer);
    let response = client.get(&url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let doc: Value = response.json().await.ok()?;
    let authorization_endpoint = doc.get("authorization_endpoint")?.as_str()?.to_string();
    let token_endpoint = doc.get("token_endpoint")?.as_str()?.to_string();
    let registration_endpoint = doc
        .get("registration_endpoint")
        .and_then(|v| v.as_str())
        .map(String::from);
    Some(OAuthMetadata {
        authorization_endpoint,
        token_endpoint,
        registration_endpoint,
    })
}

// ---------------------------------------------------------------------------
// Dynamic client registration (RFC 7591)
// ---------------------------------------------------------------------------

pub async fn register_client(
    registration_endpoint: &str,
    redirect_uri: &str,
    client_name: &str,
) -> Result<ClientInfo> {
    let client = http_client()?;
    let body = json!({
        "client_name": client_name,
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    let response = client
        .post(registration_endpoint)
        .json(&body)
        .send()
        .await
        .map_err(|e| AgentError::Tool(format!("client registration: {}", e)))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(AgentError::Tool(format!(
            "client registration failed: HTTP {} {}",
            status,
            text.chars().take(200).collect::<String>()
        )));
    }
    let doc: Value = serde_json::from_str(&text)
        .map_err(|e| AgentError::Tool(format!("client registration bad JSON: {}", e)))?;
    let client_id = doc
        .get("client_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AgentError::Tool("client registration: no client_id".into()))?
        .to_string();
    let client_secret = doc
        .get("client_secret")
        .and_then(|v| v.as_str())
        .map(String::from);
    Ok(ClientInfo {
        client_id,
        client_secret,
    })
}

// ---------------------------------------------------------------------------
// PKCE + authorization-code flow
// ---------------------------------------------------------------------------

/// Generate (state, code_verifier, code_challenge) — S256, hermes/MCP spec.
pub fn generate_pkce() -> (String, String, String) {
    let (state, verifier, challenge) = crate::google_chat_oauth::generate_pkce();
    (state, verifier, challenge)
}

/// Build the authorization URL for the browser.
pub fn build_authorization_url(
    authorization_endpoint: &str,
    client_id: &str,
    redirect_uri: &str,
    state: &str,
    code_challenge: &str,
    scope: Option<&str>,
) -> String {
    let mut pairs: Vec<(&str, String)> = vec![
        ("response_type", "code".to_string()),
        ("client_id", client_id.to_string()),
        ("redirect_uri", redirect_uri.to_string()),
        ("state", state.to_string()),
        ("code_challenge", code_challenge.to_string()),
        ("code_challenge_method", "S256".to_string()),
    ];
    if let Some(scope) = scope {
        if !scope.is_empty() {
            pairs.push(("scope", scope.to_string()));
        }
    }
    let query = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish();
    format!("{}?{}", authorization_endpoint, query)
}

/// Bind the loopback callback listener (redirect_port = 0 → free port).
pub fn bind_callback_listener(port: u16) -> Result<(tokio::net::TcpListener, u16)> {
    let bind = |p: u16| std::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], p)));
    let listener = if port == 0 {
        bind(0).map_err(|e| AgentError::Tool(format!("bind loopback callback: {}", e)))?
    } else {
        bind(port)
            .map_err(|e| AgentError::Tool(format!("bind loopback callback port {}: {}", port, e)))?
    };
    let actual = listener
        .local_addr()
        .map(|a| a.port())
        .map_err(|e| AgentError::Tool(format!("callback addr: {}", e)))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| AgentError::Tool(format!("callback nonblocking: {}", e)))?;
    let listener = tokio::net::TcpListener::from_std(listener)
        .map_err(|e| AgentError::Tool(format!("callback listener: {}", e)))?;
    Ok((listener, actual))
}

/// Wait for the OAuth redirect: one GET carrying `code` + `state`. Answers
/// with a small HTML page and returns (code, state).
pub async fn wait_for_callback(
    listener: tokio::net::TcpListener,
    expected_state: &str,
) -> Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let deadline = tokio::time::Instant::now() + Duration::from_secs(CALLBACK_TIMEOUT_SECS);
    loop {
        let accept = tokio::time::timeout_at(deadline, listener.accept()).await;
        let Ok(accept) = accept else {
            return Err(AgentError::Tool(
                "MCP OAuth: timed out waiting for the browser callback".into(),
            ));
        };
        let Ok((mut socket, _)) = accept else {
            continue;
        };
        let mut buffer = vec![0u8; 8192];
        let read = tokio::time::timeout_at(deadline, socket.read(&mut buffer)).await;
        let Ok(Ok(n)) = read else { continue };
        let request = String::from_utf8_lossy(&buffer[..n]);
        let Some(first_line) = request.lines().next() else {
            continue;
        };
        // GET /callback?code=...&state=... HTTP/1.1
        let Some(target) = first_line.split_whitespace().nth(1) else {
            continue;
        };
        let Some(query) = target.split_once('?').map(|(_, q)| q) else {
            continue;
        };
        let params: HashMap<String, String> = url::form_urlencoded::parse(query.as_bytes())
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();

        if let Some(error) = params.get("error") {
            let body = format!("OAuth error: {}\n", error);
            socket
                .write_all(http_response(&body, false).as_bytes())
                .await
                .ok();
            return Err(AgentError::Tool(format!(
                "MCP OAuth: server returned error '{}'",
                error
            )));
        }
        let Some(code) = params.get("code") else {
            continue;
        };
        let state_ok = params
            .get("state")
            .map(|s| s == expected_state)
            .unwrap_or(false);
        let body = if state_ok {
            "<html><body><h2>Authorization complete</h2><p>You can close this window and return to ulnclaw.</p></body></html>\n".to_string()
        } else {
            "OAuth state mismatch — close this window and retry.\n".to_string()
        };
        socket
            .write_all(http_response(&body, state_ok).as_bytes())
            .await
            .ok();
        if !state_ok {
            return Err(AgentError::Tool(
                "MCP OAuth: state mismatch on callback".into(),
            ));
        }
        return Ok(code.clone());
    }
}

fn http_response(body: &str, ok: bool) -> String {
    format!(
        "HTTP/1.1 {} OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        if ok { "200" } else { "400" },
        body.len(),
        body
    )
}

/// Try to open a URL in the user's browser (best effort).
pub fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "linux")]
    let result = std::process::Command::new("xdg-open").arg(url).spawn();
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let result: std::io::Result<std::process::Child> = Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "no browser launcher",
    ));
    let _ = result;
}

// ---------------------------------------------------------------------------
// Token exchange + refresh
// ---------------------------------------------------------------------------

pub async fn exchange_code(
    token_endpoint: &str,
    client: &ClientInfo,
    code: &str,
    code_verifier: &str,
    redirect_uri: &str,
) -> Result<StoredTokens> {
    let mut form: Vec<(&str, String)> = vec![
        ("grant_type", "authorization_code".into()),
        ("code", code.to_string()),
        ("code_verifier", code_verifier.to_string()),
        ("redirect_uri", redirect_uri.to_string()),
        ("client_id", client.client_id.clone()),
    ];
    if let Some(secret) = &client.client_secret {
        form.push(("client_secret", secret.clone()));
    }
    token_request(token_endpoint, &form).await
}

pub async fn refresh_grant(
    token_endpoint: &str,
    client: &ClientInfo,
    refresh_token: &str,
    scope: Option<&str>,
) -> Result<StoredTokens> {
    let mut form: Vec<(&str, String)> = vec![
        ("grant_type", "refresh_token".into()),
        ("refresh_token", refresh_token.to_string()),
        ("client_id", client.client_id.clone()),
    ];
    if let Some(secret) = &client.client_secret {
        form.push(("client_secret", secret.clone()));
    }
    if let Some(scope) = scope {
        if !scope.is_empty() {
            form.push(("scope", scope.to_string()));
        }
    }
    token_request(token_endpoint, &form).await
}

async fn token_request(token_endpoint: &str, form: &[(&str, String)]) -> Result<StoredTokens> {
    let client = http_client()?;
    let response = client
        .post(token_endpoint)
        .form(form)
        .send()
        .await
        .map_err(|e| AgentError::Tool(format!("token request: {}", e)))?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(AgentError::Tool(format!(
            "token request failed: HTTP {} {}",
            status,
            text.chars().take(200).collect::<String>()
        )));
    }
    let doc: Value = serde_json::from_str(&text)
        .map_err(|e| AgentError::Tool(format!("token response bad JSON: {}", e)))?;
    let access_token = doc
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AgentError::Tool("token response: no access_token".into()))?
        .to_string();
    let refresh_token = doc
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .map(String::from);
    let expires_in = doc.get("expires_in").and_then(|v| v.as_u64()).unwrap_or(0);
    Ok(StoredTokens {
        access_token,
        refresh_token,
        expires_at: if expires_in > 0 {
            now_secs() + expires_in
        } else {
            0
        },
    })
}

// ---------------------------------------------------------------------------
// Orchestration (hermes build_oauth_auth + MCPOAuthManager essentials)
// ---------------------------------------------------------------------------

/// Resolve a valid access token for a server: disk cache → refresh grant →
/// full interactive authorization-code flow. Tokens/registrations persist
/// under `<home>/mcp-tokens/` (0600) and survive restarts.
///
/// `interactive` must be false in unattended contexts (cron, gateway
/// sessions without a human) — the full flow then fails fast instead of
/// hanging on a browser that nobody can drive (hermes
/// `OAuthNonInteractiveError`).
pub async fn get_access_token(
    home: &Path,
    server_name: &str,
    server_url: &str,
    cfg: &McpOAuthConfig,
    interactive: bool,
) -> Result<String> {
    // 1. Cached tokens.
    if let Some(tokens) = load_tokens(home, server_name) {
        if tokens.is_valid() {
            return Ok(tokens.access_token);
        }
        // 2. Refresh grant.
        if let Some(refresh_token) = tokens.refresh_token.clone() {
            let metadata = resolve_metadata(home, server_name, server_url).await?;
            let client = resolve_client(home, server_name, &metadata, cfg, None).await?;
            match refresh_grant(
                &metadata.token_endpoint,
                &client,
                &refresh_token,
                cfg.scope.as_deref(),
            )
            .await
            {
                Ok(tokens) => {
                    save_tokens(home, server_name, &tokens)?;
                    return Ok(tokens.access_token);
                }
                Err(e) => {
                    eprintln!(
                        "[mcp-oauth] {}: refresh failed ({}); re-authorizing",
                        server_name, e
                    );
                }
            }
        }
    }

    // 3. Full authorization-code flow.
    if !interactive {
        return Err(AgentError::Tool(format!(
            "MCP OAuth for '{}' requires interactive authorization (open the printed URL in a browser); this session is unattended",
            server_name
        )));
    }
    let metadata = resolve_metadata(home, server_name, server_url).await?;

    // In a dashboard-mediated flow (hermes `dashboard_oauth_flow`
    // contextvar) the authorization URL is published to the gateway flow
    // and the browser redirect lands on the gateway's callback route
    // instead of a loopback listener (hermes `_resolve_callback_port`
    // dashboard branch: no port reserved, redirect_uri = the gateway
    // callback URL).
    let dashboard_flow = crate::mcp::dashboard_oauth::current_flow();
    let (listener, redirect_uri) = match dashboard_flow.as_ref() {
        Some(flow) => (
            None,
            cfg.redirect_uri
                .clone()
                .unwrap_or_else(|| flow.redirect_uri.clone()),
        ),
        None => {
            let (listener, port) = bind_callback_listener(cfg.redirect_port)?;
            let uri = cfg.redirect_uri.clone().unwrap_or_else(|| {
                format!(
                    "http://{}:{}/callback",
                    cfg.redirect_host_or_default(),
                    port
                )
            });
            (Some(listener), uri)
        }
    };
    let client = resolve_client(home, server_name, &metadata, cfg, Some(&redirect_uri)).await?;

    let (state, verifier, challenge) = generate_pkce();
    let auth_url = build_authorization_url(
        &metadata.authorization_endpoint,
        &client.client_id,
        &redirect_uri,
        &state,
        &challenge,
        cfg.scope.as_deref(),
    );
    let code = if let Some(flow) = dashboard_flow.as_ref() {
        flow.publish_authorization_url(&auth_url)?;
        flow.wait_for_callback(Duration::from_secs(CALLBACK_TIMEOUT_SECS))
            .await?
            .0
    } else {
        println!(
            "[mcp-oauth] {}: authorize in your browser (waiting up to {}s for the callback):",
            server_name, CALLBACK_TIMEOUT_SECS
        );
        println!("  {}", auth_url);
        open_browser(&auth_url);
        let listener =
            listener.expect("loopback listener is always bound outside dashboard-mediated flows");
        wait_for_callback(listener, &state).await?
    };
    let tokens = exchange_code(
        &metadata.token_endpoint,
        &client,
        &code,
        &verifier,
        &redirect_uri,
    )
    .await?;
    save_tokens(home, server_name, &tokens)?;
    Ok(tokens.access_token)
}

/// 401 recovery (hermes `MCPOAuthManager.handle_401` without the
/// in-flight-future dedupe — callers serialize per server): try a refresh
/// grant on stored tokens first, then fall back to the full flow when a
/// human is present.
pub async fn recover_token(
    home: &Path,
    server_name: &str,
    server_url: &str,
    cfg: &McpOAuthConfig,
    interactive: bool,
) -> Result<String> {
    if let Some(tokens) = load_tokens(home, server_name) {
        if let Some(refresh_token) = tokens.refresh_token {
            let metadata = resolve_metadata(home, server_name, server_url).await?;
            let client = resolve_client(home, server_name, &metadata, cfg, None).await?;
            match refresh_grant(
                &metadata.token_endpoint,
                &client,
                &refresh_token,
                cfg.scope.as_deref(),
            )
            .await
            {
                Ok(tokens) => {
                    save_tokens(home, server_name, &tokens)?;
                    return Ok(tokens.access_token);
                }
                Err(e) => {
                    eprintln!(
                        "[mcp-oauth] {}: refresh failed ({}); re-authorizing",
                        server_name, e
                    );
                }
            }
        }
    }
    if !interactive {
        return Err(AgentError::Tool(format!(
            "MCP OAuth for '{}': token rejected and no refresh possible in an unattended session",
            server_name
        )));
    }
    // Full re-authorization.
    remove_oauth_state(home, server_name);
    get_access_token(home, server_name, server_url, cfg, interactive).await
}

async fn resolve_metadata(
    home: &Path,
    server_name: &str,
    server_url: &str,
) -> Result<OAuthMetadata> {
    if let Some(metadata) = load_metadata(home, server_name) {
        return Ok(metadata);
    }
    let metadata = discover_metadata(server_url).await?;
    save_metadata(home, server_name, &metadata).ok();
    Ok(metadata)
}

async fn resolve_client(
    home: &Path,
    server_name: &str,
    metadata: &OAuthMetadata,
    cfg: &McpOAuthConfig,
    effective_redirect_uri: Option<&str>,
) -> Result<ClientInfo> {
    if let Some(client) = load_client_info(home, server_name) {
        return Ok(client);
    }
    let client = if let Some(client_id) = &cfg.client_id {
        ClientInfo {
            client_id: client_id.clone(),
            client_secret: cfg.client_secret.clone(),
        }
    } else {
        let Some(registration_endpoint) = &metadata.registration_endpoint else {
            return Err(AgentError::Tool(format!(
                "MCP OAuth for '{}': no client_id configured and the server advertises no registration_endpoint",
                server_name
            )));
        };
        // The registered redirect_uri must equal the one used in the
        // authorization request (providers reject mismatches); pass the
        // effective URI when the caller already resolved it.
        let redirect_uri = effective_redirect_uri
            .map(str::to_string)
            .unwrap_or_else(|| {
                cfg.redirect_uri.clone().unwrap_or_else(|| {
                    format!("http://{}/callback", cfg.redirect_host_or_default())
                })
            });
        register_client(
            registration_endpoint,
            &redirect_uri,
            cfg.client_name_or_default(),
        )
        .await?
    };
    save_client_info(home, server_name, &client).ok();
    Ok(client)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_filename_matches_hermes_semantics() {
        assert_eq!(safe_filename("my-server_1"), "my-server_1");
        assert_eq!(safe_filename("a/b\\c:d"), "a_b_c_d");
        assert_eq!(safe_filename("///"), "default");
        assert_eq!(safe_filename(""), "default");
        let long = "x".repeat(200);
        assert_eq!(safe_filename(&long).len(), 128);
    }

    #[test]
    fn stored_token_validity() {
        let valid = StoredTokens {
            access_token: "t".into(),
            refresh_token: None,
            expires_at: now_secs() + 3600,
        };
        assert!(valid.is_valid());
        let expired = StoredTokens {
            access_token: "t".into(),
            refresh_token: None,
            expires_at: now_secs() - 10,
        };
        assert!(!expired.is_valid());
        // expires_at = 0 means "no expiry advertised" — treated as valid.
        let no_expiry = StoredTokens {
            access_token: "t".into(),
            refresh_token: None,
            expires_at: 0,
        };
        assert!(no_expiry.is_valid());
    }

    #[test]
    fn authorization_url_shape() {
        let url = build_authorization_url(
            "https://as.example.com/authorize",
            "client-1",
            "http://localhost:9999/callback",
            "st4te",
            "ch4llenge",
            Some("read write"),
        );
        assert!(url.starts_with("https://as.example.com/authorize?"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("client_id=client-1"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("scope=read+write"));
        assert!(url.contains("state=st4te"));
    }

    #[test]
    fn storage_roundtrip_and_permissions() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let tokens = StoredTokens {
            access_token: "access".into(),
            refresh_token: Some("refresh".into()),
            expires_at: 12345,
        };
        save_tokens(home, "srv/one", &tokens).unwrap();
        let loaded = load_tokens(home, "srv/one").expect("tokens roundtrip");
        assert_eq!(loaded.access_token, "access");
        assert_eq!(loaded.refresh_token.as_deref(), Some("refresh"));

        let client = ClientInfo {
            client_id: "cid".into(),
            client_secret: None,
        };
        save_client_info(home, "srv/one", &client).unwrap();
        assert_eq!(load_client_info(home, "srv/one").unwrap().client_id, "cid");

        let metadata = OAuthMetadata {
            authorization_endpoint: "https://as/a".into(),
            token_endpoint: "https://as/t".into(),
            registration_endpoint: None,
        };
        save_metadata(home, "srv/one", &metadata).unwrap();
        assert_eq!(
            load_metadata(home, "srv/one").unwrap().token_endpoint,
            "https://as/t"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = token_dir(home).join("srv_one.json");
            let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
            let dir_mode = std::fs::metadata(token_dir(home))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(dir_mode, 0o700);
        }

        remove_oauth_state(home, "srv/one");
        assert!(load_tokens(home, "srv/one").is_none());
        assert!(load_client_info(home, "srv/one").is_none());
        assert!(load_metadata(home, "srv/one").is_none());
    }

    #[tokio::test]
    async fn discovery_prefers_protected_resource_metadata() {
        use axum::routing::get;
        use axum::Router;

        async fn prm() -> axum::Json<Value> {
            axum::Json(json!({"authorization_servers": ["http://AS_PLACEHOLDER"]}))
        }
        // The PRM points at a second mock AS server; stand it up first so
        // the placeholder can be substituted.
        async fn as_doc() -> axum::Json<Value> {
            axum::Json(json!({
                "authorization_endpoint": "https://as.example.com/authorize",
                "token_endpoint": "https://as.example.com/token",
                "registration_endpoint": "https://as.example.com/register"
            }))
        }

        let as_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let as_port = as_listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let app = Router::new().route("/.well-known/oauth-authorization-server", get(as_doc));
            axum::serve(as_listener, app).await.ok();
        });

        let prm_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let prm_port = prm_listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let app = Router::new().route(
                "/.well-known/oauth-protected-resource",
                get(move || async move {
                    axum::Json(json!({
                        "authorization_servers": [format!("http://127.0.0.1:{}", as_port)]
                    }))
                }),
            );
            axum::serve(prm_listener, app).await.ok();
        });
        let _ = prm; // keep the helper for clarity

        let metadata = discover_metadata(&format!("http://127.0.0.1:{}/mcp", prm_port))
            .await
            .expect("discovery");
        assert_eq!(metadata.token_endpoint, "https://as.example.com/token");
        assert_eq!(
            metadata.registration_endpoint.as_deref(),
            Some("https://as.example.com/register")
        );
    }

    #[tokio::test]
    async fn discovery_falls_back_to_origin_as_metadata() {
        use axum::routing::get;
        use axum::Router;

        async fn as_doc() -> axum::Json<Value> {
            axum::Json(json!({
                "authorization_endpoint": "https://as2.example.com/authorize",
                "token_endpoint": "https://as2.example.com/token"
            }))
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let app = Router::new().route("/.well-known/oauth-authorization-server", get(as_doc));
            axum::serve(listener, app).await.ok();
        });

        let metadata = discover_metadata(&format!("http://127.0.0.1:{}/mcp", port))
            .await
            .expect("fallback discovery");
        assert_eq!(
            metadata.authorization_endpoint,
            "https://as2.example.com/authorize"
        );
    }

    /// Full authorization-code flow against a mock AS: registration →
    /// browser redirect (driven by the test) → exchange → stored tokens.
    #[tokio::test]
    async fn full_authorization_flow_end_to_end() {
        use axum::extract::{Form, State};
        use axum::routing::post;
        use axum::Router;
        use std::sync::Arc;

        #[derive(Clone)]
        struct AsState {
            issued: Arc<tokio::sync::Mutex<Vec<Value>>>,
        }

        async fn register() -> axum::Json<Value> {
            axum::Json(json!({"client_id": "dyn-client", "client_secret": null}))
        }
        async fn token(
            State(state): State<AsState>,
            Form(form): Form<std::collections::HashMap<String, String>>,
        ) -> axum::Json<Value> {
            let grant = form.get("grant_type").cloned().unwrap_or_default();
            state.issued.lock().await.push(json!(form.clone()));
            match grant.as_str() {
                "authorization_code" => {
                    assert_eq!(form.get("code").map(String::as_str), Some("auth-code-1"));
                    assert!(form
                        .get("code_verifier")
                        .map(|v| !v.is_empty())
                        .unwrap_or(false));
                    axum::Json(json!({
                        "access_token": "access-1",
                        "refresh_token": "refresh-1",
                        "expires_in": 3600
                    }))
                }
                "refresh_token" => axum::Json(json!({
                    "access_token": "access-2",
                    "refresh_token": "refresh-2",
                    "expires_in": 3600
                })),
                _ => axum::Json(json!({"error": "unsupported_grant_type"})),
            }
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = AsState {
            issued: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        };
        let issued = state.issued.clone();
        tokio::spawn(async move {
            let app = Router::new()
                .route("/register", post(register))
                .route("/token", post(token))
                .with_state(state);
            axum::serve(listener, app).await.ok();
        });

        let base = format!("http://127.0.0.1:{}", port);
        let metadata = OAuthMetadata {
            authorization_endpoint: format!("{}/authorize", base),
            token_endpoint: format!("{}/token", base),
            registration_endpoint: Some(format!("{}/register", base)),
        };

        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let cfg = McpOAuthConfig {
            redirect_port: 0,
            ..Default::default()
        };

        // Manual orchestration (the production path opens a browser; the
        // test drives the redirect itself).
        let (listener, cb_port) = bind_callback_listener(cfg.redirect_port).unwrap();
        let redirect_uri = format!("http://localhost:{}/callback", cb_port);
        let client = register_client(
            metadata.registration_endpoint.as_ref().unwrap(),
            &redirect_uri,
            "ulnclaw",
        )
        .await
        .expect("registration");
        assert_eq!(client.client_id, "dyn-client");

        let (state_value, verifier, challenge) = generate_pkce();
        let auth_url = build_authorization_url(
            &metadata.authorization_endpoint,
            &client.client_id,
            &redirect_uri,
            &state_value,
            &challenge,
            None,
        );
        // Simulate the browser hitting the callback.
        let redirect = format!("{}?code=auth-code-1&state={}", redirect_uri, state_value);
        let expected_state = state_value.clone();
        let waiter =
            tokio::spawn(async move { wait_for_callback(listener, &expected_state).await });
        let fetched = reqwest::get(&redirect).await.expect("callback GET");
        assert_eq!(fetched.status(), 200);
        let code = waiter.await.unwrap().expect("callback captured");
        assert_eq!(code, "auth-code-1");
        assert!(auth_url.contains("code_challenge="));

        let tokens = exchange_code(
            &metadata.token_endpoint,
            &client,
            &code,
            &verifier,
            &redirect_uri,
        )
        .await
        .expect("token exchange");
        assert_eq!(tokens.access_token, "access-1");
        save_tokens(home, "flow-srv", &tokens).unwrap();
        save_client_info(home, "flow-srv", &client).unwrap();
        save_metadata(home, "flow-srv", &metadata).unwrap();

        // get_access_token serves the cached token without network.
        let token = get_access_token(home, "flow-srv", &base, &cfg, false)
            .await
            .expect("cached token");
        assert_eq!(token, "access-1");

        // Expired token + refresh_token → refresh grant (still unattended).
        let mut expired = tokens.clone();
        expired.expires_at = now_secs() - 10;
        save_tokens(home, "flow-srv", &expired).unwrap();
        let token = get_access_token(home, "flow-srv", &base, &cfg, false)
            .await
            .expect("refreshed token");
        assert_eq!(token, "access-2");
        let refreshed = load_tokens(home, "flow-srv").unwrap();
        assert_eq!(refreshed.refresh_token.as_deref(), Some("refresh-2"));
        let grants = issued.lock().await;
        assert_eq!(grants.len(), 2);
    }

    /// Dashboard-mediated flow (P242): get_access_token runs inside a
    /// `scope_flow` task-local — the authorization URL is published to the
    /// flow (no loopback listener, no browser), the redirect_uri is the
    /// gateway callback URL, and the code arrives via `deliver_callback`.
    #[tokio::test]
    async fn dashboard_mediated_flow_end_to_end() {
        use axum::extract::{Form, State};
        use axum::routing::post;
        use axum::Router;
        use std::sync::Arc;

        #[derive(Clone)]
        struct AsState {
            issued: Arc<tokio::sync::Mutex<Vec<Value>>>,
        }

        async fn register() -> axum::Json<Value> {
            axum::Json(json!({"client_id": "dyn-client", "client_secret": null}))
        }
        async fn token(
            State(state): State<AsState>,
            Form(form): Form<std::collections::HashMap<String, String>>,
        ) -> axum::Json<Value> {
            state.issued.lock().await.push(json!(form.clone()));
            // The registered redirect_uri must be the gateway callback URL.
            assert!(form
                .get("redirect_uri")
                .map(|u| u.contains("/api/mcp/oauth/callback/dash-srv"))
                .unwrap_or(false));
            axum::Json(json!({
                "access_token": "access-dash",
                "refresh_token": "refresh-dash",
                "expires_in": 3600
            }))
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = AsState {
            issued: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        };
        tokio::spawn(async move {
            let app = Router::new()
                .route("/register", post(register))
                .route("/token", post(token))
                .with_state(state);
            axum::serve(listener, app).await.ok();
        });

        let base = format!("http://127.0.0.1:{}", port);
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let metadata = OAuthMetadata {
            authorization_endpoint: format!("{}/authorize", base),
            token_endpoint: format!("{}/token", base),
            registration_endpoint: Some(format!("{}/register", base)),
        };
        save_metadata(&home, "dash-srv", &metadata).unwrap();

        let flow = crate::mcp::dashboard_oauth::DashboardOAuthFlow::new(
            "flow-dash".into(),
            "dash-srv".into(),
            None,
            home.to_string_lossy().to_string(),
            "http://127.0.0.1:8642/api/mcp/oauth/callback/dash-srv".into(),
        );
        let cfg = McpOAuthConfig::default();
        let worker_flow = flow.clone();
        let worker_home = home.clone();
        let worker_base = base.clone();
        let worker = tokio::spawn(crate::mcp::dashboard_oauth::scope_flow(
            worker_flow,
            async move { get_access_token(&worker_home, "dash-srv", &worker_base, &cfg, true).await },
        ));

        // The authorization URL surfaces through the flow, carrying the
        // gateway redirect_uri and a state parameter.
        let url = flow
            .wait_for_authorization_url(std::time::Duration::from_secs(10))
            .await
            .expect("authorization url published");
        let parsed = url::Url::parse(&url).expect("valid url");
        let redirect: String = parsed
            .query_pairs()
            .find(|(k, _)| k == "redirect_uri")
            .map(|(_, v)| v.into_owned())
            .unwrap();
        assert_eq!(
            redirect,
            "http://127.0.0.1:8642/api/mcp/oauth/callback/dash-srv"
        );
        let state_value: String = parsed
            .query_pairs()
            .find(|(k, _)| k == "state")
            .map(|(_, v)| v.into_owned())
            .expect("state present");

        // The gateway callback route delivers the browser redirect.
        flow.deliver_callback(Some("auth-code-dash"), Some(&state_value), None)
            .unwrap();
        let token = worker.await.unwrap().expect("flow completed");
        assert_eq!(token, "access-dash");
        let stored = load_tokens(&home, "dash-srv").expect("tokens saved");
        assert_eq!(stored.access_token, "access-dash");
    }

    #[tokio::test]
    async fn non_interactive_without_tokens_fails_fast() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = McpOAuthConfig::default();
        let err = get_access_token(
            tmp.path(),
            "no-tokens",
            "http://127.0.0.1:1/mcp", // nothing listens: must fail BEFORE network
            &cfg,
            false,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("interactive"), "{}", err);
    }
}

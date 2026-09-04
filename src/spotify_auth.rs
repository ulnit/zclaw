//! Spotify OAuth PKCE authentication (hermes `hermes auth spotify` —
//! `hermes_cli/auth.py` spotify sections).
//!
//! Tokens persist under `providers.spotify` in `<home>/auth.json`
//! (the same store the Nous/xAI OAuth flows use). Runtime resolution
//! refreshes access tokens early (120 s skew) via the refresh-token
//! grant and quarantines dead tokens on terminal refresh failure.

use std::sync::Mutex;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};

pub const DEFAULT_ACCOUNTS_BASE_URL: &str = "https://accounts.spotify.com";
pub const DEFAULT_API_BASE_URL: &str = "https://api.spotify.com/v1";
pub const DEFAULT_REDIRECT_URI: &str = "http://127.0.0.1:43827/spotify/callback";
/// Refresh up to two minutes early (hermes
/// `SPOTIFY_ACCESS_TOKEN_REFRESH_SKEW_SECONDS`).
pub const REFRESH_SKEW_SECONDS: i64 = 120;
const DEFAULT_CALLBACK_TIMEOUT_SECS: u64 = 180;

pub const DEFAULT_SCOPE: &str = "user-modify-playback-state user-read-playback-state \
     user-read-currently-playing user-read-recently-played playlist-read-private \
     playlist-read-collaborative playlist-modify-public playlist-modify-private \
     user-library-read user-library-modify";

/// Structured auth failure (hermes `AuthError` with provider/code).
#[derive(Debug)]
pub struct SpotifyAuthError {
    pub message: String,
    pub code: &'static str,
    pub relogin_required: bool,
}

impl std::fmt::Display for SpotifyAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

fn err(message: impl Into<String>, code: &'static str, relogin: bool) -> SpotifyAuthError {
    SpotifyAuthError {
        message: message.into(),
        code,
        relogin_required: relogin,
    }
}

fn auth_store_lock() -> &'static Mutex<()> {
    static LOCK: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn auth_json_path() -> std::path::PathBuf {
    crate::managed_gateway::auth_json_path()
}

fn load_auth_store() -> Value {
    std::fs::read_to_string(auth_json_path())
        .ok()
        .and_then(|body| serde_json::from_str::<Value>(&body).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}

fn save_auth_store(store: &Value) -> Result<std::path::PathBuf, SpotifyAuthError> {
    let path = auth_json_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let pretty = serde_json::to_string_pretty(store).map_err(|e| {
        err(
            format!("serialize auth store: {e}"),
            "spotify_store_invalid",
            false,
        )
    })?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, pretty).map_err(|e| {
        err(
            format!("write auth store: {e}"),
            "spotify_store_write_failed",
            false,
        )
    })?;
    std::fs::rename(&tmp, &path).map_err(|e| {
        err(
            format!("commit auth store: {e}"),
            "spotify_store_write_failed",
            false,
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).ok();
    }
    Ok(path)
}

/// `providers.spotify` state block, if present.
pub fn provider_state() -> Option<Value> {
    let store = load_auth_store();
    let state = store.get("providers")?.get("spotify")?;
    if state.is_object() {
        Some(state.clone())
    } else {
        None
    }
}

pub fn store_provider_state(state: &Value) -> Result<std::path::PathBuf, SpotifyAuthError> {
    let _guard = auth_store_lock()
        .lock()
        .map_err(|_| err("auth store lock poisoned", "spotify_store_locked", false))?;
    let mut store = load_auth_store();
    if !store
        .get("providers")
        .map(Value::is_object)
        .unwrap_or(false)
    {
        store["providers"] = json!({});
    }
    store["providers"]["spotify"] = state.clone();
    save_auth_store(&store)
}

/// hermes `_spotify_client_id`: explicit flag → `ULNCLAW_SPOTIFY_CLIENT_ID`
/// → `SPOTIFY_CLIENT_ID` → stored state.
pub fn resolve_client_id(
    explicit: Option<&str>,
    state: Option<&Value>,
) -> Result<String, SpotifyAuthError> {
    let candidates: [Option<String>; 4] = [
        explicit
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty()),
        crate::config::get_env_value("ULNCLAW_SPOTIFY_CLIENT_ID")
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty()),
        crate::config::get_env_value("SPOTIFY_CLIENT_ID")
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty()),
        state
            .and_then(|s| s.get("client_id"))
            .and_then(Value::as_str)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty()),
    ];
    for candidate in candidates.into_iter().flatten() {
        return Ok(candidate);
    }
    Err(err(
        "Spotify client_id is required. Set ULNCLAW_SPOTIFY_CLIENT_ID/SPOTIFY_CLIENT_ID or pass --client-id.",
        "spotify_client_id_missing",
        false,
    ))
}

/// hermes `_spotify_redirect_uri`.
pub fn resolve_redirect_uri(explicit: Option<&str>, state: Option<&Value>) -> String {
    let candidates: [Option<String>; 5] = [
        explicit
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty()),
        crate::config::get_env_value("ULNCLAW_SPOTIFY_REDIRECT_URI")
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty()),
        crate::config::get_env_value("SPOTIFY_REDIRECT_URI")
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty()),
        state
            .and_then(|s| s.get("redirect_uri"))
            .and_then(Value::as_str)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty()),
        Some(DEFAULT_REDIRECT_URI.to_string()),
    ];
    candidates
        .into_iter()
        .flatten()
        .next()
        .unwrap_or_else(|| DEFAULT_REDIRECT_URI.to_string())
}

/// hermes `_spotify_api_base_url`.
pub fn resolve_api_base_url(state: Option<&Value>) -> String {
    let candidates: [Option<String>; 3] = [
        crate::config::get_env_value("ULNCLAW_SPOTIFY_API_BASE_URL")
            .map(|v| v.trim().trim_end_matches('/').to_string())
            .filter(|v| !v.is_empty()),
        state
            .and_then(|s| s.get("api_base_url"))
            .and_then(Value::as_str)
            .map(|v| v.trim().trim_end_matches('/').to_string())
            .filter(|v| !v.is_empty()),
        Some(DEFAULT_API_BASE_URL.to_string()),
    ];
    candidates
        .into_iter()
        .flatten()
        .next()
        .unwrap_or_else(|| DEFAULT_API_BASE_URL.to_string())
}

/// hermes `_spotify_accounts_base_url`.
pub fn resolve_accounts_base_url(state: Option<&Value>) -> String {
    let candidates: [Option<String>; 3] = [
        crate::config::get_env_value("ULNCLAW_SPOTIFY_ACCOUNTS_BASE_URL")
            .map(|v| v.trim().trim_end_matches('/').to_string())
            .filter(|v| !v.is_empty()),
        state
            .and_then(|s| s.get("accounts_base_url"))
            .and_then(Value::as_str)
            .map(|v| v.trim().trim_end_matches('/').to_string())
            .filter(|v| !v.is_empty()),
        Some(DEFAULT_ACCOUNTS_BASE_URL.to_string()),
    ];
    candidates
        .into_iter()
        .flatten()
        .next()
        .unwrap_or_else(|| DEFAULT_ACCOUNTS_BASE_URL.to_string())
}

/// hermes `_spotify_scope_string`: whitespace-split, dedupe, preserve order.
pub fn normalize_scope(raw: Option<&str>) -> String {
    let text = raw.unwrap_or(DEFAULT_SCOPE).trim();
    let source = if text.is_empty() { DEFAULT_SCOPE } else { text };
    let mut seen = std::collections::HashSet::new();
    let mut ordered: Vec<&str> = Vec::new();
    for scope in source.split_whitespace() {
        if seen.insert(scope) {
            ordered.push(scope);
        }
    }
    ordered.join(" ")
}

/// hermes `_spotify_code_verifier`: 64 random bytes → URL-safe b64, ≤128.
pub fn code_verifier() -> String {
    let mut bytes: Vec<u8> = Vec::with_capacity(64);
    for _ in 0..4 {
        bytes.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    }
    let mut encoded = URL_SAFE_NO_PAD.encode(&bytes);
    encoded.truncate(128);
    encoded
}

/// hermes `_spotify_code_challenge`: S256, URL-safe b64 without padding.
pub fn code_challenge(verifier: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

/// hermes `_spotify_build_authorize_url`.
pub fn build_authorize_url(
    client_id: &str,
    redirect_uri: &str,
    scope: &str,
    state_nonce: &str,
    challenge: &str,
    accounts_base_url: &str,
) -> String {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("client_id", client_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", scope)
        .append_pair("state", state_nonce)
        .append_pair("code_challenge_method", "S256")
        .append_pair("code_challenge", challenge)
        .finish();
    format!("{accounts_base_url}/authorize?{query}")
}

/// hermes `_spotify_validate_redirect_uri` — http scheme, loopback host,
/// explicit port.
pub fn validate_redirect_uri(
    redirect_uri: &str,
) -> Result<(String, u16, String), SpotifyAuthError> {
    let parsed = url::Url::parse(redirect_uri).map_err(|_| {
        err(
            "Spotify PKCE redirect_uri is not a valid URL.",
            "spotify_redirect_invalid",
            false,
        )
    })?;
    if parsed.scheme() != "http" {
        return Err(err(
            "Spotify PKCE redirect_uri must use http://localhost or http://127.0.0.1.",
            "spotify_redirect_invalid",
            false,
        ));
    }
    let host = parsed.host_str().unwrap_or("").to_string();
    if host != "127.0.0.1" && host != "localhost" {
        return Err(err(
            "Spotify PKCE redirect_uri must point to localhost or 127.0.0.1.",
            "spotify_redirect_invalid",
            false,
        ));
    }
    let Some(port) = parsed.port() else {
        return Err(err(
            "Spotify PKCE redirect_uri must include an explicit localhost port.",
            "spotify_redirect_invalid",
            false,
        ));
    };
    let path = if parsed.path().is_empty() {
        "/".to_string()
    } else {
        parsed.path().to_string()
    };
    Ok((host, port, path))
}

fn coerce_ttl_seconds(value: &Value) -> i64 {
    match value {
        Value::Number(n) => n.as_i64().unwrap_or(0),
        Value::String(s) => s.trim().parse::<i64>().unwrap_or(0),
        _ => 0,
    }
}

/// hermes `_spotify_token_payload_to_state`.
pub fn token_payload_to_state(
    payload: &Value,
    client_id: &str,
    redirect_uri: &str,
    requested_scope: &str,
    accounts_base_url: &str,
    api_base_url: &str,
    previous: Option<&Value>,
) -> Value {
    let now = Utc::now();
    let expires_in = coerce_ttl_seconds(payload.get("expires_in").unwrap_or(&json!(0)));
    let expires_at = now + chrono::Duration::seconds(expires_in);
    let mut state = previous
        .filter(|p| p.is_object())
        .cloned()
        .unwrap_or_else(|| json!({}));
    let refresh_token = payload
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .or_else(|| {
            state
                .get("refresh_token")
                .and_then(Value::as_str)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        })
        .unwrap_or_default();
    let granted_scope = payload
        .get("scope")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or(requested_scope);
    let token_type = {
        let t = payload
            .get("token_type")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if t.is_empty() {
            "Bearer"
        } else {
            t
        }
    };
    let access_token = payload
        .get("access_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default();
    let obtained_at = now.to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
    let expires_at_text = expires_at.to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
    let updates = json!({
        "client_id": client_id,
        "redirect_uri": redirect_uri,
        "accounts_base_url": accounts_base_url,
        "api_base_url": api_base_url,
        "scope": requested_scope,
        "granted_scope": granted_scope,
        "token_type": token_type,
        "access_token": access_token,
        "refresh_token": refresh_token,
        "obtained_at": obtained_at,
        "expires_at": expires_at_text,
        "expires_in": expires_in,
        "auth_type": "oauth_pkce",
    });
    if let (Some(obj), Some(updates)) = (state.as_object_mut(), updates.as_object()) {
        for (key, value) in updates {
            obj.insert(key.clone(), value.clone());
        }
    }
    state
}

async fn form_post(
    url: &str,
    form: &[(&str, &str)],
    timeout_secs: u64,
) -> Result<(u16, Value, String), SpotifyAuthError> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .build()
        .map_err(|e| err(format!("http client: {e}"), "spotify_http_client", false))?;
    let response = client
        .post(url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .form(
            &form
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<Vec<_>>(),
        )
        .send()
        .await
        .map_err(|e| {
            err(
                format!("request failed: {e}"),
                "spotify_token_exchange_failed",
                false,
            )
        })?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    let parsed: Value = serde_json::from_str(&text).unwrap_or(json!({}));
    Ok((status, parsed, text))
}

/// hermes `_spotify_exchange_code_for_tokens`.
pub async fn exchange_code_for_tokens(
    client_id: &str,
    code: &str,
    redirect_uri: &str,
    code_verifier: &str,
    accounts_base_url: &str,
    timeout_secs: u64,
) -> Result<Value, SpotifyAuthError> {
    let (status, payload, text) = form_post(
        &format!("{accounts_base_url}/api/token"),
        &[
            ("client_id", client_id),
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", code_verifier),
        ],
        timeout_secs,
    )
    .await?;
    if status >= 400 {
        let detail = text.trim();
        let suffix = if detail.is_empty() {
            String::new()
        } else {
            format!(" Response: {detail}")
        };
        return Err(err(
            format!("Spotify token exchange failed.{suffix}"),
            "spotify_token_exchange_failed",
            false,
        ));
    }
    let token = payload
        .get("access_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default();
    if token.is_empty() {
        return Err(err(
            "Spotify token response did not include an access_token.",
            "spotify_token_exchange_invalid",
            false,
        ));
    }
    Ok(payload)
}

/// hermes `_refresh_spotify_oauth_state`.
pub async fn refresh_state(state: &Value, timeout_secs: u64) -> Result<Value, SpotifyAuthError> {
    let refresh_token = state
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default()
        .to_string();
    if refresh_token.is_empty() {
        return Err(err(
            "Spotify refresh token missing. Run `ulnclaw spotify-auth login` again.",
            "spotify_refresh_token_missing",
            true,
        ));
    }
    let client_id = resolve_client_id(None, Some(state))?;
    let accounts_base_url = resolve_accounts_base_url(Some(state));
    let (status, payload, text) = form_post(
        &format!("{accounts_base_url}/api/token"),
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", &refresh_token),
            ("client_id", &client_id),
        ],
        timeout_secs,
    )
    .await
    .map_err(|mut e| {
        e.relogin_required = true;
        e.code = "spotify_refresh_failed";
        e
    })?;
    if status >= 400 {
        let detail = text.trim();
        let suffix = if detail.is_empty() {
            String::new()
        } else {
            format!(" Response: {detail}")
        };
        return Err(err(
            format!(
                "Spotify token refresh failed. Run `ulnclaw spotify-auth login` again.{suffix}"
            ),
            "spotify_refresh_failed",
            true,
        ));
    }
    let token = payload
        .get("access_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default();
    if token.is_empty() {
        return Err(err(
            "Spotify refresh response did not include an access_token.",
            "spotify_refresh_invalid",
            true,
        ));
    }
    let scope = state
        .get("scope")
        .and_then(Value::as_str)
        .filter(|v| !v.trim().is_empty())
        .unwrap_or(DEFAULT_SCOPE);
    Ok(token_payload_to_state(
        &payload,
        &client_id,
        &resolve_redirect_uri(None, Some(state)),
        scope,
        &accounts_base_url,
        &resolve_api_base_url(Some(state)),
        Some(state),
    ))
}

fn parse_expires_at(state: &Value) -> Option<DateTime<Utc>> {
    let text = state.get("expires_at")?.as_str()?.trim().to_string();
    if text.is_empty() {
        return None;
    }
    DateTime::parse_from_rfc3339(&text)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn is_expiring(state: &Value, skew_seconds: i64) -> bool {
    let Some(expires) = parse_expires_at(state) else {
        return true;
    };
    (expires - Utc::now()).num_seconds() <= skew_seconds.max(0)
}

/// Runtime credentials for API calls (hermes
/// `resolve_spotify_runtime_credentials`).
#[derive(Debug)]
pub struct SpotifyRuntime {
    pub access_token: String,
    pub base_url: String,
    pub client_id: String,
}

pub async fn resolve_runtime_credentials(
    force_refresh: bool,
    refresh_if_expiring: bool,
) -> Result<SpotifyRuntime, SpotifyAuthError> {
    let state = match provider_state() {
        Some(state) => state,
        None => {
            return Err(err(
                "Spotify is not authenticated. Run `ulnclaw spotify-auth login` first.",
                "spotify_auth_missing",
                true,
            ))
        }
    };
    let mut state = state;
    let should_refresh =
        force_refresh || (refresh_if_expiring && is_expiring(&state, REFRESH_SKEW_SECONDS));
    if should_refresh {
        match refresh_state(&state, 20).await {
            Ok(refreshed) => {
                state = refreshed;
                store_provider_state(&state)?;
            }
            Err(e) => {
                if e.relogin_required {
                    // Terminal refresh failure — quarantine dead tokens so
                    // later calls fail fast (hermes parity).
                    if let Some(obj) = state.as_object_mut() {
                        for key in [
                            "access_token",
                            "refresh_token",
                            "expires_at",
                            "expires_in",
                            "obtained_at",
                        ] {
                            obj.remove(key);
                        }
                        obj.insert(
                            "last_auth_error".to_string(),
                            json!({
                                "provider": "spotify",
                                "code": e.code,
                                "message": e.message,
                                "reason": "runtime_refresh_failure",
                                "relogin_required": true,
                                "at": Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
                            }),
                        );
                    }
                    let _ = store_provider_state(&state);
                }
                return Err(e);
            }
        }
    }
    let access_token = state
        .get("access_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default()
        .to_string();
    if access_token.is_empty() {
        return Err(err(
            "Spotify access token missing. Run `ulnclaw spotify-auth login` again.",
            "spotify_access_token_missing",
            true,
        ));
    }
    Ok(SpotifyRuntime {
        access_token,
        base_url: resolve_api_base_url(Some(&state)),
        client_id: resolve_client_id(None, Some(&state)).unwrap_or_default(),
    })
}

/// hermes `get_spotify_auth_status`.
pub fn auth_status() -> Value {
    let Some(state) = provider_state() else {
        return json!({"logged_in": false});
    };
    let refresh_token = state
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default()
        .to_string();
    let expires_at = state.get("expires_at").cloned();
    let logged_in = !refresh_token.is_empty() || !is_expiring(&state, 0);
    json!({
        "logged_in": logged_in,
        "auth_type": state.get("auth_type").and_then(Value::as_str).unwrap_or("oauth_pkce"),
        "client_id": state.get("client_id").cloned(),
        "redirect_uri": state.get("redirect_uri").cloned(),
        "scope": state.get("granted_scope").or_else(|| state.get("scope")).cloned(),
        "expires_at": expires_at,
        "api_base_url": state.get("api_base_url").cloned(),
        "has_refresh_token": !refresh_token.is_empty(),
    })
}

/// Remove `providers.spotify` entirely (logout).
pub fn logout() -> Result<(), SpotifyAuthError> {
    let _guard = auth_store_lock()
        .lock()
        .map_err(|_| err("auth store lock poisoned", "spotify_store_locked", false))?;
    let mut store = load_auth_store();
    if let Some(providers) = store.get_mut("providers").and_then(Value::as_object_mut) {
        providers.remove("spotify");
    }
    save_auth_store(&store)?;
    Ok(())
}

/// Result of a finished loopback callback.
pub struct CallbackResult {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

/// Minimal loopback HTTP listener for the OAuth redirect (hermes
/// `_spotify_wait_for_callback`). Returns once `code` or `error` arrives
/// or the deadline passes.
pub async fn wait_for_callback(
    redirect_uri: &str,
    timeout_secs: u64,
) -> Result<CallbackResult, SpotifyAuthError> {
    let (host, port, expected_path) = validate_redirect_uri(redirect_uri)?;
    let listener = tokio::net::TcpListener::bind((host.as_str(), port))
        .await
        .map_err(|e| {
            err(
                format!("Could not bind Spotify callback server on {host}:{port}: {e}"),
                "spotify_callback_bind_failed",
                false,
            )
        })?;
    let deadline = tokio::time::Instant::now()
        + std::time::Duration::from_secs(
            timeout_secs.max(5).min(DEFAULT_CALLBACK_TIMEOUT_SECS * 10),
        );
    loop {
        let accept = tokio::time::timeout_at(deadline, listener.accept()).await;
        let (mut stream, _) = match accept {
            Ok(Ok(conn)) => conn,
            Ok(Err(e)) => {
                return Err(err(
                    format!("callback server error: {e}"),
                    "spotify_callback_bind_failed",
                    false,
                ))
            }
            Err(_) => {
                return Err(err(
                    "Spotify authorization timed out waiting for the local callback.",
                    "spotify_callback_timeout",
                    false,
                ))
            }
        };
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut buf = vec![0u8; 8192];
        let read =
            tokio::time::timeout(std::time::Duration::from_secs(5), stream.read(&mut buf)).await;
        let n = match read {
            Ok(Ok(n)) => n,
            _ => continue,
        };
        let request = String::from_utf8_lossy(&buf[..n]);
        let first_line = request.lines().next().unwrap_or("");
        let mut parts = first_line.split_whitespace();
        let method = parts.next().unwrap_or("");
        let target = parts.next().unwrap_or("");
        if method != "GET" {
            let response = "HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\n\r\n";
            let _ = stream.write_all(response.as_bytes()).await;
            continue;
        }
        let (path, query) = match target.split_once('?') {
            Some((p, q)) => (p.to_string(), q.to_string()),
            None => (target.to_string(), String::new()),
        };
        if path != expected_path {
            let response = "HTTP/1.1 404 Not Found\r\nContent-Length: 10\r\n\r\nNot found.";
            let _ = stream.write_all(response.as_bytes()).await;
            continue;
        }
        let params: std::collections::HashMap<String, String> =
            url::form_urlencoded::parse(query.as_bytes())
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect();
        let result = CallbackResult {
            code: params.get("code").cloned(),
            state: params.get("state").cloned(),
            error: params.get("error").cloned(),
            error_description: params.get("error_description").cloned(),
        };
        let body = if result.error.is_some() {
            "<html><body><h1>Spotify authorization failed.</h1>You can close this tab.</body></html>"
        } else {
            "<html><body><h1>Spotify authorization received.</h1>You can close this tab.</body></html>"
        };
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(response.as_bytes()).await;
        return Ok(result);
    }
}

/// Full interactive PKCE login (hermes `login_spotify_command`).
pub async fn login(
    explicit_client_id: Option<&str>,
    explicit_redirect_uri: Option<&str>,
    explicit_scope: Option<&str>,
    open_browser: bool,
    timeout_secs: Option<u64>,
) -> Result<String, SpotifyAuthError> {
    let existing = provider_state();
    let client_id = resolve_client_id(explicit_client_id, existing.as_ref())?;
    let redirect_uri = resolve_redirect_uri(explicit_redirect_uri, existing.as_ref());
    let scope = normalize_scope(explicit_scope.filter(|s| !s.trim().is_empty()).or_else(|| {
        existing
            .as_ref()
            .and_then(|s| s.get("scope").and_then(Value::as_str))
    }));
    let accounts_base_url = resolve_accounts_base_url(existing.as_ref());
    let api_base_url = resolve_api_base_url(existing.as_ref());

    let verifier = code_verifier();
    let challenge = code_challenge(&verifier);
    let state_nonce = uuid::Uuid::new_v4().simple().to_string();
    let authorize_url = build_authorize_url(
        &client_id,
        &redirect_uri,
        &scope,
        &state_nonce,
        &challenge,
        &accounts_base_url,
    );

    println!("Starting Spotify PKCE login...");
    println!("Client ID: {client_id}");
    println!("Redirect URI: {redirect_uri}");
    println!("Make sure this redirect URI is allow-listed in your Spotify app settings.");
    println!();
    println!("Open this URL to authorize ulnclaw:");
    println!("{authorize_url}");
    println!();

    if open_browser {
        open_in_browser(&authorize_url);
    }

    let callback = wait_for_callback(
        &redirect_uri,
        timeout_secs.unwrap_or(DEFAULT_CALLBACK_TIMEOUT_SECS),
    )
    .await?;
    if let Some(error) = &callback.error {
        let detail = callback
            .error_description
            .clone()
            .unwrap_or_else(|| error.clone());
        return Err(err(
            format!("Spotify authorization failed: {detail}"),
            "spotify_authorization_failed",
            false,
        ));
    }
    if callback.state.as_deref() != Some(state_nonce.as_str()) {
        return Err(err(
            "Spotify authorization failed: state mismatch.",
            "spotify_state_mismatch",
            false,
        ));
    }
    let payload = exchange_code_for_tokens(
        &client_id,
        callback.code.as_deref().unwrap_or(""),
        &redirect_uri,
        &verifier,
        &accounts_base_url,
        20,
    )
    .await?;
    let state = token_payload_to_state(
        &payload,
        &client_id,
        &redirect_uri,
        &scope,
        &accounts_base_url,
        &api_base_url,
        existing.as_ref(),
    );
    let saved_to = store_provider_state(&state)?;
    Ok(format!(
        "Spotify login successful!\n  Auth state: {}\n  Provider state saved under providers.spotify",
        saved_to.display()
    ))
}

/// Best-effort browser launch; the URL is always printed as fallback.
fn open_in_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let cmd = ("open", url.to_string());
    #[cfg(all(unix, not(target_os = "macos")))]
    let cmd = ("xdg-open", url.to_string());
    #[cfg(windows)]
    let cmd = ("cmd", format!("/c start {url}"));
    match std::process::Command::new(cmd.0).arg(cmd.1).spawn() {
        Ok(_) => println!("Browser opened for Spotify authorization."),
        Err(_) => println!("Could not open the browser automatically; use the URL above."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_home<F: FnOnce()>(tmp: &tempfile::TempDir, f: F) {
        let _guard = crate::models_dev::test_env_lock();
        let prev = std::env::var("ULNCLAW_HOME").ok();
        std::env::set_var("ULNCLAW_HOME", tmp.path());
        f();
        match prev {
            Some(v) => std::env::set_var("ULNCLAW_HOME", v),
            None => std::env::remove_var("ULNCLAW_HOME"),
        }
    }

    #[test]
    fn code_challenge_matches_rfc7636_vector() {
        // RFC 7636 Appendix B test vector.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            code_challenge(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn code_verifier_is_url_safe_and_bounded() {
        let verifier = code_verifier();
        assert!(
            verifier.len() <= 128 && verifier.len() >= 43,
            "len {}",
            verifier.len()
        );
        assert!(verifier
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn build_authorize_url_contains_pkce_params() {
        let url = build_authorize_url(
            "cid",
            "http://127.0.0.1:9/callback",
            "a b",
            "nonce",
            "chal",
            "https://accounts.spotify.com",
        );
        assert!(url.starts_with("https://accounts.spotify.com/authorize?"));
        assert!(url.contains("client_id=cid"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("code_challenge=chal"));
        assert!(url.contains("scope=a+b") || url.contains("scope=a%20b"));
        assert!(url.contains("state=nonce"));
    }

    #[test]
    fn validate_redirect_uri_rules() {
        let (host, port, path) =
            validate_redirect_uri("http://127.0.0.1:43827/spotify/callback").unwrap();
        assert_eq!(
            (host.as_str(), port, path.as_str()),
            ("127.0.0.1", 43827, "/spotify/callback")
        );
        assert!(validate_redirect_uri("https://127.0.0.1:9/cb").is_err());
        assert!(validate_redirect_uri("http://example.com:9/cb").is_err());
        assert!(validate_redirect_uri("http://127.0.0.1/cb").is_err());
    }

    #[test]
    fn normalize_scope_dedupes_preserving_order() {
        assert_eq!(normalize_scope(Some("a b a c")), "a b c");
        assert_eq!(normalize_scope(Some("")), DEFAULT_SCOPE);
        assert_eq!(normalize_scope(None), DEFAULT_SCOPE);
    }

    #[test]
    fn token_payload_to_state_maps_fields_and_keeps_refresh() {
        let payload = json!({
            "access_token": " at1 ",
            "expires_in": 3600,
            "scope": "user-read-playback-state",
            "token_type": "Bearer"
        });
        let previous = json!({"refresh_token": "kept-rt", "extra": 1});
        let state = token_payload_to_state(
            &payload,
            "cid",
            "http://127.0.0.1:9/cb",
            "scope-req",
            "https://accounts.spotify.com",
            "https://api.spotify.com/v1",
            Some(&previous),
        );
        assert_eq!(state["access_token"], "at1");
        assert_eq!(state["refresh_token"], "kept-rt");
        assert_eq!(state["granted_scope"], "user-read-playback-state");
        assert_eq!(state["scope"], "scope-req");
        assert_eq!(state["auth_type"], "oauth_pkce");
        assert_eq!(state["extra"], 1);
        assert_eq!(state["expires_in"], 3600);
        let expires_at = state["expires_at"].as_str().unwrap();
        let parsed = DateTime::parse_from_rfc3339(expires_at).unwrap();
        let delta = (parsed.with_timezone(&Utc) - Utc::now()).num_seconds();
        assert!((3500..=3600).contains(&delta), "delta {delta}");
    }

    #[test]
    fn client_id_resolution_order() {
        let _guard = crate::models_dev::test_env_lock();
        let prev_env = std::env::var("SPOTIFY_CLIENT_ID").ok();
        let prev_uln = std::env::var("ULNCLAW_SPOTIFY_CLIENT_ID").ok();
        std::env::remove_var("SPOTIFY_CLIENT_ID");
        std::env::remove_var("ULNCLAW_SPOTIFY_CLIENT_ID");

        let state = json!({"client_id": "state-cid"});
        // No env, no explicit → stored state wins.
        assert_eq!(resolve_client_id(None, Some(&state)).unwrap(), "state-cid");
        // Env beats state.
        std::env::set_var("SPOTIFY_CLIENT_ID", "env-cid");
        assert_eq!(resolve_client_id(None, Some(&state)).unwrap(), "env-cid");
        // Branded env beats plain env.
        std::env::set_var("ULNCLAW_SPOTIFY_CLIENT_ID", "branded-cid");
        assert_eq!(
            resolve_client_id(None, Some(&state)).unwrap(),
            "branded-cid"
        );
        // Explicit beats everything.
        assert_eq!(
            resolve_client_id(Some("explicit-cid"), Some(&state)).unwrap(),
            "explicit-cid"
        );
        // Nothing anywhere → error with guidance.
        std::env::remove_var("SPOTIFY_CLIENT_ID");
        std::env::remove_var("ULNCLAW_SPOTIFY_CLIENT_ID");
        let e = resolve_client_id(None, None).unwrap_err();
        assert_eq!(e.code, "spotify_client_id_missing");

        match prev_env {
            Some(v) => std::env::set_var("SPOTIFY_CLIENT_ID", v),
            None => std::env::remove_var("SPOTIFY_CLIENT_ID"),
        }
        match prev_uln {
            Some(v) => std::env::set_var("ULNCLAW_SPOTIFY_CLIENT_ID", v),
            None => std::env::remove_var("ULNCLAW_SPOTIFY_CLIENT_ID"),
        }
    }

    #[test]
    fn runtime_credentials_missing_state_fails_fast() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(&tmp, || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let e = rt
                .block_on(resolve_runtime_credentials(false, true))
                .unwrap_err();
            assert_eq!(e.code, "spotify_auth_missing");
            assert!(e.relogin_required);
            assert_eq!(auth_status()["logged_in"], false);
        });
    }

    #[test]
    fn auth_status_reports_stored_tokens() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(&tmp, || {
            let future = Utc::now() + chrono::Duration::seconds(3600);
            let state = json!({
                "access_token": "at",
                "refresh_token": "rt",
                "expires_at": future.to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
                "client_id": "cid",
                "auth_type": "oauth_pkce",
                "granted_scope": "user-read-playback-state",
            });
            store_provider_state(&state).unwrap();
            let status = auth_status();
            assert_eq!(status["logged_in"], true);
            assert_eq!(status["has_refresh_token"], true);
            assert_eq!(status["client_id"], "cid");
            // logout clears the provider block.
            logout().unwrap();
            assert_eq!(auth_status()["logged_in"], false);
            assert!(provider_state().is_none());
        });
    }

    #[test]
    fn expiring_detection_uses_skew() {
        let soon = Utc::now() + chrono::Duration::seconds(60);
        let state =
            json!({"expires_at": soon.to_rfc3339_opts(chrono::SecondsFormat::Micros, true)});
        assert!(is_expiring(&state, REFRESH_SKEW_SECONDS));
        assert!(!is_expiring(&state, 0));
        assert!(is_expiring(&json!({}), 0));
    }

    #[test]
    fn store_round_trips_through_auth_json() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(&tmp, || {
            let state = json!({"access_token": "at", "refresh_token": "rt"});
            let path = store_provider_state(&state).unwrap();
            assert!(path.ends_with("auth.json"));
            let parsed: Value =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            assert_eq!(parsed["providers"]["spotify"]["access_token"], "at");
            assert_eq!(provider_state().unwrap()["refresh_token"], "rt");
        });
    }
}

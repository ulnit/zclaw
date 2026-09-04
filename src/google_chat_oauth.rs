//! Google Chat per-user OAuth for native attachment delivery — port of
//! hermes `plugins/platforms/google_chat/oauth.py`.
//!
//! Google Chat's `media.upload` endpoint hard-rejects service-account
//! authentication ("This method doesn't support app authentication with
//! a service account"). For the bot to deliver native file attachments,
//! each user grants the bot the `chat.messages.create` scope ONCE via
//! the in-chat `/setup-files` flow; the stored refresh token lets the
//! bot call `media.upload` + `messages.create` AS that user.
//!
//! Flow (RFC 8252-style installed-app + PKCE, hermes semantics):
//! localhost:1 redirect that is expected to FAIL — the user copies the
//! `code=` value (or the whole failed URL) from the browser URL bar
//! back into chat.
//!
//! Storage layout under `<home>` (hermes paths, 0600 files / 0700 dirs):
//! - client secret: `google_chat_user_client_secret.json`
//! - per-user tokens: `google_chat_user_tokens/<sanitized-email>.json`
//! - legacy single-user token: `google_chat_user_token.json`
//! - per-user pending PKCE state: `google_chat_user_oauth_pending/<email>.json`
//! - legacy pending state: `google_chat_user_oauth_pending.json`

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Minimum scope for native Chat attachment delivery (covers both
/// `media.upload` and the referencing `messages.create`; hermes
/// `SCOPES` — least privilege, no drive.file).
pub const USER_AUTH_SCOPE: &str = "https://www.googleapis.com/auth/chat.messages.create";
/// Out-of-band redirect: Google deprecated the OOB flow, so this is a
/// localhost redirect that is expected to FAIL (hermes `_REDIRECT_URI`).
pub const REDIRECT_URI: &str = "http://localhost:1";
const AUTH_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const DEFAULT_TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";
const REVOKE_ENDPOINT: &str = "https://oauth2.googleapis.com/revoke";

// ---------------------------------------------------------------------------
// Paths + email sanitization (hermes `_token_path` / `_sanitize_email`)
// ---------------------------------------------------------------------------

/// Filesystem-safe key: lowercase, keep `[a-z0-9._@-]`, replace the
/// rest with `_` (hermes `_EMAIL_FS_RE`).
pub fn sanitize_email(email: &str) -> String {
    let cleaned: String = email
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '@' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "_unknown_".to_string()
    } else {
        cleaned
    }
}

pub fn client_secret_path(home: &Path) -> PathBuf {
    home.join("google_chat_user_client_secret.json")
}

fn user_tokens_dir(home: &Path) -> PathBuf {
    home.join("google_chat_user_tokens")
}

fn user_pending_dir(home: &Path) -> PathBuf {
    home.join("google_chat_user_oauth_pending")
}

/// Token path for `email`, or the legacy single-user path (hermes
/// `_token_path`).
pub fn token_path(home: &Path, email: Option<&str>) -> PathBuf {
    match email {
        Some(email) if !email.is_empty() => {
            user_tokens_dir(home).join(format!("{}.json", sanitize_email(email)))
        }
        _ => home.join("google_chat_user_token.json"),
    }
}

/// Pending PKCE state path (hermes `_pending_auth_path`).
pub fn pending_path(home: &Path, email: Option<&str>) -> PathBuf {
    match email {
        Some(email) if !email.is_empty() => {
            user_pending_dir(home).join(format!("{}.json", sanitize_email(email)))
        }
        _ => home.join("google_chat_user_oauth_pending.json"),
    }
}

/// Emails with stored per-user tokens (hermes `list_authorized_emails`;
/// legacy token excluded — owner unknown; display-only).
pub fn list_authorized_emails(home: &Path) -> Vec<String> {
    let dir = user_tokens_dir(home);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().map(|x| x == "json").unwrap_or(false))
        .filter_map(|p| p.file_stem().map(|s| s.to_string_lossy().to_string()))
        .collect();
    out.sort();
    out
}

// ---------------------------------------------------------------------------
// Private JSON writes (hermes `_write_private_json`: 0600 + atomic)
// ---------------------------------------------------------------------------

fn write_private_json(path: &Path, data: &Value) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).ok();
        }
    }
    let pid = std::process::id();
    let tmp = path.with_extension(format!("tmp.{pid}"));
    let body = serde_json::to_string_pretty(data).unwrap_or_default();
    std::fs::write(&tmp, body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).ok();
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Client secret (hermes `store_client_secret` / load)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ClientSecret {
    pub client_id: String,
    pub client_secret: String,
    pub token_uri: String,
    pub raw: Value,
}

/// Parse a Google OAuth client secret file (`installed` or `web` block;
/// hermes validation).
pub fn parse_client_secret(json_str: &str) -> Result<ClientSecret, String> {
    let data: Value = serde_json::from_str(json_str).map_err(|e| format!("invalid JSON: {e}"))?;
    let block = data
        .get("installed")
        .or_else(|| data.get("web"))
        .ok_or_else(|| {
            "not a Google OAuth client secret file (missing 'installed' or 'web' key)".to_string()
        })?;
    let client_id = block
        .get("client_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "client secret is missing client_id".to_string())?
        .to_string();
    let client_secret = block
        .get("client_secret")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let token_uri = block
        .get("token_uri")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_TOKEN_ENDPOINT)
        .to_string();
    Ok(ClientSecret {
        client_id,
        client_secret,
        token_uri,
        raw: data,
    })
}

/// Validate + copy the user's client_secret.json into `<home>` (hermes
/// `store_client_secret`). Returns the destination path.
pub fn store_client_secret(home: &Path, src: &Path) -> Result<PathBuf, String> {
    let body = std::fs::read_to_string(src).map_err(|e| format!("file not found: {e}"))?;
    let secret = parse_client_secret(&body)?;
    let target = client_secret_path(home);
    write_private_json(&target, &secret.raw).map_err(|e| format!("cannot write secret: {e}"))?;
    Ok(target)
}

pub fn load_client_secret(home: &Path) -> Option<ClientSecret> {
    let body = std::fs::read_to_string(client_secret_path(home)).ok()?;
    parse_client_secret(&body).ok()
}

// ---------------------------------------------------------------------------
// PKCE + auth URL (hermes `get_auth_url` via google_auth_oauthlib Flow)
// ---------------------------------------------------------------------------

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    crate::feishu::fill_random_bytes(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

fn base64_url_nopad(data: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

/// PKCE triple: random state, verifier (64 random bytes, base64url),
/// S256 challenge.
pub fn generate_pkce() -> (String, String, String) {
    let state = random_hex(16);
    let mut verifier_bytes = vec![0u8; 48];
    crate::feishu::fill_random_bytes(&mut verifier_bytes);
    let verifier = base64_url_nopad(&verifier_bytes);
    let challenge = base64_url_nopad(&Sha256::digest(verifier.as_bytes()));
    (state, verifier, challenge)
}

/// Build the OAuth authorization URL (hermes `authorization_url` with
/// `access_type=offline`, `prompt=consent`).
pub fn build_auth_url(client_id: &str, state: &str, code_challenge: &str) -> String {
    let pairs = [
        ("client_id", client_id.to_string()),
        ("redirect_uri", REDIRECT_URI.to_string()),
        ("response_type", "code".to_string()),
        ("scope", USER_AUTH_SCOPE.to_string()),
        ("access_type", "offline".to_string()),
        ("prompt", "consent".to_string()),
        ("state", state.to_string()),
        ("code_challenge", code_challenge.to_string()),
        ("code_challenge_method", "S256".to_string()),
    ];
    let query = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish();
    format!("{AUTH_ENDPOINT}?{query}")
}

/// Start the flow: persist PKCE state and return the auth URL (hermes
/// `get_auth_url`). Errors when no client secret is stored.
pub fn start_auth(home: &Path, email: Option<&str>) -> Result<String, String> {
    let secret = load_client_secret(home)
        .ok_or_else(|| "no client secret stored — run the client-secret step first".to_string())?;
    let (state, verifier, challenge) = generate_pkce();
    let pending = json!({
        "state": state,
        "code_verifier": verifier,
        "redirect_uri": REDIRECT_URI,
        "email": email.unwrap_or(""),
    });
    write_private_json(&pending_path(home, email), &pending)
        .map_err(|e| format!("cannot persist pending auth: {e}"))?;
    Ok(build_auth_url(&secret.client_id, &state, &challenge))
}

// ---------------------------------------------------------------------------
// Code exchange (hermes `exchange_auth_code`)
// ---------------------------------------------------------------------------

/// Accept a raw auth code OR the full failed-redirect URL (hermes
/// `_extract_code_and_state`). Returns `(code, state?, granted_scopes?)`.
pub fn extract_code_and_state(
    code_or_url: &str,
) -> Result<(String, Option<String>, Option<String>), String> {
    if !code_or_url.starts_with("http") {
        return Ok((code_or_url.trim().to_string(), None, None));
    }
    let parsed = url::Url::parse(code_or_url).map_err(|e| format!("invalid URL: {e}"))?;
    let mut code = None;
    let mut state = None;
    let mut scope = None;
    for (key, value) in parsed.query_pairs() {
        match key.as_ref() {
            "code" => code = Some(value.to_string()),
            "state" => state = Some(value.to_string()),
            "scope" => scope = Some(value.to_string()),
            _ => {}
        }
    }
    match code {
        Some(code) => Ok((code, state, scope)),
        None => Err("no 'code' parameter found in URL".to_string()),
    }
}

fn load_pending(home: &Path, email: Option<&str>) -> Result<Value, String> {
    let path = pending_path(home, email);
    let body = std::fs::read_to_string(&path)
        .map_err(|_| "no pending OAuth session found — run the auth-url step first".to_string())?;
    let data: Value =
        serde_json::from_str(&body).map_err(|e| format!("cannot read pending session: {e}"))?;
    if data
        .get("state")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .is_empty()
        || data
            .get("code_verifier")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .is_empty()
    {
        return Err("pending OAuth session is missing PKCE data".to_string());
    }
    Ok(data)
}

/// Persisted per-user token (google-auth `authorized_user` shape so
/// files stay compatible with hermes installs).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UserToken {
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    /// Unix seconds at which the access token expires (0 = unknown).
    #[serde(default)]
    pub expires_at: i64,
    #[serde(default)]
    pub scope: String,
    #[serde(default)]
    pub token_uri: String,
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub client_secret: String,
}

impl UserToken {
    pub fn expired(&self) -> bool {
        self.expires_at > 0
            && std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64 >= self.expires_at - 60)
                .unwrap_or(false)
    }

    pub fn usable(&self) -> bool {
        !self.access_token.is_empty() && (!self.expired() || !self.refresh_token.is_empty())
    }
}

fn persist_user_token(home: &Path, email: Option<&str>, token: &UserToken) {
    let payload = json!({
        "type": "authorized_user",
        "client_id": token.client_id,
        "client_secret": token.client_secret,
        "refresh_token": token.refresh_token,
        "access_token": token.access_token,
        "expiry": token.expires_at,
        "scope": token.scope,
        "token_uri": token.token_uri,
    });
    let _ = write_private_json(&token_path(home, email), &payload);
}

fn read_user_token(home: &Path, email: Option<&str>) -> Option<UserToken> {
    let body = std::fs::read_to_string(token_path(home, email)).ok()?;
    let data: Value = serde_json::from_str(&body).ok()?;
    let expires_at = data
        .get("expiry")
        .and_then(|v| v.as_i64())
        .unwrap_or_default();
    Some(UserToken {
        access_token: data
            .get("access_token")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        refresh_token: data
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        expires_at,
        scope: data
            .get("scope")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        token_uri: data
            .get("token_uri")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        client_id: data
            .get("client_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        client_secret: data
            .get("client_secret")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    })
}

/// Exchange an auth code (or pasted redirect URL) for a refresh token
/// and persist it (hermes `exchange_auth_code`). Returns the granted
/// scope on success.
pub async fn exchange_code(
    home: &Path,
    client: &reqwest::Client,
    code_or_url: &str,
    email: Option<&str>,
) -> Result<String, String> {
    let secret = load_client_secret(home)
        .ok_or_else(|| "no client secret stored — run the client-secret step first".to_string())?;
    let pending = load_pending(home, email)?;
    let pending_state = pending
        .get("state")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let verifier = pending
        .get("code_verifier")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let (code, returned_state, granted_scope) = extract_code_and_state(code_or_url)?;
    if let Some(returned_state) = &returned_state {
        if *returned_state != pending_state {
            return Err("OAuth state mismatch — start a fresh session and retry".to_string());
        }
    }
    let form = [
        ("grant_type", "authorization_code".to_string()),
        ("code", code),
        ("redirect_uri", REDIRECT_URI.to_string()),
        ("client_id", secret.client_id.clone()),
        ("client_secret", secret.client_secret.clone()),
        ("code_verifier", verifier),
    ];
    let resp = client
        .post(&secret.token_uri)
        .form(&form)
        .send()
        .await
        .map_err(|e| format!("token exchange failed: {e}"))?;
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(json!({}));
    if !status.is_success() || body.get("access_token").is_none() {
        let detail = body
            .get("error_description")
            .or_else(|| body.get("error"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error");
        return Err(format!(
            "token exchange failed ({status}): {detail} — the code may have expired"
        ));
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let scope = granted_scope
        .filter(|s| !s.is_empty())
        .or_else(|| {
            body.get("scope")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| USER_AUTH_SCOPE.to_string());
    let token = UserToken {
        access_token: body
            .get("access_token")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        refresh_token: body
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        expires_at: now
            + body
                .get("expires_in")
                .and_then(|v| v.as_i64())
                .unwrap_or(3600),
        scope,
        token_uri: secret.token_uri.clone(),
        client_id: secret.client_id.clone(),
        client_secret: secret.client_secret.clone(),
    };
    persist_user_token(home, email, &token);
    let _ = std::fs::remove_file(pending_path(home, email));
    Ok(token.scope.clone())
}

// ---------------------------------------------------------------------------
// Load + refresh (hermes `load_user_credentials` / `refresh_or_none`)
// ---------------------------------------------------------------------------

/// Refresh an expired token; persists the refreshed payload (hermes
/// `refresh_or_none`). Returns `None` when unusable.
pub async fn refresh_token(
    home: &Path,
    client: &reqwest::Client,
    token: &UserToken,
    email: Option<&str>,
) -> Option<UserToken> {
    if token.refresh_token.is_empty() {
        return None;
    }
    let token_uri = if token.token_uri.is_empty() {
        DEFAULT_TOKEN_ENDPOINT.to_string()
    } else {
        token.token_uri.clone()
    };
    let form = [
        ("grant_type", "refresh_token".to_string()),
        ("refresh_token", token.refresh_token.clone()),
        ("client_id", token.client_id.clone()),
        ("client_secret", token.client_secret.clone()),
    ];
    let resp = client.post(&token_uri).form(&form).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body: Value = resp.json().await.ok()?;
    let access = body.get("access_token").and_then(|v| v.as_str())?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let mut refreshed = token.clone();
    refreshed.access_token = access.to_string();
    refreshed.expires_at = now
        + body
            .get("expires_in")
            .and_then(|v| v.as_i64())
            .unwrap_or(3600);
    if let Some(scope) = body.get("scope").and_then(|v| v.as_str()) {
        if !scope.is_empty() {
            refreshed.scope = scope.to_string();
        }
    }
    persist_user_token(home, email, &refreshed);
    Some(refreshed)
}

/// Load + validate persisted user OAuth credentials (hermes
/// `load_user_credentials`): `None` means "user has not completed
/// /setup-files yet" (or the token is revoked/corrupt).
pub async fn load_user_credentials(
    home: &Path,
    client: &reqwest::Client,
    email: Option<&str>,
) -> Option<UserToken> {
    if !token_path(home, email).exists() {
        return None;
    }
    let token = read_user_token(home, email)?;
    if !token.expired() && !token.access_token.is_empty() {
        return Some(token);
    }
    if !token.refresh_token.is_empty() {
        return refresh_token(home, client, &token, email).await;
    }
    None
}

/// Revoke + delete the stored token (hermes `revoke`). Returns a
/// human-readable outcome line.
pub async fn revoke(home: &Path, client: &reqwest::Client, email: Option<&str>) -> String {
    let path = token_path(home, email);
    let Some(token) = read_user_token(home, email) else {
        return format!("no stored token at {}", path.display());
    };
    let mut outcome = String::new();
    if !token.access_token.is_empty() || !token.refresh_token.is_empty() {
        let revoke_target = if token.refresh_token.is_empty() {
            token.access_token.clone()
        } else {
            token.refresh_token.clone()
        };
        match client
            .post(REVOKE_ENDPOINT)
            .form(&[("token", revoke_target)])
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                outcome.push_str("revoked remote grant; ");
            }
            Ok(resp) => {
                outcome.push_str(&format!(
                    "remote revoke HTTP {} (deleting local token anyway); ",
                    resp.status()
                ));
            }
            Err(e) => {
                outcome.push_str(&format!(
                    "remote revoke failed: {e} (deleting local token anyway); "
                ));
            }
        }
    }
    match std::fs::remove_file(&path) {
        Ok(()) => outcome.push_str(&format!("deleted {}", path.display())),
        Err(_) => outcome.push_str("local token already absent"),
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_sanitization() {
        assert_eq!(
            sanitize_email("Ramon.Fernandez@NTTData.com"),
            "ramon.fernandez@nttdata.com"
        );
        assert_eq!(sanitize_email("a+b@c.com"), "a_b@c.com");
        assert_eq!(sanitize_email("  "), "_unknown_");
        assert_eq!(sanitize_email("weird!#$%x@y.z"), "weird____x@y.z");
    }

    #[test]
    fn token_paths_per_user_and_legacy() {
        let home = Path::new("/home/u");
        assert_eq!(
            token_path(home, Some("Alice@Example.com")),
            PathBuf::from("/home/u/google_chat_user_tokens/alice@example.com.json")
        );
        assert_eq!(
            token_path(home, None),
            PathBuf::from("/home/u/google_chat_user_token.json")
        );
        assert_eq!(
            pending_path(home, Some("bob@x.y")),
            PathBuf::from("/home/u/google_chat_user_oauth_pending/bob@x.y.json")
        );
        assert_eq!(
            pending_path(home, None),
            PathBuf::from("/home/u/google_chat_user_oauth_pending.json")
        );
    }

    #[test]
    fn client_secret_parsing_and_validation() {
        let installed = r#"{"installed":{"client_id":"cid","client_secret":"cs","token_uri":"https://oauth2.googleapis.com/token"}}"#;
        let secret = parse_client_secret(installed).unwrap();
        assert_eq!(secret.client_id, "cid");
        assert_eq!(secret.token_uri, "https://oauth2.googleapis.com/token");
        let web = r#"{"web":{"client_id":"wcid","client_secret":"wcs"}}"#;
        let secret = parse_client_secret(web).unwrap();
        assert_eq!(secret.client_id, "wcid");
        assert_eq!(secret.token_uri, DEFAULT_TOKEN_ENDPOINT);
        assert!(parse_client_secret(r#"{"bogus":{}}"#).is_err());
        assert!(parse_client_secret("not json").is_err());
        assert!(parse_client_secret(r#"{"installed":{"client_secret":"x"}}"#).is_err());
    }

    #[test]
    fn store_and_load_client_secret_roundtrip() {
        let temp = tempfile::tempdir().expect("tempdir");
        let src = temp.path().join("client_secret.json");
        std::fs::write(
            &src,
            r#"{"installed":{"client_id":"cid","client_secret":"cs"}}"#,
        )
        .unwrap();
        let dest = store_client_secret(temp.path(), &src).unwrap();
        assert_eq!(dest, client_secret_path(temp.path()));
        let loaded = load_client_secret(temp.path()).unwrap();
        assert_eq!(loaded.client_id, "cid");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn pkce_shape_and_challenge() {
        let (state, verifier, challenge) = generate_pkce();
        assert_eq!(state.len(), 32); // 16 bytes hex
        assert!(verifier.len() >= 43); // RFC 7636 minimum
                                       // challenge == base64url(sha256(verifier))
        let expected = base64_url_nopad(&Sha256::digest(verifier.as_bytes()));
        assert_eq!(challenge, expected);
        assert!(!challenge.contains('='));
    }

    #[test]
    fn auth_url_carries_pkce_and_scopes() {
        let url = build_auth_url("client-123", "st4te", "ch4llenge");
        assert!(url.starts_with(AUTH_ENDPOINT));
        assert!(url.contains("client_id=client-123"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("access_type=offline"));
        assert!(url.contains("prompt=consent"));
        assert!(url.contains("state=st4te"));
        assert!(url.contains("code_challenge=ch4llenge"));
        assert!(url.contains("code_challenge_method=S256"));
        let scope_encoded: String =
            url::form_urlencoded::byte_serialize(USER_AUTH_SCOPE.as_bytes()).collect();
        assert!(url.contains(&scope_encoded));
        let redirect_encoded: String =
            url::form_urlencoded::byte_serialize(REDIRECT_URI.as_bytes()).collect();
        assert!(url.contains(&redirect_encoded));
    }

    #[test]
    fn start_auth_requires_client_secret_and_persists_pending() {
        let temp = tempfile::tempdir().expect("tempdir");
        assert!(start_auth(temp.path(), Some("a@b.c")).is_err());
        let src = temp.path().join("cs.json");
        std::fs::write(
            &src,
            r#"{"installed":{"client_id":"cid","client_secret":"cs"}}"#,
        )
        .unwrap();
        store_client_secret(temp.path(), &src).unwrap();
        let url = start_auth(temp.path(), Some("a@b.c")).unwrap();
        assert!(url.contains("client_id=cid"));
        let pending: Value = serde_json::from_str(
            &std::fs::read_to_string(pending_path(temp.path(), Some("a@b.c"))).unwrap(),
        )
        .unwrap();
        assert!(!pending["state"].as_str().unwrap().is_empty());
        assert!(!pending["code_verifier"].as_str().unwrap().is_empty());
        assert_eq!(pending["redirect_uri"], REDIRECT_URI);
    }

    #[test]
    fn extract_code_from_raw_and_url_forms() {
        let (code, state, _scope) = extract_code_and_state("4/rawcode").unwrap();
        assert_eq!(code, "4/rawcode");
        assert!(state.is_none());
        let (code, state, scope) = extract_code_and_state(
            "http://localhost:1/?state=xyz&code=abc123&scope=https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fchat.messages.create",
        )
        .unwrap();
        assert_eq!(code, "abc123");
        assert_eq!(state.as_deref(), Some("xyz"));
        assert!(scope.unwrap().contains("chat.messages.create"));
        assert!(extract_code_and_state("http://localhost:1/?nope=1").is_err());
    }

    #[test]
    fn user_token_expiry_semantics() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let mut token = UserToken {
            access_token: "at".into(),
            refresh_token: "rt".into(),
            expires_at: now + 3600,
            ..Default::default()
        };
        assert!(!token.expired());
        assert!(token.usable());
        token.expires_at = now - 10;
        assert!(token.expired());
        assert!(token.usable()); // refreshable
        token.refresh_token.clear();
        assert!(!token.usable()); // expired + no refresh
        token.expires_at = 0;
        token.access_token.clear();
        assert!(!token.usable());
    }

    #[test]
    fn token_roundtrip_and_listing() {
        let temp = tempfile::tempdir().expect("tempdir");
        let token = UserToken {
            access_token: "at".into(),
            refresh_token: "rt".into(),
            expires_at: 123,
            scope: USER_AUTH_SCOPE.into(),
            token_uri: DEFAULT_TOKEN_ENDPOINT.into(),
            client_id: "cid".into(),
            client_secret: "cs".into(),
        };
        persist_user_token(temp.path(), Some("Alice@X.y"), &token);
        persist_user_token(temp.path(), None, &token);
        let loaded = read_user_token(temp.path(), Some("alice@x.y")).unwrap();
        assert_eq!(loaded.access_token, "at");
        assert_eq!(loaded.refresh_token, "rt");
        assert_eq!(loaded.expires_at, 123);
        assert_eq!(
            list_authorized_emails(temp.path()),
            vec!["alice@x.y".to_string()]
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(token_path(temp.path(), Some("alice@x.y")))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[tokio::test]
    async fn load_missing_credentials_is_none() {
        let temp = tempfile::tempdir().expect("tempdir");
        let client = reqwest::Client::new();
        assert!(
            load_user_credentials(temp.path(), &client, Some("nobody@x.y"))
                .await
                .is_none()
        );
        assert!(load_user_credentials(temp.path(), &client, None)
            .await
            .is_none());
    }

    #[tokio::test]
    async fn revoke_without_token_reports_absent() {
        let temp = tempfile::tempdir().expect("tempdir");
        let client = reqwest::Client::new();
        let out = revoke(temp.path(), &client, Some("ghost@x.y")).await;
        assert!(out.contains("no stored token"));
    }
}

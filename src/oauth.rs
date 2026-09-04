//! OAuth 2.0 Device Authorization Grant (RFC 8628) — service-agnostic
//! port of the hermes portal login flow (`hermes_cli/portal_cli.py`,
//! `hermes_cli/auth.py` are Nous-Portal-specific; ulnclaw targets any
//! provider that implements the standard device flow).
//!
//! Config (`[oauth]`): device_authorization_url, token_url, client_id,
//! scopes, portal_url. Tokens persist at `<home>/oauth_tokens.json`
//! (0600) with refresh support.

use crate::error::{AgentError, Result};
use serde::{Deserialize, Serialize};
#[cfg(test)]
use serde_json::json;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// `[oauth]` config block.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OAuthConfig {
    /// RFC 8628 device authorization endpoint.
    #[serde(default)]
    pub device_authorization_url: String,
    /// Token endpoint (device_code + refresh_token grants).
    #[serde(default)]
    pub token_url: String,
    #[serde(default)]
    pub client_id: String,
    /// Space-separated scopes requested at authorization time.
    #[serde(default)]
    pub scopes: String,
    /// Human portal page (for `auth open`).
    #[serde(default)]
    pub portal_url: String,
}

/// Persisted token state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StoredTokens {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    /// Unix seconds when the access token expires (0 = unknown).
    #[serde(default)]
    pub expires_at: u64,
    #[serde(default)]
    pub scope: String,
}

impl StoredTokens {
    pub fn logged_in(&self) -> bool {
        !self.access_token.is_empty()
    }

    pub fn expired(&self) -> bool {
        self.expires_at > 0
            && std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() >= self.expires_at)
                .unwrap_or(false)
    }
}

pub fn tokens_path(home: &Path) -> PathBuf {
    home.join("oauth_tokens.json")
}

pub fn load_tokens(home: &Path) -> StoredTokens {
    std::fs::read_to_string(tokens_path(home))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Save tokens with 0600 permissions (POSIX).
pub fn save_tokens(home: &Path, tokens: &StoredTokens) -> Result<()> {
    let text = serde_json::to_string_pretty(tokens)
        .map_err(|e| AgentError::config(format!("serialize tokens: {e}")))?;
    let path = tokens_path(home);
    std::fs::write(&path, text).map_err(|e| AgentError::config(format!("write tokens: {e}")))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).ok();
    }
    Ok(())
}

pub fn clear_tokens(home: &Path) -> Result<()> {
    std::fs::remove_file(tokens_path(home))
        .or_else(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Ok(())
            } else {
                Err(e)
            }
        })
        .map_err(|e| AgentError::config(format!("remove tokens: {e}")))
}

fn require_endpoints(cfg: &OAuthConfig) -> Result<()> {
    if cfg.device_authorization_url.trim().is_empty() || cfg.token_url.trim().is_empty() {
        return Err(AgentError::config(
            "OAuth is not configured — set [oauth] device_authorization_url, \
             token_url, and client_id in config.toml",
        ));
    }
    Ok(())
}

fn parse_token_response(value: &Value) -> std::result::Result<StoredTokens, String> {
    if let Some(error) = value.get("error").and_then(|v| v.as_str()) {
        return Err(error.to_string());
    }
    let access_token = value
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or("token response missing access_token")?
        .to_string();
    let expires_in = value
        .get("expires_in")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let expires_at = if expires_in > 0 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() + expires_in)
            .unwrap_or(0)
    } else {
        0
    };
    Ok(StoredTokens {
        access_token,
        refresh_token: value
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        expires_at,
        scope: value
            .get("scope")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    })
}

async fn token_request(
    client: &reqwest::Client,
    cfg: &OAuthConfig,
    form: &[(&str, String)],
) -> Result<Value> {
    let response = client
        .post(&cfg.token_url)
        .header("Accept", "application/json")
        .form(
            &form
                .iter()
                .map(|(k, v)| (*k, v.clone()))
                .collect::<Vec<_>>(),
        )
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| AgentError::Tool(format!("token request: {e}")))?;
    let status = response.status();
    let value: Value = response
        .json()
        .await
        .map_err(|e| AgentError::Tool(format!("token response parse: {e}")))?;
    // OAuth errors arrive with 4xx but a JSON body.
    if value.get("error").is_some() || status.is_success() {
        Ok(value)
    } else {
        Err(AgentError::Tool(format!(
            "token endpoint returned {status}"
        )))
    }
}

/// One device-authorization attempt: returns the user-facing codes.
pub async fn device_authorize(cfg: &OAuthConfig) -> Result<Value> {
    require_endpoints(cfg)?;
    let client = reqwest::Client::new();
    let mut form = vec![("client_id", cfg.client_id.clone())];
    if !cfg.scopes.trim().is_empty() {
        form.push(("scope", cfg.scopes.trim().to_string()));
    }
    let response = client
        .post(&cfg.device_authorization_url)
        .header("Accept", "application/json")
        .form(&form)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| AgentError::Tool(format!("device authorization: {e}")))?;
    let value: Value = response
        .json()
        .await
        .map_err(|e| AgentError::Tool(format!("device authorization parse: {e}")))?;
    if value.get("device_code").is_none() {
        let error = value
            .get("error_description")
            .or_else(|| value.get("error"))
            .and_then(|v| v.as_str())
            .unwrap_or("no device_code in response");
        return Err(AgentError::Tool(format!("device authorization: {error}")));
    }
    Ok(value)
}

/// Poll the token endpoint until the user completes authorization
/// (RFC 8628 §3.4): authorization_pending keeps polling, slow_down adds
/// 5s, expired_token/access_denied abort.
pub async fn poll_for_token(cfg: &OAuthConfig, auth: &Value) -> Result<StoredTokens> {
    let client = reqwest::Client::new();
    let device_code = auth
        .get("device_code")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let expires_in = auth
        .get("expires_in")
        .and_then(|v| v.as_u64())
        .unwrap_or(600);
    let mut interval = auth
        .get("interval")
        .and_then(|v| v.as_u64())
        .unwrap_or(5)
        .max(3);
    let deadline = std::time::Instant::now() + Duration::from_secs(expires_in);
    loop {
        if std::time::Instant::now() > deadline {
            return Err(AgentError::Tool(
                "device code expired before authorization".into(),
            ));
        }
        tokio::time::sleep(Duration::from_secs(interval)).await;
        let form = vec![
            (
                "grant_type",
                "urn:ietf:params:oauth:grant-type:device_code".to_string(),
            ),
            ("device_code", device_code.clone()),
            ("client_id", cfg.client_id.clone()),
        ];
        let value = token_request(&client, cfg, &form).await?;
        match value.get("error").and_then(|v| v.as_str()) {
            None => return parse_token_response(&value).map_err(|e| AgentError::Tool(e)),
            Some("authorization_pending") => continue,
            Some("slow_down") => {
                interval += 5;
                continue;
            }
            Some("access_denied") => {
                return Err(AgentError::Tool("authorization denied by the user".into()));
            }
            Some("expired_token") => {
                return Err(AgentError::Tool(
                    "device code expired — run auth login again".into(),
                ));
            }
            Some(other) => return Err(AgentError::Tool(format!("token error: {other}"))),
        }
    }
}

/// Refresh the access token using the stored refresh_token.
pub async fn refresh(cfg: &OAuthConfig, home: &Path) -> Result<StoredTokens> {
    require_endpoints(cfg)?;
    let stored = load_tokens(home);
    if stored.refresh_token.is_empty() {
        return Err(AgentError::Tool(
            "no refresh_token stored — run auth login".into(),
        ));
    }
    let client = reqwest::Client::new();
    let form = vec![
        ("grant_type", "refresh_token".to_string()),
        ("refresh_token", stored.refresh_token.clone()),
        ("client_id", cfg.client_id.clone()),
    ];
    let value = token_request(&client, cfg, &form).await?;
    let mut tokens = parse_token_response(&value).map_err(AgentError::Tool)?;
    if tokens.refresh_token.is_empty() {
        tokens.refresh_token = stored.refresh_token;
    }
    save_tokens(home, &tokens)?;
    Ok(tokens)
}

/// Best-effort: return a usable access token, refreshing when expired.
pub async fn access_token(cfg: &OAuthConfig, home: &Path) -> Option<String> {
    let tokens = load_tokens(home);
    if !tokens.logged_in() {
        return None;
    }
    if tokens.expired() {
        if let Ok(fresh) = refresh(cfg, home).await {
            return Some(fresh.access_token);
        }
        return None;
    }
    Some(tokens.access_token)
}

/// Render the login prompt lines (hermes portal shows code + URL).
pub fn login_instructions(auth: &Value) -> Vec<String> {
    let user_code = auth
        .get("user_code")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let uri = auth
        .get("verification_uri_complete")
        .or_else(|| auth.get("verification_uri"))
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    vec![
        format!("Open:  {uri}"),
        format!("Code:  {user_code}"),
        "Waiting for authorization...".to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_home(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ulnclaw-oauth-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn token_roundtrip() {
        let home = tmp_home("roundtrip");
        let tokens = StoredTokens {
            access_token: "at".into(),
            refresh_token: "rt".into(),
            expires_at: 0,
            scope: "read".into(),
        };
        save_tokens(&home, &tokens).unwrap();
        let loaded = load_tokens(&home);
        assert_eq!(loaded.access_token, "at");
        assert!(loaded.logged_in());
        assert!(!loaded.expired(), "expires_at 0 = unknown = not expired");
        clear_tokens(&home).unwrap();
        assert!(!load_tokens(&home).logged_in());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn expiry_detection() {
        let mut tokens = StoredTokens::default();
        tokens.expires_at = 1; // 1970 — long past
        assert!(tokens.expired());
        tokens.expires_at = u64::MAX;
        // u64::MAX seconds is far future — not expired
        assert!(!tokens.expired());
    }

    #[test]
    fn parse_success_response() {
        let value = json!({
            "access_token": "abc",
            "refresh_token": "def",
            "expires_in": 3600,
            "scope": "skills:sync",
        });
        let tokens = parse_token_response(&value).unwrap();
        assert_eq!(tokens.access_token, "abc");
        assert_eq!(tokens.refresh_token, "def");
        assert!(tokens.expires_at > 0);
        assert_eq!(tokens.scope, "skills:sync");
    }

    #[test]
    fn parse_error_response() {
        let value = json!({"error": "authorization_pending"});
        assert_eq!(
            parse_token_response(&value).unwrap_err(),
            "authorization_pending"
        );
    }

    #[test]
    fn login_instructions_render() {
        let auth = json!({
            "user_code": "ABCD-1234",
            "verification_uri": "https://example.com/device",
            "interval": 5,
        });
        let lines = login_instructions(&auth);
        assert!(lines[0].contains("https://example.com/device"));
        assert!(lines[1].contains("ABCD-1234"));
    }

    #[test]
    fn require_endpoints_gates() {
        let cfg = OAuthConfig::default();
        assert!(require_endpoints(&cfg).is_err());
        let cfg = OAuthConfig {
            device_authorization_url: "https://x/device".into(),
            token_url: "https://x/token".into(),
            client_id: "c".into(),
            ..Default::default()
        };
        assert!(require_endpoints(&cfg).is_ok());
    }
}

//! Nous managed tool gateway helpers — port of hermes
//! `tools/managed_tool_gateway.py` (v2026.8.3).
//!
//! Managed vendors (e.g. BFL FLUX 3 video) are reached through a
//! Nous-hosted passthrough gateway. The client needs three things:
//!
//!   - the gateway origin (`{vendor}-gateway.nousresearch.com` by default,
//!     overridable via `TOOL_GATEWAY_DOMAIN` / `{VENDOR}_GATEWAY_URL`),
//!   - a Nous bearer token (`TOOL_GATEWAY_USER_TOKEN` env override, else
//!     `auth.json` provider state written by a Nous Portal sign-in),
//!   - the presigned-upload protocol for local media (`nous-upload:<token>`
//!     references instead of inline base64).
//!
//! Divergence from hermes: there is no OAuth refresh here (ulnclaw has no
//! Portal sign-in flow) — cached tokens are used as-is and an expired token
//! surfaces as the gateway's 401 "sign in" answer. Entitlement probing
//! (`managed_nous_tools_enabled`) keys on token presence instead of a Portal
//! account query; the gateway itself rules on spending.

use chrono::{DateTime, Utc};
use url::Url;

const DEFAULT_TOOL_GATEWAY_DOMAIN: &str = "nousresearch.com";
const DEFAULT_TOOL_GATEWAY_SCHEME: &str = "https";
/// Pseudo-vendor used only to resolve the shared tool-gateway origin.
const MANAGED_GATEWAY_VENDOR: &str = "tool";

/// The Hermes auth store path, respecting home overrides (hermes
/// `auth_json_path`).
pub fn auth_json_path() -> std::path::PathBuf {
    crate::config::ulnclaw_home().join("auth.json")
}

fn read_nous_provider_state() -> Option<serde_json::Value> {
    let path = auth_json_path();
    let data = std::fs::read_to_string(&path).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&data).ok()?;
    let nous = parsed.get("providers")?.get("nous")?;
    if nous.is_object() {
        Some(nous.clone())
    } else {
        None
    }
}

fn parse_timestamp(value: &serde_json::Value) -> Option<DateTime<Utc>> {
    let text = value.as_str()?.trim().to_string();
    if text.is_empty() {
        return None;
    }
    let normalized = if let Some(stripped) = text.strip_suffix('Z') {
        format!("{}+00:00", stripped)
    } else {
        text
    };
    DateTime::parse_from_rfc3339(&normalized)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn access_token_is_expiring(expires_at: &Option<serde_json::Value>, skew_seconds: i64) -> bool {
    let Some(value) = expires_at else {
        return true;
    };
    let Some(expires) = parse_timestamp(value) else {
        return true;
    };
    let remaining = (expires - Utc::now()).num_seconds();
    remaining <= skew_seconds.max(0)
}

/// `TOOL_GATEWAY_USER_TOKEN` env override (hermes reads it through the
/// secret scope; ulnclaw reads the process env directly).
fn read_user_token_override() -> Option<String> {
    let explicit = std::env::var("TOOL_GATEWAY_USER_TOKEN").ok()?;
    let trimmed = explicit.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// Cheap probe for a Nous gateway token without triggering refresh (hermes
/// `peek_nous_access_token`) — for availability scans.
pub fn peek_nous_access_token() -> Option<String> {
    if let Some(explicit) = read_user_token_override() {
        return Some(explicit);
    }
    let state = read_nous_provider_state().unwrap_or(serde_json::json!({}));
    let token = state.get("access_token")?.as_str()?.trim().to_string();
    if token.is_empty() {
        None
    } else {
        Some(token)
    }
}

/// Read a Nous Subscriber OAuth access token (hermes
/// `read_nous_access_token`). ulnclaw does not run the OAuth refresh; a
/// token past its expiry is still returned so the gateway can answer with
/// its own sign-in guidance.
pub fn read_nous_access_token() -> Option<String> {
    if let Some(explicit) = read_user_token_override() {
        return Some(explicit);
    }
    let state = read_nous_provider_state().unwrap_or(serde_json::json!({}));
    peek_nous_access_token().map(|token| {
        let _ = access_token_is_expiring(&state.get("expires_at").cloned(), 120);
        token
    })
}

/// Configured shared gateway URL scheme (hermes `get_tool_gateway_scheme`).
pub fn tool_gateway_scheme() -> Result<&'static str, String> {
    let scheme = std::env::var("TOOL_GATEWAY_SCHEME")
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if scheme.is_empty() {
        return Ok(DEFAULT_TOOL_GATEWAY_SCHEME);
    }
    match scheme.as_str() {
        "http" | "https" => Ok(if scheme == "http" { "http" } else { "https" }),
        _ => Err("TOOL_GATEWAY_SCHEME must be 'http' or 'https'".to_string()),
    }
}

/// Gateway origin for a specific vendor (hermes `build_vendor_gateway_url`).
pub fn build_vendor_gateway_url(vendor: &str) -> Result<String, String> {
    let vendor_key = format!("{}_GATEWAY_URL", vendor.to_uppercase().replace('-', "_"));
    if let Ok(explicit) = std::env::var(&vendor_key) {
        let trimmed = explicit.trim().trim_end_matches('/').to_string();
        if !trimmed.is_empty() {
            return Ok(trimmed);
        }
    }
    let scheme = tool_gateway_scheme()?;
    let shared_domain = std::env::var("TOOL_GATEWAY_DOMAIN")
        .unwrap_or_default()
        .trim()
        .trim_matches('/')
        .to_string();
    let domain = if shared_domain.is_empty() {
        DEFAULT_TOOL_GATEWAY_DOMAIN.to_string()
    } else {
        shared_domain
    };
    Ok(format!("{}://{}-gateway.{}", scheme, vendor, domain))
}

/// Absolute URLs for a managed vendor (hermes `managed_vendor_endpoints`).
/// `None` when no origin resolves (misconfigured scheme).
pub struct ManagedVendorEndpoints {
    pub origin: String,
    pub base_url: String,
    pub upload_path: String,
}

pub fn managed_vendor_endpoints(vendor: &str) -> Option<ManagedVendorEndpoints> {
    let origin = build_vendor_gateway_url(MANAGED_GATEWAY_VENDOR)
        .ok()?
        .trim_end_matches('/')
        .to_string();
    if origin.is_empty() {
        return None;
    }
    Some(ManagedVendorEndpoints {
        base_url: format!("{}/api/{}", origin, vendor),
        upload_path: format!("/api/uploads/{}", vendor),
        origin,
    })
}

/// True when `url` is on the Nous tool-gateway origin this client builds
/// (hermes `is_managed_nous_gateway_url`). Anything granting a URL extra
/// trust — the bearer, reading files off disk to upload — must gate on
/// this rather than on a name.
pub fn is_managed_nous_gateway_url(url: &str) -> bool {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return false;
    }
    let Ok(expected) = build_vendor_gateway_url(MANAGED_GATEWAY_VENDOR) else {
        return false;
    };
    let (Ok(expected), Ok(actual)) = (Url::parse(&expected), Url::parse(trimmed)) else {
        return false;
    };
    actual.scheme() == expected.scheme() && actual.host_str() == expected.host_str()
}

/// True when managed Nous tools may be used (hermes
/// `managed_nous_tools_enabled`). ulnclaw keys on token presence rather
/// than a Portal entitlement query; the gateway rules on spending.
pub fn managed_nous_tools_enabled() -> bool {
    peek_nous_access_token().is_some()
}

/// Live auth bearer for a managed gateway URL, or `None` when not managed
/// or unsigned-in (hermes `managed_gateway_auth_headers`). Read fresh on
/// every call: a Nous access token expires within the hour.
pub fn managed_gateway_auth_bearer(url: &str) -> Option<String> {
    if !is_managed_nous_gateway_url(url) {
        return None;
    }
    read_nous_access_token()
}

/// Presign + PUT one media blob to the managed upload endpoint; returns the
/// `nous-upload:<token>` reference for the tool argument (hermes
/// `build_managed_media_uploader`).
pub async fn upload_managed_media(
    base_url: &str,
    upload_path: &str,
    data: &[u8],
    mime: &str,
) -> Result<String, String> {
    if !is_managed_nous_gateway_url(base_url) {
        return Err("not a managed Nous gateway URL".to_string());
    }
    if !upload_path.starts_with('/') {
        return Err("upload path must be absolute".to_string());
    }
    let parsed = Url::parse(base_url).map_err(|e| format!("bad gateway URL: {}", e))?;
    let origin = format!(
        "{}://{}",
        parsed.scheme(),
        parsed.host_str().unwrap_or_default()
    );
    let presign_url = format!("{}{}", origin, upload_path);
    let Some(bearer) = managed_gateway_auth_bearer(base_url) else {
        return Err("no Nous credential is available for the upload".to_string());
    };

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;

    // 1. Presign: POST content type + exact length, get a single-object PUT
    //    URL and an upload token.
    let presign = client
        .post(&presign_url)
        .bearer_auth(&bearer)
        .json(&serde_json::json!({
            "contentType": mime,
            "contentLength": data.len(),
        }))
        .send()
        .await
        .map_err(|e| format!("could not reach the upload endpoint: {}", e))?;
    if presign.status() != 200 {
        let status = presign.status();
        let message = presign
            .json::<serde_json::Value>()
            .await
            .ok()
            .and_then(|v| {
                v.get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .map(|m| m.trim().to_string())
            })
            .filter(|m| !m.is_empty());
        return Err(
            message.unwrap_or_else(|| format!("the gateway refused the upload (HTTP {})", status))
        );
    }
    let payload: serde_json::Value = presign
        .json()
        .await
        .map_err(|_| "the gateway's upload response was malformed".to_string())?;
    let upload_url = payload
        .get("uploadUrl")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "the gateway's upload response was malformed".to_string())?;
    let token = payload
        .get("token")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "the gateway's upload response was malformed".to_string())?;

    // 2. PUT straight to storage (bypasses the gateway's request ceiling).
    let put_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .map_err(|e| e.to_string())?;
    let put = put_client
        .put(upload_url)
        .header("Content-Type", mime)
        .body(data.to_vec())
        .send()
        .await
        .map_err(|e| format!("storage upload failed: {}", e))?;
    if put.status() != 200 {
        return Err(format!(
            "storage refused the upload (HTTP {})",
            put.status()
        ));
    }

    Ok(format!("nous-upload:{}", token))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Process-wide lock: the gateway tests below mutate shared env vars
    /// (TOOL_GATEWAY_*) and would otherwise race under parallel test
    /// threads.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn default_gateway_origin() {
        let _guard = env_lock();
        // Clear overrides for a deterministic default.
        std::env::remove_var("TOOL_GATEWAY_DOMAIN");
        std::env::remove_var("TOOL_GATEWAY_SCHEME");
        std::env::remove_var("TOOL_GATEWAY_URL");
        let origin = build_vendor_gateway_url("bfl").expect("default builds");
        assert_eq!(origin, "https://bfl-gateway.nousresearch.com");
    }

    #[test]
    fn vendor_env_override_wins() {
        let _guard = env_lock();
        std::env::set_var("BFL_GATEWAY_URL", "http://127.0.0.1:9999/");
        let origin = build_vendor_gateway_url("bfl").expect("override builds");
        assert_eq!(origin, "http://127.0.0.1:9999");
        std::env::remove_var("BFL_GATEWAY_URL");
    }

    #[test]
    fn shared_domain_applies_to_all_vendors() {
        let _guard = env_lock();
        std::env::remove_var("BFL_GATEWAY_URL");
        std::env::set_var("TOOL_GATEWAY_DOMAIN", "example.test");
        let origin = build_vendor_gateway_url("bfl").expect("shared domain builds");
        assert_eq!(origin, "https://bfl-gateway.example.test");
        std::env::remove_var("TOOL_GATEWAY_DOMAIN");
    }

    #[test]
    fn managed_url_trust_check() {
        let _guard = env_lock();
        std::env::remove_var("TOOL_GATEWAY_DOMAIN");
        std::env::remove_var("TOOL_GATEWAY_URL");
        assert!(is_managed_nous_gateway_url(
            "https://tool-gateway.nousresearch.com/api/bfl/generations"
        ));
        assert!(!is_managed_nous_gateway_url(
            "https://evil.example.com/api/bfl"
        ));
        assert!(!is_managed_nous_gateway_url(""));
    }

    #[test]
    fn endpoints_shape() {
        let _guard = env_lock();
        std::env::remove_var("TOOL_GATEWAY_DOMAIN");
        let endpoints = managed_vendor_endpoints("bfl").expect("endpoints resolve");
        assert!(endpoints.base_url.ends_with("/api/bfl"));
        assert_eq!(endpoints.upload_path, "/api/uploads/bfl");
    }

    #[test]
    fn expiry_parsing() {
        let future = serde_json::json!((Utc::now() + chrono::Duration::hours(1)).to_rfc3339());
        assert!(!access_token_is_expiring(&Some(future), 120));
        let past = serde_json::json!((Utc::now() - chrono::Duration::hours(1)).to_rfc3339());
        assert!(access_token_is_expiring(&Some(past), 120));
        assert!(access_token_is_expiring(&None, 120));
    }
}

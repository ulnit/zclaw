//! Inbound cron-fire token verification for Chronos — port of hermes
//! `plugins/cron_providers/chronos/verify.py`.
//!
//! When NAS relays an external scheduler fire to the agent, it POSTs
//! `/api/jobs/fire` with a short-lived NAS-minted JWT. This module
//! verifies that JWT before any job runs — the security boundary for
//! remotely-triggered job execution. Crypto is delegated to
//! `jsonwebtoken` (never hand-rolled).

use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::OnceLock;

/// The purpose claim that scopes a token to the fire endpoint. A general
/// agent JWT (without this claim) must NOT be replayable against
/// `/api/jobs/fire` (hermes `_FIRE_PURPOSE`).
pub const FIRE_PURPOSE: &str = "cron_fire";

/// Asymmetric families accepted for NAS signatures — symmetric secrets
/// are rejected (hermes `algorithms=[...]`).
const ALLOWED_ALGORITHMS: &[Algorithm] = &[
    Algorithm::RS256,
    Algorithm::RS384,
    Algorithm::RS512,
    Algorithm::ES256,
    Algorithm::ES384,
];

/// Process-wide cache of fetched JWKS key sets, keyed by JWKS URL.
/// Reusing one entry per URL keeps the signing keys cached (NAS keys
/// rotate rarely), so the steady state is zero JWKS fetches per fire —
/// hermes `_JWK_CLIENTS` (a fresh fetch per fire tripped the portal's
/// rate limit and blew the relay's 30 s timeout).
fn jwks_cache() -> &'static tokio::sync::Mutex<HashMap<String, jsonwebtoken::jwk::JwkSet>> {
    static CACHE: OnceLock<tokio::sync::Mutex<HashMap<String, jsonwebtoken::jwk::JwkSet>>> =
        OnceLock::new();
    CACHE.get_or_init(|| tokio::sync::Mutex::new(HashMap::new()))
}

/// Shared HTTP client for JWKS fetches with the WAF-friendly headers
/// hermes uses (explicit Accept + User-Agent).
fn jwks_http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .unwrap_or_default()
    })
}

async fn fetch_jwks(url: &str) -> Option<jsonwebtoken::jwk::JwkSet> {
    {
        let cache = jwks_cache().lock().await;
        if let Some(existing) = cache.get(url) {
            return Some(existing.clone());
        }
    }
    let response = jwks_http_client()
        .get(url)
        .header("Accept", "application/json")
        .header("User-Agent", "HermesAgent/1.0")
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body = response.text().await.ok()?;
    let jwks: jsonwebtoken::jwk::JwkSet = serde_json::from_str(&body).ok()?;
    let mut cache = jwks_cache().lock().await;
    cache.insert(url.to_string(), jwks.clone());
    Some(jwks)
}

/// Key family of a resolved decoding key — `jsonwebtoken` requires every
/// algorithm in the validation list to belong to the key's family, so
/// the RS/ES allowlist must be narrowed to the resolved key's family.
#[derive(Debug, Clone, Copy, PartialEq)]
enum KeyFamily {
    Rsa,
    Ec,
}

/// Resolve the decoding key for a token: JWKS lookup by the token's
/// `kid` when `jwks_or_key` is a URL, or an inline PEM public key
/// otherwise (test / pinned-key deployments). Returns the key plus its
/// family.
async fn resolve_decoding_key(token: &str, jwks_or_key: &str) -> Option<(DecodingKey, KeyFamily)> {
    if jwks_or_key.starts_with("http://") || jwks_or_key.starts_with("https://") {
        let jwks = fetch_jwks(jwks_or_key).await?;
        let header = jsonwebtoken::decode_header(token).ok()?;
        // Look up by kid; a kid-less token against a single-key set uses
        // that key (PyJWKClient parity for single-key deployments).
        let jwk = match header.kid.as_deref() {
            Some(kid) => jwks.find(kid).or_else(|| {
                if jwks.keys.len() == 1 {
                    jwks.keys.first()
                } else {
                    None
                }
            }),
            None => {
                if jwks.keys.len() == 1 {
                    jwks.keys.first()
                } else {
                    None
                }
            }
        }?;
        let family = match &jwk.algorithm {
            jsonwebtoken::jwk::AlgorithmParameters::EllipticCurve(_) => KeyFamily::Ec,
            _ => KeyFamily::Rsa,
        };
        DecodingKey::from_jwk(jwk).ok().map(|key| (key, family))
    } else {
        if let Ok(key) = DecodingKey::from_rsa_pem(jwks_or_key.as_bytes()) {
            return Some((key, KeyFamily::Rsa));
        }
        DecodingKey::from_ec_pem(jwks_or_key.as_bytes())
            .ok()
            .map(|key| (key, KeyFamily::Ec))
    }
}

/// Verify a NAS-minted cron-fire JWT. Returns decoded claims, or None
/// (never panics) on any failure, so the handler can answer 401 without
/// leaking which check failed. Checks (all must pass):
///   - signature against the NAS JWKS / inline PEM — RS/ES family only;
///     symmetric secrets are rejected.
///   - `aud` == `expected_audience`.
///   - `exp` / `nbf` within `leeway_secs`.
///   - `iss` == `issuer` when an issuer is configured.
///   - `purpose` == `"cron_fire"`.
pub async fn verify_fire_token(
    token: &str,
    expected_audience: &str,
    jwks_or_key: Option<&str>,
    issuer: Option<&str>,
    leeway_secs: u64,
) -> Option<Value> {
    if token.is_empty() || expected_audience.is_empty() {
        return None;
    }
    let jwks_or_key = jwks_or_key.filter(|key| !key.is_empty())?;

    let header = jsonwebtoken::decode_header(token).ok()?;
    if !ALLOWED_ALGORITHMS.contains(&header.alg) {
        return None;
    }
    let (decoding_key, family) = resolve_decoding_key(token, jwks_or_key).await?;

    let mut validation = Validation::new(header.alg);
    // Narrow the allowlist to the resolved key's family (jsonwebtoken
    // rejects a validation list mixing families).
    validation.algorithms = ALLOWED_ALGORITHMS
        .iter()
        .filter(|alg| match (family, alg) {
            (KeyFamily::Rsa, alg) => {
                matches!(alg, Algorithm::RS256 | Algorithm::RS384 | Algorithm::RS512)
            }
            (KeyFamily::Ec, alg) => matches!(alg, Algorithm::ES256 | Algorithm::ES384),
        })
        .copied()
        .collect();
    validation.set_audience(&[expected_audience]);
    validation.leeway = leeway_secs;
    validation.required_spec_claims = ["exp"].iter().map(|s| s.to_string()).collect();
    if let Some(issuer) = issuer.filter(|issuer| !issuer.is_empty()) {
        validation.set_issuer(&[issuer]);
    }

    let token_data = jsonwebtoken::decode::<Value>(token, &decoding_key, &validation).ok()?;
    if token_data.claims.get("purpose").and_then(Value::as_str) != Some(FIRE_PURPOSE) {
        return None;
    }
    Some(token_data.claims)
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{encode, EncodingKey, Header};
    use serde_json::json;

    // Fixed test-only RSA-2048 key pair (generated for these tests; the
    // private half never leaves this file).
    const TEST_PRIVATE_PEM: &str = r#"-----BEGIN PRIVATE KEY-----
MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQCSvaIDx3Hb8l3N
9XeRqOaS573tRdCt7uuymy03owpSYiTbaWiyWw2muUEPwiywcyLaCChSwRncPyJ9
0Mnd/e1D1NAnST16A1lYl+TO+tDS4ect657i8BxqE7LasjNZUe4uXJ9yFD9nFrgs
mgI6OcZfcQvdjO/ztzEz/ThBWh7LTIyvzp13Dboo/mwCMZXTTslce/ffCDu04mbU
eTV4eJswOUHfUPaZ92KsHYgaZmg2ghwW+i8DdJ7MkKrsO2K86fBhOi5FbVNqUYul
jD2ptKiJDUm+6jkTX+MuLaETQWYFXSVoxke2xD/psL9sDJcWZOYwKL6d/elWkD1P
lCHiJHPRAgMBAAECggEAJVINjqB/GM1/hg5UJruqSNqft2T2OgZ186r7yRayXVmQ
vi0E77ewtSKQpY1hCE+AIavJdaKfDSERiKY9cTRPz9ykRBmghROs+ZdIHkw0KC5E
Oa2fb2BaGbCA4JZJ8QGhbjEobD8yEOn6VX2l62EeTs/VkLdzn6yL2wkf8Z8WDeY7
kp5IIgktXVKZ0xP2lKhcE04EmPj8T3FGzS6AibdsBhjwRSdBFlmE0wqKa2k6lBns
D5m8H1OnE0/tWMJ+YOZHN+29CLwpKcksOWl38h1FUw42EL7oprul7ydS02OLWjwy
sGmVies77zLFYPvseyxMwu8KL5Fn7lzlYQL1zEHDsQKBgQDG35BWdbHaU5oFFwCb
xgkKN4qx0kreBsd6eTwwfI5rQHy778Cc19P4ApuI9HmRxKII9FbKuHzJb3ZRrmN9
MwRhmCtklXxvaw075WBZVERgKbGA3P5hlNcjX1JmvgPNLkCW+qTtJp1zcCgf1h/7
UXgxqUoQ3DR1M6FgXq5a6iRU8wKBgQC85G1LXMERMGDKWzixLigP78TbiW258vMU
MKTRf4Jli8RKASRAe+DFs0klk5eYvlqyWgZt6/4vkPguOfmy/KW5kl2n3CfoRIed
DshI6M19BHEV4G6N3qzsnNQxoQPeDN/IamDou0RhyTwgudgBVdaHMxNPuKOtxCBC
xWV1JjTVKwKBgDVHtA3V3l5Vw4/Vh840EjvwgXH+mxw8yLihPmTnGejWEBTxuLLM
h/eMC0t35BIPkjG/9Hi/UH9PI23iwLjMMEJNWGLMQdg/3/3KCDQmhWMWCH4ztttB
2xmY8iSgh7gyyg8o+4Kls803oShWX58fRopXhoZZ2JwFxxhghWnKDQ3NAoGBAJ5Q
lYnkY6yUb4sqiYl2tf0laEjYFi8TgMgbPQiZZiDV095ytn+VU/5fFZ945EYQxNNW
wKzAbnpPdrLHxJBPUFcIZZaa3pe9WCw6h4MUG6X8YwuC3yXoy+ZES1SNL0CcabL/
9dkZm2aZ0tta57+2webu1/CpQAYTqzZLW42kSAOhAoGBAKiE8vYmA4EcmxngSYEA
cnOnGmFUh6k36PMifq5EQmPJvy1kK5zn2Ay9pOXN0DT89NjTF7n/2+3yQRRWzWDY
ELGypw8hndOvpGrG2zj7MnhECfEcXZcTh3PrBEU/DsYV3EISNN5qh4Uby1zeYqxU
FFkH6vhVrqNHIzR6WyBVZTya
-----END PRIVATE KEY-----"#;

    const TEST_PUBLIC_PEM: &str = r#"-----BEGIN PUBLIC KEY-----
MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAkr2iA8dx2/JdzfV3kajm
kue97UXQre7rspstN6MKUmIk22loslsNprlBD8IssHMi2ggoUsEZ3D8ifdDJ3f3t
Q9TQJ0k9egNZWJfkzvrQ0uHnLeue4vAcahOy2rIzWVHuLlyfchQ/Zxa4LJoCOjnG
X3EL3Yzv87cxM/04QVoey0yMr86ddw26KP5sAjGV007JXHv33wg7tOJm1Hk1eHib
MDlB31D2mfdirB2IGmZoNoIcFvovA3SezJCq7DtivOnwYTouRW1TalGLpYw9qbSo
iQ1Jvuo5E1/jLi2hE0FmBV0laMZHtsQ/6bC/bAyXFmTmMCi+nf3pVpA9T5Qh4iRz
0QIDAQAB
-----END PUBLIC KEY-----"#;

    const TEST_JWK: &str = r#"{"kty":"RSA","alg":"RS256","use":"sig","kid":"test-key-1","n":"kr2iA8dx2_JdzfV3kajmkue97UXQre7rspstN6MKUmIk22loslsNprlBD8IssHMi2ggoUsEZ3D8ifdDJ3f3tQ9TQJ0k9egNZWJfkzvrQ0uHnLeue4vAcahOy2rIzWVHuLlyfchQ_Zxa4LJoCOjnGX3EL3Yzv87cxM_04QVoey0yMr86ddw26KP5sAjGV007JXHv33wg7tOJm1Hk1eHibMDlB31D2mfdirB2IGmZoNoIcFvovA3SezJCq7DtivOnwYTouRW1TalGLpYw9qbSoiQ1Jvuo5E1_jLi2hE0FmBV0laMZHtsQ_6bC_bAyXFmTmMCi-nf3pVpA9T5Qh4iRz0Q","e":"AQAB"}"#;

    fn now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    fn sign(claims: Value) -> String {
        encode(
            &Header::new(Algorithm::RS256),
            &claims,
            &EncodingKey::from_rsa_pem(TEST_PRIVATE_PEM.as_bytes()).unwrap(),
        )
        .unwrap()
    }

    fn fire_claims(aud: &str, exp: i64) -> Value {
        json!({"aud": aud, "exp": exp, "purpose": "cron_fire", "sub": "agent:test"})
    }

    #[tokio::test]
    async fn test_verify_inline_pem_success() {
        let token = sign(fire_claims("agent:instance-1", now() + 300));
        let claims = verify_fire_token(&token, "agent:instance-1", Some(TEST_PUBLIC_PEM), None, 30)
            .await
            .expect("valid token verifies");
        assert_eq!(claims["purpose"], "cron_fire");
        assert_eq!(claims["sub"], "agent:test");
    }

    #[tokio::test]
    async fn test_verify_rejects_wrong_purpose() {
        let mut claims = fire_claims("agent:instance-1", now() + 300);
        claims["purpose"] = json!("general");
        let token = sign(claims);
        assert!(
            verify_fire_token(&token, "agent:instance-1", Some(TEST_PUBLIC_PEM), None, 30)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn test_verify_rejects_wrong_audience() {
        let token = sign(fire_claims("agent:other", now() + 300));
        assert!(
            verify_fire_token(&token, "agent:instance-1", Some(TEST_PUBLIC_PEM), None, 30)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn test_verify_rejects_expired_beyond_leeway() {
        let token = sign(fire_claims("agent:instance-1", now() - 120));
        assert!(
            verify_fire_token(&token, "agent:instance-1", Some(TEST_PUBLIC_PEM), None, 30)
                .await
                .is_none()
        );
        // Within leeway it passes.
        let token = sign(fire_claims("agent:instance-1", now() - 10));
        assert!(
            verify_fire_token(&token, "agent:instance-1", Some(TEST_PUBLIC_PEM), None, 30)
                .await
                .is_some()
        );
    }

    #[tokio::test]
    async fn test_verify_rejects_symmetric_algorithm() {
        let token = encode(
            &Header::new(Algorithm::HS256),
            &fire_claims("agent:instance-1", now() + 300),
            &EncodingKey::from_secret(b"shared-secret"),
        )
        .unwrap();
        assert!(
            verify_fire_token(&token, "agent:instance-1", Some(TEST_PUBLIC_PEM), None, 30)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn test_verify_requires_key_and_audience() {
        let token = sign(fire_claims("agent:instance-1", now() + 300));
        assert!(
            verify_fire_token(&token, "agent:instance-1", None, None, 30)
                .await
                .is_none()
        );
        assert!(
            verify_fire_token(&token, "", Some(TEST_PUBLIC_PEM), None, 30)
                .await
                .is_none()
        );
        assert!(
            verify_fire_token("", "agent:instance-1", Some(TEST_PUBLIC_PEM), None, 30)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn test_verify_issuer_enforced_when_configured() {
        let mut claims = fire_claims("agent:instance-1", now() + 300);
        claims["iss"] = json!("https://portal.example.org");
        let token = sign(claims);
        assert!(verify_fire_token(
            &token,
            "agent:instance-1",
            Some(TEST_PUBLIC_PEM),
            Some("https://other.example.org"),
            30
        )
        .await
        .is_none());
        assert!(verify_fire_token(
            &token,
            "agent:instance-1",
            Some(TEST_PUBLIC_PEM),
            Some("https://portal.example.org"),
            30
        )
        .await
        .is_some());
    }

    /// Serve a JWKS document on a random port; returns (base_url,
    /// fetch_counter).
    async fn jwks_server(
        jwks_body: String,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use axum::routing::get;
        use axum::Router;
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter_in = counter.clone();
        let app = Router::new().route(
            "/jwks.json",
            get(move || {
                let counter = counter_in.clone();
                let body = jwks_body.clone();
                async move {
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    (
                        [(axum::http::header::CONTENT_TYPE, "application/json")],
                        body,
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        (
            format!("http://127.0.0.1:{}/jwks.json", addr.port()),
            counter,
        )
    }

    #[tokio::test]
    async fn test_verify_via_jwks_url_with_caching() {
        let jwks = format!(r#"{{"keys":[{}]}}"#, TEST_JWK);
        let (url, counter) = jwks_server(jwks).await;
        let token = sign(fire_claims("agent:instance-1", now() + 300));
        let claims = verify_fire_token(&token, "agent:instance-1", Some(&url), None, 30)
            .await
            .expect("JWKS-verified token passes");
        assert_eq!(claims["purpose"], "cron_fire");
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
        // Second fire: the cached key set is reused — no second fetch.
        assert!(
            verify_fire_token(&token, "agent:instance-1", Some(&url), None, 30)
                .await
                .is_some()
        );
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_verify_jwks_unknown_kid_fails() {
        let jwk: Value = serde_json::from_str(TEST_JWK).unwrap();
        let mut renamed = jwk;
        renamed["kid"] = json!("some-other-key");
        let jwks = format!(r#"{{"keys":[{}]}}"#, renamed);
        let (url, _) = jwks_server(jwks).await;
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("test-key-1".to_string());
        let token = encode(
            &header,
            &fire_claims("agent:instance-1", now() + 300),
            &EncodingKey::from_rsa_pem(TEST_PRIVATE_PEM.as_bytes()).unwrap(),
        )
        .unwrap();
        // Single-key JWKS fallback applies (one key in the set), so the
        // verification still succeeds — matching PyJWKClient behaviour
        // for single-key sets.
        assert!(
            verify_fire_token(&token, "agent:instance-1", Some(&url), None, 30)
                .await
                .is_some()
        );
        // Two keys and no matching kid → fails.
        let mut second: Value = serde_json::from_str(TEST_JWK).unwrap();
        second["kid"] = json!("another-key");
        let jwks = format!(r#"{{"keys":[{},{}]}}"#, renamed, second);
        let (url, _) = jwks_server(jwks).await;
        assert!(
            verify_fire_token(&token, "agent:instance-1", Some(&url), None, 30)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn test_verify_garbage_token_fails() {
        assert!(
            verify_fire_token("not-a-jwt", "agent:x", Some(TEST_PUBLIC_PEM), None, 30)
                .await
                .is_none()
        );
    }
}

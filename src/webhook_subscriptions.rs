//! Dynamic webhook subscriptions (hermes `hermes_cli/webhook.py` parity).
//!
//! `hermes webhook subscribe|list|remove|test` manages per-route webhook
//! definitions in `~/.hermes/webhook_subscriptions.json`; the gateway
//! hot-reloads the file on every request, so subscriptions take effect
//! without a restart. This module ports the store + CLI logic; the
//! gateway-side handler lives in `gateway/mod.rs` (`dynamic_webhook_route`)
//! and maps each subscription onto the generic webhook pipeline.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::webhook_platforms::WebhookRoute;

pub const SUBSCRIPTIONS_FILENAME: &str = "webhook_subscriptions.json";

/// One dynamic subscription (hermes route dict schema).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Subscription {
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub events: Vec<String>,
    #[serde(default)]
    pub secret: String,
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default = "default_deliver")]
    pub deliver: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deliver_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deliver_extra: Option<DeliverExtra>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    #[serde(default)]
    pub created_at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeliverExtra {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_id: Option<String>,
}

fn default_deliver() -> String {
    "log".to_string()
}

pub fn subscriptions_path() -> PathBuf {
    crate::config::ulnclaw_home().join(SUBSCRIPTIONS_FILENAME)
}

/// Load the subscription store; missing or corrupt files yield an empty
/// map (hermes `_load_subscriptions`).
pub fn load_subscriptions() -> BTreeMap<String, Subscription> {
    let path = subscriptions_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return BTreeMap::new();
    };
    match serde_json::from_str::<BTreeMap<String, Subscription>>(&text) {
        Ok(map) => map,
        Err(_) => BTreeMap::new(),
    }
}

/// Atomic save with 0600 permissions (hermes `_save_subscriptions` — the
/// file holds per-route HMAC secrets).
pub fn save_subscriptions(subs: &BTreeMap<String, Subscription>) -> Result<(), String> {
    let path = subscriptions_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(subs).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &json).map_err(|e| e.to_string())?;
    set_owner_only_permissions(&tmp);
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    set_owner_only_permissions(&path);
    Ok(())
}

#[cfg(unix)]
fn set_owner_only_permissions(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mut perms = meta.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(path, perms).ok();
    }
}

#[cfg(not(unix))]
fn set_owner_only_permissions(_path: &std::path::Path) {}

/// Subscription-name grammar (hermes: lowercase alphanumeric leading,
/// then `[a-z0-9_-]`).
pub fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// Normalize a user-supplied name the way hermes does (trim, lowercase,
/// spaces → hyphens).
pub fn normalize_name(raw: &str) -> String {
    raw.trim().to_lowercase().replace(' ', "-")
}

/// Mint a random route secret (hermes `secrets.token_urlsafe(32)`).
/// No `rand` dependency: two UUIDv4s, hyphens stripped → 64 hex chars.
pub fn mint_secret() -> String {
    let mut out = String::with_capacity(64);
    for _ in 0..2 {
        out.extend(
            uuid::Uuid::new_v4()
                .to_string()
                .chars()
                .filter(|c| *c != '-'),
        );
    }
    out
}

/// Current UTC timestamp in hermes' `created_at` format.
pub fn created_at_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // RFC-3339 date/time without pulling in chrono: derive the fields
    // from the civil-from-days algorithm.
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, mo, d) = civil_from_days(days);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Howard Hinnant's civil-from-days (proleptic Gregorian).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Map a subscription to the gateway's generic webhook route type so the
/// dynamic handler rides the exact same pipeline as static config routes.
/// `skills`/`script` persist for hermes schema parity; the current
/// pipeline consumes the remaining fields.
pub fn to_webhook_route(name: &str, sub: &Subscription) -> WebhookRoute {
    WebhookRoute {
        name: name.to_string(),
        secret: sub.secret.clone(),
        events: sub.events.clone(),
        prompt: if sub.prompt.is_empty() {
            "Incoming webhook ({event}): {body}".to_string()
        } else {
            sub.prompt.clone()
        },
        deliver: if sub.deliver.is_empty() {
            "log".to_string()
        } else {
            sub.deliver.clone()
        },
        deliver_chat: sub
            .deliver_extra
            .as_ref()
            .and_then(|e| e.chat_id.clone())
            .unwrap_or_default(),
        deliver_only: sub.deliver_only,
    }
}

/// Base URL for webhook endpoints, derived from `[gateway]` host/port
/// (hermes `_get_webhook_base_url` display rules).
pub fn base_url(config: &crate::config::UlncLawConfig) -> String {
    let host = &config.gateway.host;
    let port = config.gateway.port;
    let display = if host.is_empty() || host == "0.0.0.0" || host == "::" {
        "localhost".to_string()
    } else {
        host.clone()
    };
    let display = if display.contains(':') && !display.starts_with('[') {
        format!("[{display}]")
    } else {
        display
    };
    format!("http://{display}:{port}")
}

/// HMAC-SHA256 signature header for a payload (hermes test command:
/// `sha256=<hex>`).
pub fn signature_header(secret: &str, payload: &str) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else {
        return String::new();
    };
    mac.update(payload.as_bytes());
    let hex: String = mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("sha256={hex}")
}

// ---------------------------------------------------------------------------
// CLI command logic (return renderable strings; main.rs prints them)
// ---------------------------------------------------------------------------

/// Options for `webhook subscribe`.
#[derive(Debug, Clone, Default)]
pub struct SubscribeOptions {
    pub description: Option<String>,
    pub events: Option<String>,
    pub secret: Option<String>,
    pub prompt: Option<String>,
    pub skills: Option<String>,
    pub deliver: Option<String>,
    pub deliver_chat_id: Option<String>,
    pub deliver_only: bool,
    pub script: Option<String>,
}

pub fn cmd_subscribe(raw_name: &str, opts: &SubscribeOptions) -> Result<String, String> {
    let name = normalize_name(raw_name);
    if !valid_name(&name) {
        return Err(format!(
            "Invalid name '{name}'. Use lowercase alphanumeric with hyphens/underscores."
        ));
    }
    let mut subs = load_subscriptions();
    let is_update = subs.contains_key(&name);

    let secret = opts
        .secret
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(mint_secret);
    let events: Vec<String> = opts
        .events
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(|s| {
            s.split(',')
                .map(|e| e.trim().to_string())
                .filter(|e| !e.is_empty())
                .collect()
        })
        .unwrap_or_default();

    let mut route = Subscription {
        description: opts
            .description
            .clone()
            .unwrap_or_else(|| format!("Agent-created subscription: {name}")),
        events,
        secret,
        prompt: opts.prompt.clone().unwrap_or_default(),
        skills: opts
            .skills
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(|s| {
                s.split(',')
                    .map(|e| e.trim().to_string())
                    .filter(|e| !e.is_empty())
                    .collect()
            })
            .unwrap_or_default(),
        deliver: opts
            .deliver
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(default_deliver),
        deliver_only: false,
        deliver_extra: None,
        script: None,
        created_at: created_at_now(),
    };

    if opts.deliver_only {
        if route.deliver == "log" {
            return Err("--deliver-only requires --deliver to be a real target \
                 (telegram, discord, slack, …) — not 'log'."
                .to_string());
        }
        route.deliver_only = true;
    }
    if let Some(script) = opts
        .script
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        route.script = Some(script.to_string());
    }
    if let Some(chat_id) = opts
        .deliver_chat_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        route.deliver_extra = Some(DeliverExtra {
            chat_id: Some(chat_id.to_string()),
        });
    }

    subs.insert(name.clone(), route.clone());
    save_subscriptions(&subs)?;

    let config = crate::config::UlncLawConfig::load(None).unwrap_or_default();
    let base = base_url(&config);
    let status = if is_update { "Updated" } else { "Created" };

    let mut out = String::new();
    out.push_str(&format!("\n  {status} webhook subscription: {name}\n"));
    out.push_str(&format!("  URL:    {base}/webhooks/{name}\n"));
    out.push_str(&format!("  Secret: {}\n", route.secret));
    if route.events.is_empty() {
        out.push_str("  Events: (all)\n");
    } else {
        out.push_str(&format!("  Events: {}\n", route.events.join(", ")));
    }
    out.push_str(&format!("  Deliver: {}\n", route.deliver));
    if route.deliver_only {
        out.push_str("  Mode: direct delivery (no agent, zero LLM cost)\n");
    }
    if !route.prompt.is_empty() {
        let preview: String = route.prompt.chars().take(80).collect();
        let suffix = if route.prompt.chars().count() > 80 {
            "..."
        } else {
            ""
        };
        let label = if route.deliver_only {
            "Message"
        } else {
            "Prompt"
        };
        out.push_str(&format!("  {label}: {preview}{suffix}\n"));
    }
    if let Some(script) = &route.script {
        out.push_str(&format!("  Script: {script}\n"));
    }
    out.push_str("\n  Configure your service to POST to the URL above.\n");
    out.push_str("  Use the secret for HMAC-SHA256 signature validation.\n");
    out.push_str("  The gateway must be running to receive events (ulnclaw gateway).\n");
    Ok(out)
}

pub fn cmd_list() -> Result<String, String> {
    let subs = load_subscriptions();
    if subs.is_empty() {
        return Ok("  No dynamic webhook subscriptions.\n  Create one with: ulnclaw webhook subscribe <name>\n".to_string());
    }
    let config = crate::config::UlncLawConfig::load(None).unwrap_or_default();
    let base = base_url(&config);
    let mut out = format!("\n  {} webhook subscription(s):\n\n", subs.len());
    for (name, route) in &subs {
        let events = if route.events.is_empty() {
            "(all)".to_string()
        } else {
            route.events.join(", ")
        };
        let mut deliver = route.deliver.clone();
        if route.deliver_only {
            deliver.push_str(" (direct — no agent)");
        }
        out.push_str(&format!("  ◆ {name}\n"));
        if !route.description.is_empty() {
            out.push_str(&format!("    {}\n", route.description));
        }
        out.push_str(&format!("    URL:     {base}/webhooks/{name}\n"));
        out.push_str(&format!("    Events:  {events}\n"));
        out.push_str(&format!("    Deliver: {deliver}\n"));
        if let Some(script) = &route.script {
            out.push_str(&format!("    Script:  {script}\n"));
        }
        out.push('\n');
    }
    Ok(out)
}

pub fn cmd_remove(raw_name: &str) -> Result<String, String> {
    let name = raw_name.trim().to_lowercase();
    let mut subs = load_subscriptions();
    if subs.remove(&name).is_none() {
        return Ok(format!(
            "  No subscription named '{name}'.\n  Note: Static routes from config.toml cannot be removed here.\n"
        ));
    }
    save_subscriptions(&subs)?;
    Ok(format!("  Removed webhook subscription: {name}\n"))
}

pub fn cmd_test(raw_name: &str, payload: Option<&str>) -> Result<String, String> {
    let name = raw_name.trim().to_lowercase();
    let subs = load_subscriptions();
    let Some(route) = subs.get(&name) else {
        return Ok(format!("  No subscription named '{name}'.\n"));
    };
    let config = crate::config::UlncLawConfig::load(None).unwrap_or_default();
    let url = format!("{}/webhooks/{name}", base_url(&config));
    let body = payload
        .filter(|p| !p.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            r#"{"test": true, "event_type": "test", "message": "Hello from ulnclaw webhook test"}"#
                .to_string()
        });
    let sig = signature_header(&route.secret, &body);

    let mut out = format!("  Sending test POST to {url}\n");
    match post_json_signed(&url, &body, &sig) {
        Ok((status, resp_body)) => {
            out.push_str(&format!("  Response ({status}): {resp_body}\n"));
        }
        Err(e) => {
            out.push_str(&format!(
                "  Error: {e}\n  Is the gateway running? (ulnclaw gateway)\n"
            ));
        }
    }
    Ok(out)
}

/// Signed JSON POST. The reqwest blocking client lives on a scoped OS
/// thread (models_dev parity) so async dispatch contexts never panic.
fn post_json_signed(url: &str, body: &str, signature: &str) -> Result<(u16, String), String> {
    let url = url.to_string();
    let body = body.to_string();
    let signature = signature.to_string();
    std::thread::scope(|scope| {
        scope
            .spawn(move || {
                let client = reqwest::blocking::Client::builder()
                    .timeout(std::time::Duration::from_secs(10))
                    .build()
                    .map_err(|e| format!("client: {e}"))?;
                let resp = client
                    .post(&url)
                    .header("Content-Type", "application/json")
                    .header("X-Hub-Signature-256", &signature)
                    .header("X-GitHub-Event", "test")
                    .body(body)
                    .send()
                    .map_err(|e| format!("post: {e}"))?;
                let status = resp.status().as_u16();
                let text = resp.text().unwrap_or_default();
                Ok((status, text))
            })
            .join()
            .map_err(|_| "webhook test thread panicked".to_string())?
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn with_home<F: FnOnce()>(f: F) {
        // ULNCLAW_HOME mutation must be serialized across the whole
        // process (shared with other env-mutating suites).
        let _guard = crate::models_dev::test_env_lock();
        let dir = std::env::temp_dir().join(format!("ulnclaw-whsub-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("ULNCLAW_HOME", &dir);
        f();
        std::env::remove_var("ULNCLAW_HOME");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn name_validation_matches_hermes_grammar() {
        assert!(valid_name("github-push"));
        assert!(valid_name("a"));
        assert!(valid_name("0hook"));
        assert!(valid_name("my_hook-2"));
        assert!(!valid_name(""));
        assert!(!valid_name("-lead"));
        assert!(!valid_name("_lead"));
        assert!(!valid_name("Upper"));
        assert!(!valid_name("has space"));
    }

    #[test]
    fn normalize_name_lowercases_and_hyphenates() {
        assert_eq!(normalize_name("  My Hook "), "my-hook");
    }

    #[test]
    fn mint_secret_is_long_and_unique() {
        let a = mint_secret();
        let b = mint_secret();
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn subscription_roundtrip_via_file() {
        with_home(|| {
            let mut subs = BTreeMap::new();
            subs.insert(
                "ci".to_string(),
                Subscription {
                    description: "CI events".into(),
                    events: vec!["push".into()],
                    secret: "s3cret".into(),
                    prompt: "handle {body}".into(),
                    deliver: "telegram".into(),
                    deliver_extra: Some(DeliverExtra {
                        chat_id: Some("123".into()),
                    }),
                    created_at: created_at_now(),
                    ..Default::default()
                },
            );
            save_subscriptions(&subs).unwrap();
            let loaded = load_subscriptions();
            let sub = &loaded["ci"];
            assert_eq!(sub.secret, "s3cret");
            assert_eq!(
                sub.deliver_extra.as_ref().unwrap().chat_id.as_deref(),
                Some("123")
            );
            // Secret file must be owner-only on unix.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(subscriptions_path())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777;
                assert_eq!(mode, 0o600);
            }
        });
    }

    #[test]
    fn load_missing_or_corrupt_is_empty() {
        with_home(|| {
            assert!(load_subscriptions().is_empty());
            std::fs::write(subscriptions_path(), "{ not json").unwrap();
            assert!(load_subscriptions().is_empty());
        });
    }

    #[test]
    fn to_webhook_route_maps_fields() {
        let sub = Subscription {
            secret: "abc".into(),
            events: vec!["push".into()],
            prompt: String::new(),
            deliver: "discord".into(),
            deliver_only: true,
            deliver_extra: Some(DeliverExtra {
                chat_id: Some("42".into()),
            }),
            ..Default::default()
        };
        let route = to_webhook_route("ci", &sub);
        assert_eq!(route.name, "ci");
        assert_eq!(route.secret, "abc");
        assert_eq!(route.deliver, "discord");
        assert_eq!(route.deliver_chat, "42");
        assert!(route.deliver_only);
        assert!(route.prompt.contains("{body}")); // default template filled
    }

    #[test]
    fn base_url_display_rules() {
        let mut config = crate::config::UlncLawConfig::default();
        config.gateway.host = "0.0.0.0".into();
        config.gateway.port = 8642;
        assert_eq!(base_url(&config), "http://localhost:8642");
        config.gateway.host = "::".into();
        assert_eq!(base_url(&config), "http://localhost:8642");
        config.gateway.host = "2001:db8::1".into();
        assert_eq!(base_url(&config), "http://[2001:db8::1]:8642");
        config.gateway.host = "example.com".into();
        assert_eq!(base_url(&config), "http://example.com:8642");
    }

    #[test]
    fn signature_header_is_deterministic_hmac() {
        let sig = signature_header("secret", "payload");
        assert!(sig.starts_with("sha256="));
        assert_eq!(sig.len(), 7 + 64);
        assert!(sig[7..].chars().all(|c| c.is_ascii_hexdigit()));
        // Deterministic; secret changes the digest.
        assert_eq!(sig, signature_header("secret", "payload"));
        assert_ne!(sig, signature_header("other", "payload"));
        assert_ne!(sig, signature_header("secret", "payload2"));
    }

    #[test]
    fn created_at_format_is_rfc3339_like() {
        let ts = created_at_now();
        assert_eq!(ts.len(), 20);
        assert!(ts.ends_with('Z'));
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[10..11], "T");
    }

    #[test]
    fn subscribe_rejects_deliver_only_with_log() {
        with_home(|| {
            let opts = SubscribeOptions {
                deliver_only: true,
                ..Default::default()
            };
            let err = cmd_subscribe("hook", &opts).unwrap_err();
            assert!(err.contains("--deliver-only"), "{err}");
        });
    }

    #[test]
    fn subscribe_list_remove_flow() {
        with_home(|| {
            let opts = SubscribeOptions {
                events: Some("push, pr".into()),
                deliver: Some("telegram".into()),
                deliver_chat_id: Some("999".into()),
                prompt: Some("CI: {body}".into()),
                ..Default::default()
            };
            let out = cmd_subscribe("CI Hook", &opts).unwrap();
            assert!(out.contains("Created webhook subscription: ci-hook"));
            assert!(out.contains("/webhooks/ci-hook"));
            assert!(out.contains("Events: push, pr"));

            let listing = cmd_list().unwrap();
            assert!(listing.contains("1 webhook subscription(s)"));
            assert!(listing.contains("◆ ci-hook"));

            // Update path.
            let opts2 = SubscribeOptions {
                deliver: Some("discord".into()),
                ..Default::default()
            };
            let out2 = cmd_subscribe("ci-hook", &opts2).unwrap();
            assert!(out2.contains("Updated webhook subscription"));

            let removed = cmd_remove("ci-hook").unwrap();
            assert!(removed.contains("Removed webhook subscription: ci-hook"));
            assert!(cmd_list()
                .unwrap()
                .contains("No dynamic webhook subscriptions"));
            // Removing static/unknown notes the config-route caveat.
            assert!(cmd_remove("nope").unwrap().contains("Static routes"));
        });
    }

    #[test]
    fn test_command_reports_unknown_subscription() {
        with_home(|| {
            let out = cmd_test("ghost", None).unwrap();
            assert!(out.contains("No subscription named 'ghost'"));
        });
    }
}

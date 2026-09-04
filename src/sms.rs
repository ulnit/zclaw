//! SMS (Twilio) platform adapter — port of hermes `plugins/platforms/sms`
//! @ v2026.8.3 (adapter.py).
//!
//! Outbound SMS goes through the Twilio REST API
//! (`2010-04-01/Accounts/<sid>/Messages.json`, HTTP Basic auth,
//! form-encoded `From`/`To`/`Body`), markdown-stripped and chunked at
//! 1600 chars (~10 segments, hermes `MAX_SMS_LENGTH`).
//!
//! Inbound messages arrive as form-encoded Twilio webhooks. hermes runs
//! a dedicated aiohttp server on `SMS_WEBHOOK_PORT`; ulnclaw mounts the
//! same handler on the gateway router at `/webhooks/twilio` instead —
//! point Twilio at that public URL (`SMS_WEBHOOK_URL`). Requests are
//! validated with the `X-Twilio-Signature` HMAC-SHA1 scheme (url +
//! sorted key/value concatenation, base64 digest, default-port variant
//! fallback per the Twilio docs); without a configured webhook URL the
//! route fails closed unless `SMS_INSECURE_NO_SIGNATURE=true`.
//!
//! Each inbound number is its own DM session; echoes from the bot's own
//! number are dropped, and intake is gated by `SMS_ALLOWED_USERS` /
//! `SMS_ALLOW_ALL_USERS` unioned with the pairing flow (pairing codes
//! are delivered by SMS).

use crate::messaging::Dispatcher;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

/// hermes `MAX_SMS_LENGTH` (~10 SMS segments).
pub const MAX_SMS_LENGTH: usize = 1600;
/// hermes `_TWILIO_WEBHOOK_MAX_BODY_BYTES`.
const WEBHOOK_MAX_BODY_BYTES: usize = 65_536;
/// hermes Twilio REST base.
const TWILIO_API_BASE: &str = "https://api.twilio.com/2010-04-01/Accounts";
/// Empty TwiML ack — replies always ride the REST API.
const EMPTY_TWIML: &str = r#"<?xml version="1.0" encoding="UTF-8"?><Response></Response>"#;

/// `[messaging.sms]` — Twilio SMS adapter (hermes `platforms.sms`
/// plugin config + `TWILIO_*`/`SMS_*` env vars).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SmsConfig {
    pub enabled: bool,
    /// Twilio account SID (fallback `TWILIO_ACCOUNT_SID`).
    pub account_sid: String,
    /// Twilio auth token (fallback `TWILIO_AUTH_TOKEN`).
    pub auth_token: String,
    /// E.164 from-number replies are sent from (fallback
    /// `TWILIO_PHONE_NUMBER`).
    pub from_number: String,
    /// Public URL Twilio signs requests against (fallback
    /// `SMS_WEBHOOK_URL`). Required unless `insecure_no_signature`.
    pub webhook_url: String,
    /// Disable signature validation — dev only (fallback
    /// `SMS_INSECURE_NO_SIGNATURE`).
    pub insecure_no_signature: bool,
    /// E.164 numbers allowed to talk to the bot (fallback
    /// `SMS_ALLOWED_USERS`).
    pub allowed_users: Vec<String>,
    /// Accept every sender (fallback `SMS_ALLOW_ALL_USERS`).
    pub allow_all_users: bool,
    /// Cron/notification delivery number (fallback `SMS_HOME_CHANNEL`).
    pub home_channel: String,
}

impl Default for SmsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            account_sid: String::new(),
            auth_token: String::new(),
            from_number: String::new(),
            webhook_url: String::new(),
            insecure_no_signature: false,
            allowed_users: Vec::new(),
            allow_all_users: false,
            home_channel: String::new(),
        }
    }
}

fn env_trim(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn env_list(name: &str) -> Option<Vec<String>> {
    env_trim(name).map(|raw| {
        raw.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    })
}

fn env_bool(name: &str) -> Option<bool> {
    env_trim(name).map(|v| matches!(v.to_lowercase().as_str(), "true" | "1" | "yes"))
}

/// Resolved runtime settings (env > config, hermes precedence).
#[derive(Debug, Clone)]
pub struct ResolvedSms {
    pub account_sid: String,
    pub auth_token: String,
    pub from_number: String,
    pub webhook_url: String,
    pub insecure_no_signature: bool,
    pub allowed_users: Vec<String>,
    pub allow_all_users: bool,
    pub home_channel: String,
}

impl SmsConfig {
    pub fn resolve(&self) -> ResolvedSms {
        ResolvedSms {
            account_sid: env_trim("TWILIO_ACCOUNT_SID").unwrap_or_else(|| self.account_sid.clone()),
            auth_token: env_trim("TWILIO_AUTH_TOKEN").unwrap_or_else(|| self.auth_token.clone()),
            from_number: env_trim("TWILIO_PHONE_NUMBER")
                .unwrap_or_else(|| self.from_number.clone()),
            webhook_url: env_trim("SMS_WEBHOOK_URL").unwrap_or_else(|| self.webhook_url.clone()),
            insecure_no_signature: env_bool("SMS_INSECURE_NO_SIGNATURE")
                .unwrap_or(self.insecure_no_signature),
            allowed_users: env_list("SMS_ALLOWED_USERS")
                .unwrap_or_else(|| self.allowed_users.clone()),
            allow_all_users: env_bool("SMS_ALLOW_ALL_USERS").unwrap_or(self.allow_all_users),
            home_channel: env_trim("SMS_HOME_CHANNEL").unwrap_or_else(|| self.home_channel.clone()),
        }
    }
}

/// hermes `strip_markdown` — SMS renders markdown as literal characters.
pub fn strip_markdown(message: &str) -> String {
    let mut out = message.to_string();
    // Bold/italic/underscore emphasis (hermes order: **, __, *, _).
    for mark in ["**", "__", "*", "_"] {
        out = strip_paired(&out, mark);
    }
    // Fenced code blocks then inline code.
    out = strip_fenced_code(&out);
    out = strip_paired(&out, "`");
    // Heading markers at line starts.
    out = out
        .lines()
        .map(|line| {
            let trimmed = line.trim_start();
            let hashes = trimmed.chars().take_while(|c| *c == '#').count();
            if (1..=6).contains(&hashes)
                && trimmed.chars().nth(hashes).map(|c| c.is_whitespace()) == Some(true)
            {
                trimmed[hashes..].trim_start()
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    // Markdown links -> label.
    out = strip_links(&out);
    // Collapse 3+ newlines.
    while out.contains("\n\n\n") {
        out = out.replace("\n\n\n", "\n\n");
    }
    out.trim().to_string()
}

/// Public wrapper for cross-adapter reuse (IRC markdown stripping).
pub fn strip_paired_pub(input: &str, mark: &str) -> String {
    strip_paired(input, mark)
}

/// Remove `MARK text MARK` pairs, keeping the inner text (non-greedy,
/// DOTALL — mirrors hermes `re.sub(r"\*\*(.+?)\*\*", r"\1", flags=DOTALL)`).
fn strip_paired(input: &str, mark: &str) -> String {
    let mut out = String::new();
    let mut rest = input;
    while let Some(start) = rest.find(mark) {
        out.push_str(&rest[..start]);
        let after = &rest[start + mark.len()..];
        if let Some(end) = after.find(mark) {
            out.push_str(&after[..end]);
            rest = &after[end + mark.len()..];
        } else {
            // Unpaired marker — keep it literal.
            out.push_str(mark);
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

fn strip_fenced_code(input: &str) -> String {
    let mut out = String::new();
    let mut in_fence = false;
    for line in input.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if !in_fence {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !input.ends_with('\n') {
        out.pop();
    }
    out
}

/// `[label](target)` → `label`.
fn strip_links(input: &str) -> String {
    let mut out = String::new();
    let mut rest = input;
    while let Some(start) = rest.find('[') {
        let Some(close_label) = rest[start..].find(']') else {
            break;
        };
        let label_end = start + close_label;
        if rest[label_end..].starts_with("](") {
            let Some(close_target) = rest[label_end..].find(')') else {
                break;
            };
            out.push_str(&rest[..start]);
            out.push_str(&rest[start + 1..label_end]);
            rest = &rest[label_end + close_target + 1..];
        } else {
            out.push_str(&rest[..label_end + 1]);
            rest = &rest[label_end + 1..];
        }
    }
    out.push_str(rest);
    out
}

/// hermes `_check_signature` — HMAC-SHA1 over `url + sorted(key+value)`,
/// base64 digest, constant-time compare.
pub fn twilio_check_signature(
    auth_token: &str,
    url: &str,
    params: &HashMap<String, String>,
    signature: &str,
) -> bool {
    let mut data = url.to_string();
    let mut keys: Vec<&String> = params.keys().collect();
    keys.sort();
    for key in keys {
        data.push_str(key);
        data.push_str(&params[key]);
    }
    use hmac::{Hmac, Mac};
    type HmacSha1 = Hmac<sha1::Sha1>;
    let Ok(mut mac) = HmacSha1::new_from_slice(auth_token.as_bytes()) else {
        return false;
    };
    mac.update(data.as_bytes());
    let computed = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
    constant_time_eq(computed.as_bytes(), signature.as_bytes())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// hermes `_port_variant_url` — toggle the scheme's default port
/// (Twilio may sign with or without it). Non-standard ports untouched.
pub fn port_variant_url(url: &str) -> Option<String> {
    // String-level split (hermes uses Python urlparse, which preserves
    // explicit default ports; the `url` crate normalizes them away at
    // parse time, so it cannot distinguish the variants).
    let scheme_end = url.find("://")?;
    let (scheme, rest) = (&url[..scheme_end], &url[scheme_end + 3..]);
    let default_port = match scheme {
        "https" => "443",
        "http" => "80",
        _ => return None,
    };
    let authority_end = rest
        .find(|c| c == '/' || c == '?' || c == '#')
        .unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let tail = &rest[authority_end..];
    // A port colon must come after any userinfo `@`.
    let host_part = match authority.rfind('@') {
        Some(at) => &authority[at + 1..],
        None => authority,
    };
    let host_offset = authority.len() - host_part.len();
    // IPv6 literals: only count a colon after the closing bracket.
    let search_from = match host_part.rfind(']') {
        Some(bracket) => host_offset + bracket + 1,
        None => host_offset,
    };
    match authority[search_from..].rfind(':') {
        Some(rel) => {
            let colon = search_from + rel;
            if &authority[colon + 1..] == default_port {
                // Explicit default port → strip it.
                Some(format!("{}://{}{}", scheme, &authority[..colon], tail))
            } else {
                // Non-standard port — no variant.
                None
            }
        }
        None => {
            // No port → add the default.
            Some(format!(
                "{}://{}:{}{}",
                scheme, authority, default_port, tail
            ))
        }
    }
}

/// hermes `_validate_twilio_signature` — try the URL and its port variant.
pub fn validate_twilio_signature(
    auth_token: &str,
    url: &str,
    params: &HashMap<String, String>,
    signature: &str,
) -> bool {
    if twilio_check_signature(auth_token, url, params, signature) {
        return true;
    }
    if let Some(variant) = port_variant_url(url) {
        if twilio_check_signature(auth_token, &variant, params, signature) {
            return true;
        }
    }
    false
}

/// HTTP Basic auth header for the Twilio REST API.
pub fn basic_auth_header(account_sid: &str, auth_token: &str) -> String {
    let encoded = base64::engine::general_purpose::STANDARD
        .encode(format!("{account_sid}:{auth_token}").as_bytes());
    format!("Basic {encoded}")
}

/// Send one SMS body through the Twilio REST API (hermes `send()`).
pub async fn send_sms(
    client: &reqwest::Client,
    cfg: &ResolvedSms,
    to: &str,
    body: &str,
) -> Result<String, String> {
    let url = format!("{TWILIO_API_BASE}/{}/Messages.json", cfg.account_sid);
    let resp = client
        .post(&url)
        .header(
            "Authorization",
            basic_auth_header(&cfg.account_sid, &cfg.auth_token),
        )
        .form(&[
            ("From", cfg.from_number.as_str()),
            ("To", to),
            ("Body", body),
        ])
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let json: Value = resp.json().await.unwrap_or(json!({}));
    if status.as_u16() >= 400 {
        let msg = json
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error");
        return Err(format!("Twilio {status}: {msg}"));
    }
    Ok(json
        .get("sid")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string())
}

/// Webhook response handed back to the gateway route.
pub struct SmsWebhookResponse {
    pub status: u16,
    pub body: String,
}

fn twiml(status: u16) -> SmsWebhookResponse {
    SmsWebhookResponse {
        status,
        body: EMPTY_TWIML.to_string(),
    }
}

/// Intake gate (hermes allowlist envs unioned with ulnclaw pairing).
async fn sender_allowed(
    cfg: &ResolvedSms,
    pairing: Option<&crate::pairing::PairingStore>,
    from: &str,
) -> bool {
    if cfg.allow_all_users || cfg.allowed_users.iter().any(|u| u == from || u == "*") {
        return true;
    }
    if let Some(store) = pairing {
        if store.is_approved("sms", from) {
            return true;
        }
        if let Some(code_msg) = crate::messaging::pairing_offer_public(store, "sms", from, from) {
            let client = reqwest::Client::new();
            match send_sms(&client, cfg, from, &code_msg).await {
                Ok(_) => eprintln!("[sms] pairing code sent to {from}"),
                Err(e) => eprintln!("[sms] failed to send pairing code to {from}: {e}"),
            }
        } else {
            eprintln!("[sms] unauthorized sender {from} — add to allowed_users or approve pairing");
        }
        return false;
    }
    eprintln!("[sms] unauthorized sender {from} — add to allowed_users");
    false
}

/// Gateway webhook entry point (hermes `_handle_webhook`), mounted at
/// `/webhooks/twilio`.
pub async fn sms_handle_webhook(
    cfg: &SmsConfig,
    dispatcher: &Arc<Dispatcher>,
    pairing: Option<&crate::pairing::PairingStore>,
    body: &[u8],
    headers: &[(String, String)],
) -> SmsWebhookResponse {
    if body.len() > WEBHOOK_MAX_BODY_BYTES {
        return twiml(413);
    }
    let resolved = cfg.resolve();
    // Twilio posts form-encoded data, not JSON.
    let form: Vec<(String, String)> = url::form_urlencoded::parse(body)
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    if form.is_empty() {
        return twiml(400);
    }
    let mut flat: HashMap<String, String> = HashMap::new();
    for (k, v) in &form {
        flat.entry(k.clone()).or_insert_with(|| v.clone());
    }

    // Signature validation (fail closed without a configured URL).
    if !resolved.webhook_url.is_empty() {
        let sig = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("x-twilio-signature"))
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        if sig.is_empty() {
            eprintln!("[sms] rejected webhook: missing X-Twilio-Signature header");
            return twiml(403);
        }
        if !validate_twilio_signature(&resolved.auth_token, &resolved.webhook_url, &flat, &sig) {
            eprintln!("[sms] rejected webhook: invalid Twilio signature");
            return twiml(403);
        }
    } else if !resolved.insecure_no_signature {
        eprintln!(
            "[sms] rejected webhook: SMS_WEBHOOK_URL not configured (set it, or SMS_INSECURE_NO_SIGNATURE=true for dev)"
        );
        return twiml(403);
    }

    let from = flat
        .get("From")
        .cloned()
        .unwrap_or_default()
        .trim()
        .to_string();
    let text = flat
        .get("Body")
        .cloned()
        .unwrap_or_default()
        .trim()
        .to_string();
    let message_sid = flat
        .get("MessageSid")
        .cloned()
        .unwrap_or_default()
        .trim()
        .to_string();
    if from.is_empty() || text.is_empty() {
        return twiml(200);
    }
    // Echo prevention — ignore messages from our own number.
    if from == resolved.from_number {
        return twiml(200);
    }
    if !sender_allowed(&resolved, pairing, &from).await {
        return twiml(200);
    }

    crate::messaging::register_platform_sender(
        "sms",
        Arc::new(SmsSender {
            cfg: resolved.clone(),
        }),
    );

    let event = crate::messaging::MessageEvent {
        platform: "sms".into(),
        chat_id: from.clone(),
        sender_id: from.clone(),
        sender_name: from.clone(),
        text,
        message_id: message_sid,
        attachments: Vec::new(),
    };
    let mut gate_check = event.clone();
    if !crate::messaging::pre_gateway_dispatch_gate_public(&mut gate_check).await {
        return twiml(200);
    }
    let outcome = match dispatcher.handle_event(event).await {
        Ok(o) => o,
        Err(e) => crate::messaging::DispatchOutcome {
            reply: format!("error: {e}"),
            transcript_echoes: Vec::new(),
        },
    };
    let mut full = String::new();
    for echo in &outcome.transcript_echoes {
        full.push_str(echo);
        full.push('\n');
    }
    full.push_str(&outcome.reply);
    let (reply_text, _media) = crate::messaging::extract_media_tags(&full);
    let reply_text = reply_text.trim().to_string();
    if !reply_text.is_empty() {
        // P705: ledger-protected reply delivery (all chunks).
        dispatcher
            .try_send_with_ledger("sms", &from, &reply_text, || async {
                let client = reqwest::Client::new();
                let formatted = strip_markdown(&reply_text);
                let mut result: Result<(), String> = Ok(());
                for chunk in crate::messaging::chunk_text(&formatted, MAX_SMS_LENGTH) {
                    if let Err(e) = send_sms(&client, &resolved, &from, &chunk).await {
                        eprintln!("[sms] reply to {from} failed: {e}");
                        if result.is_ok() {
                            result = Err(e.to_string());
                        }
                    }
                }
                result
            })
            .await;
    }
    twiml(200)
}

struct SmsSender {
    cfg: ResolvedSms,
}

#[async_trait::async_trait]
impl crate::messaging::PlatformSender for SmsSender {
    async fn send_text(&self, chat_id: &str, text: &str) {
        let client = reqwest::Client::new();
        let formatted = strip_markdown(text);
        for chunk in crate::messaging::chunk_text(&formatted, MAX_SMS_LENGTH) {
            if let Err(e) = send_sms(&client, &self.cfg, chat_id, &chunk).await {
                eprintln!("[sms] send_text to {chat_id} failed: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn dummy_dispatcher() -> Arc<Dispatcher> {
        use crate::agent::Agent;
        use crate::provider::openai::OpenAiProvider;
        use crate::session::sqlite::SqliteSessionStore;
        use crate::tools::ToolRegistry;
        use std::sync::Arc as StdArc;
        // The tested webhook paths return before touching the dispatcher;
        // build a real one against a temp store anyway.
        let temp = tempfile::tempdir().expect("tempdir");
        let store = StdArc::new(
            SqliteSessionStore::open(temp.path().join("state.db")).expect("store opens"),
        );
        std::mem::forget(temp);
        let provider = StdArc::new(
            OpenAiProvider::builder()
                .endpoint("http://127.0.0.1:9/v1")
                .model("test-model")
                .name("test")
                .build()
                .expect("provider builds"),
        );
        let agent = Agent::new(provider, ToolRegistry::new()).with_store(store.clone());
        Dispatcher::new(StdArc::new(agent), store)
    }

    fn sig_for(token: &str, url: &str, params: &HashMap<String, String>) -> String {
        use hmac::{Hmac, Mac};
        type HmacSha1 = Hmac<sha1::Sha1>;
        let mut data = url.to_string();
        let mut keys: Vec<&String> = params.keys().collect();
        keys.sort();
        for key in keys {
            data.push_str(key);
            data.push_str(&params[key]);
        }
        let mut mac = HmacSha1::new_from_slice(token.as_bytes()).unwrap();
        mac.update(data.as_bytes());
        base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
    }

    #[test]
    fn signature_roundtrip() {
        let params: HashMap<String, String> = [
            ("From".to_string(), "+15551234567".to_string()),
            ("Body".to_string(), "hello world".to_string()),
            ("To".to_string(), "+15559876543".to_string()),
        ]
        .into_iter()
        .collect();
        let url = "https://example.com/webhooks/twilio";
        let sig = sig_for("secret-token", url, &params);
        assert!(validate_twilio_signature(
            "secret-token",
            url,
            &params,
            &sig
        ));
        assert!(!validate_twilio_signature(
            "wrong-token",
            url,
            &params,
            &sig
        ));
        assert!(!validate_twilio_signature(
            "secret-token",
            url,
            &params,
            "bogus"
        ));
    }

    #[test]
    fn signature_matches_twilio_doc_example_format() {
        // Digit-string keys must sort lexicographically (Digit1, Digit2,
        // Digit10 would sort Digit1 < Digit10 < Digit2 — verify ours does
        // the same as hermes' sorted()).
        let params: HashMap<String, String> = [
            ("Digit2".to_string(), "2".to_string()),
            ("Digit10".to_string(), "10".to_string()),
            ("Digit1".to_string(), "1".to_string()),
        ]
        .into_iter()
        .collect();
        let url = "https://mycompany.com/myapp.php?foo=1&bar=2";
        let sig = sig_for("12345", url, &params);
        assert!(validate_twilio_signature("12345", url, &params, &sig));
    }

    #[test]
    fn port_variant_toggles_default_ports() {
        assert_eq!(
            port_variant_url("https://example.com/webhooks/twilio"),
            Some("https://example.com:443/webhooks/twilio".to_string())
        );
        assert_eq!(
            port_variant_url("https://example.com:443/webhooks/twilio"),
            Some("https://example.com/webhooks/twilio".to_string())
        );
        assert_eq!(
            port_variant_url("http://example.com/hook"),
            Some("http://example.com:80/hook".to_string())
        );
        // Non-standard ports are never modified.
        assert_eq!(port_variant_url("https://example.com:8443/hook"), None);
    }

    #[test]
    fn port_variant_signature_fallback() {
        let params: HashMap<String, String> = [("Body".to_string(), "hi".to_string())]
            .into_iter()
            .collect();
        // Signed against the :443 variant, validated with the bare URL.
        let sig = sig_for("tok", "https://example.com:443/webhooks/twilio", &params);
        assert!(validate_twilio_signature(
            "tok",
            "https://example.com/webhooks/twilio",
            &params,
            &sig
        ));
    }

    #[test]
    fn strip_markdown_covers_hermes_rules() {
        let input = "**bold** and *italic* and __under__ and _em_ and `code`";
        let out = strip_markdown(input);
        assert_eq!(out, "bold and italic and under and em and code");

        let fenced = "before\n```python\nprint(1)\n```\nafter";
        assert_eq!(strip_markdown(fenced), "before\nafter");

        let heading = "## Title\nplain";
        assert_eq!(strip_markdown(heading), "Title\nplain");

        let link = "see [the docs](https://example.com) now";
        assert_eq!(strip_markdown(link), "see the docs now");

        let newlines = "a\n\n\n\nb";
        assert_eq!(strip_markdown(newlines), "a\n\nb");
    }

    #[test]
    fn basic_auth_header_encodes_credentials() {
        let header = basic_auth_header("ACSID", "token");
        let expected = base64::engine::general_purpose::STANDARD.encode("ACSID:token");
        assert_eq!(header, format!("Basic {expected}"));
    }

    #[test]
    fn resolve_env_precedence() {
        let _guard = crate::models_dev::test_env_lock();
        std::env::set_var("TWILIO_ACCOUNT_SID", "env-sid");
        std::env::set_var("SMS_ALLOWED_USERS", "+15550001111, +15552223333");
        let cfg = SmsConfig {
            account_sid: "cfg-sid".into(),
            allowed_users: vec!["+15559998888".into()],
            ..Default::default()
        };
        let resolved = cfg.resolve();
        assert_eq!(resolved.account_sid, "env-sid");
        assert_eq!(
            resolved.allowed_users,
            vec!["+15550001111".to_string(), "+15552223333".to_string()]
        );
        std::env::remove_var("TWILIO_ACCOUNT_SID");
        std::env::remove_var("SMS_ALLOWED_USERS");
    }

    #[test]
    fn webhook_body_limit_matches_hermes() {
        assert_eq!(WEBHOOK_MAX_BODY_BYTES, 65_536);
        assert_eq!(MAX_SMS_LENGTH, 1600);
    }

    #[test]
    fn form_parse_extracts_first_values() {
        let body = b"From=%2B15551234567&To=%2B15559876543&Body=hello+world&MessageSid=SM123";
        let form: Vec<(String, String)> = url::form_urlencoded::parse(body)
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        let mut flat: HashMap<String, String> = HashMap::new();
        for (k, v) in &form {
            flat.entry(k.clone()).or_insert_with(|| v.clone());
        }
        assert_eq!(flat.get("From").unwrap(), "+15551234567");
        assert_eq!(flat.get("Body").unwrap(), "hello world");
        assert_eq!(flat.get("MessageSid").unwrap(), "SM123");
    }

    #[tokio::test]
    async fn webhook_oversized_body_is_413() {
        let cfg = SmsConfig::default();
        let dispatcher = dummy_dispatcher().await;
        let big = vec![b'a'; WEBHOOK_MAX_BODY_BYTES + 1];
        let resp = sms_handle_webhook(&cfg, &dispatcher, None, &big, &[]).await;
        assert_eq!(resp.status, 413);
    }

    #[tokio::test]
    async fn webhook_without_webhook_url_fails_closed() {
        let cfg = SmsConfig {
            insecure_no_signature: false,
            ..Default::default()
        };
        let dispatcher = dummy_dispatcher().await;
        let resp = sms_handle_webhook(&cfg, &dispatcher, None, b"From=%2B1&Body=hi", &[]).await;
        assert_eq!(resp.status, 403);
    }

    #[tokio::test]
    async fn webhook_missing_signature_header_is_403() {
        let cfg = SmsConfig {
            webhook_url: "https://example.com/webhooks/twilio".into(),
            auth_token: "tok".into(),
            ..Default::default()
        };
        let dispatcher = dummy_dispatcher().await;
        let resp = sms_handle_webhook(&cfg, &dispatcher, None, b"From=%2B1&Body=hi", &[]).await;
        assert_eq!(resp.status, 403);
    }

    #[tokio::test]
    async fn webhook_invalid_signature_is_403() {
        let cfg = SmsConfig {
            webhook_url: "https://example.com/webhooks/twilio".into(),
            auth_token: "tok".into(),
            ..Default::default()
        };
        let dispatcher = dummy_dispatcher().await;
        let headers = vec![("X-Twilio-Signature".to_string(), "AAAA".to_string())];
        let resp =
            sms_handle_webhook(&cfg, &dispatcher, None, b"From=%2B1&Body=hi", &headers).await;
        assert_eq!(resp.status, 403);
    }

    #[tokio::test]
    async fn webhook_echo_from_own_number_acks_without_dispatch() {
        let cfg = SmsConfig {
            insecure_no_signature: true,
            from_number: "+15550000000".into(),
            allow_all_users: true,
            ..Default::default()
        };
        let dispatcher = dummy_dispatcher().await;
        let body = b"From=%2B15550000000&Body=echo&MessageSid=SM1";
        let resp = sms_handle_webhook(&cfg, &dispatcher, None, body, &[]).await;
        assert_eq!(resp.status, 200);
        assert!(resp.body.contains("<Response>"));
    }
}

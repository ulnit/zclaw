//! `ulnclaw whatsapp-cloud` — WhatsApp Business Cloud API setup wizard
//! (lean port of hermes `hermes_cli/setup_whatsapp_cloud.py`).
//!
//! Walks the Meta-side credentials (Phone Number ID, Access Token, App
//! Secret), auto-generates the webhook verify token, captures the
//! recipient allowlist, and prints the follow-up steps that happen
//! outside the wizard (tunnel, gateway, Meta webhook dashboard).
//!
//! Differences versus hermes (documented): credentials persist to
//! `[messaging.whatsapp_cloud]` in config.toml (ulnclaw's platform
//! resolution) instead of `.env`; the analytics-only App ID / WABA ID
//! step is omitted (no consumer in ulnclaw); follow-up instructions use
//! ulnclaw's `/webhooks/whatsapp` route and gateway port.

use crate::config_cmd;

// ---------------------------------------------------------------------------
// Field-shape validators (hermes `_validate_*` parity)
// ---------------------------------------------------------------------------

/// Phone Number ID: Meta's 15-17 digit internal ID — NOT a phone number.
pub fn validate_phone_number_id(value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err("Phone Number ID is required".into());
    }
    let s = value.trim();
    if !s.chars().all(|c| c.is_ascii_digit()) {
        return Err("Phone Number ID must be numeric (no '+', spaces, or dashes)".into());
    }
    if (10..=12).contains(&s.len()) {
        return Err(
            "That looks like a phone number — but this field needs the Phone Number ID \
             (Meta's internal ID, 15-17 digits, e.g. '7794189252778687'). Look just BELOW \
             the 'From' dropdown in API Setup → it's labelled 'Phone number ID'."
                .into(),
        );
    }
    if s.len() < 13 {
        return Err("Phone Number ID looks too short (expected 13-18 digits)".into());
    }
    if s.len() > 20 {
        return Err("Phone Number ID looks too long (expected 13-18 digits)".into());
    }
    Ok(())
}

/// Access token: starts with `EAA`, 100+ chars; diagnoses common paste
/// mistakes (OpenAI/Slack/GitHub tokens).
pub fn validate_access_token(value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err("Access token is required".into());
    }
    let s = value.trim();
    if !s.starts_with("EAA") {
        if s.starts_with("sk-") {
            return Err(
                "That's an OpenAI key (starts with 'sk-'), not a Meta WhatsApp access \
                 token. Meta tokens start with 'EAA'."
                    .into(),
            );
        }
        if s.starts_with("xoxb-") || s.starts_with("xoxp-") {
            return Err(
                "That's a Slack token, not a Meta WhatsApp access token. Meta tokens \
                 start with 'EAA'."
                    .into(),
            );
        }
        if s.starts_with("ghp_") || s.starts_with("gho_") {
            return Err(
                "That's a GitHub token, not a Meta WhatsApp access token. Meta tokens \
                 start with 'EAA'."
                    .into(),
            );
        }
        return Err(
            "Meta WhatsApp access tokens start with 'EAA'. Check that you're copying \
             from the right place (API Setup → 'Generate access token', or Business \
             Settings → System Users → 'Generate token' for a permanent one)."
                .into(),
        );
    }
    if s.len() < 100 {
        return Err(format!(
            "Access token looks too short ({} chars, expected 100+)",
            s.len()
        ));
    }
    Ok(())
}

/// App Secret: exactly 32 lowercase hex chars.
pub fn validate_app_secret(value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err("App Secret is required".into());
    }
    let s = value.trim();
    if !s.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(
            "App Secret should be a hex string (only digits 0-9 and letters a-f). Make \
             sure you copied the 'App secret' from Settings → Basic, not some other token."
                .into(),
        );
    }
    if s.len() != 32 {
        return Err(format!(
            "App Secret should be exactly 32 hex characters (got {})",
            s.len()
        ));
    }
    Ok(())
}

/// Recipient allowlist normalization: strip +/spaces/dashes per entry,
/// drop empties, rejoin with commas.
pub fn normalize_allowlist(raw: &str) -> String {
    raw.split(',')
        .map(|part| {
            part.chars()
                .filter(|c| !matches!(c, ' ' | '\t' | '-' | '+'))
                .collect::<String>()
        })
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(",")
}

/// Auto-generated webhook verify token (hermes `secrets.token_urlsafe(32)`).
pub fn mint_verify_token() -> String {
    crate::webhook_subscriptions::mint_secret()
}

// ---------------------------------------------------------------------------
// Config persistence (ulnclaw: `[messaging.whatsapp_cloud]` in config.toml)
// ---------------------------------------------------------------------------

fn current(field: &str) -> String {
    let key = format!("messaging.whatsapp_cloud.{field}");
    config_cmd::get_config_value(&key, false)
        .ok()
        .map(|v| v.trim().trim_matches('"').to_string())
        .filter(|v| !v.is_empty() && v != "not set")
        .unwrap_or_default()
}

fn write(field: &str, value: &str) -> Result<(), String> {
    let key = format!("messaging.whatsapp_cloud.{field}");
    config_cmd::set_config_value(&key, value, true).map(|_| ())
}

// ---------------------------------------------------------------------------
// Wizard
// ---------------------------------------------------------------------------

/// Entry point for `ulnclaw whatsapp-cloud`. Returns the process exit
/// code with hermes semantics: 0 full success, 1 user abort, 2 partial.
pub fn run_wizard() -> Result<i32, String> {
    use crate::setup_cmd;
    if !setup_cmd::is_interactive_stdin() {
        return Err(
            "`ulnclaw whatsapp-cloud` needs an interactive TTY. Configure directly:\n  \
             ulnclaw config set messaging.whatsapp_cloud.enabled true\n  \
             ulnclaw config set messaging.whatsapp_cloud.access_token EAA...\n  \
             ulnclaw config set messaging.whatsapp_cloud.phone_number_id <id>"
                .to_string(),
        );
    }

    println!();
    println!("  ─── WhatsApp Business Cloud API Setup ───");
    println!();
    println!("  This wizard configures ulnclaw to talk to WhatsApp via Meta's");
    println!("  official Cloud API — the production-grade path:");
    println!();
    println!("    • No QR codes, no Node.js bridge subprocess");
    println!("    • Stable connection — no account-ban risk");
    println!("    • Business account required (not personal WhatsApp)");
    println!("    • Public webhook URL required (Cloudflare Tunnel, ngrok,");
    println!("      or your own reverse proxy with TLS)");
    println!();
    println!("  If you don't have a Meta app yet:");
    println!("    1. https://developers.facebook.com/apps → Create App →");
    println!("       'Connect with customers through WhatsApp'");
    println!("    2. App Dashboard → WhatsApp → API Setup");
    println!("    3. 'Generate access token' (temp 24h token is fine to start)");
    println!();
    if !setup_cmd::prompt_yes_no("Continue?", true)? {
        println!("  Setup cancelled.");
        return Ok(1);
    }

    let mut wrote_any = false;

    // STEP 1 — Phone Number ID (required)
    println!();
    println!("  ── STEP 1 — Phone Number ID ──");
    println!("  Found in: App Dashboard → WhatsApp → API Setup, in the");
    println!("  'Send and receive messages' section — BELOW the 'From' dropdown.");
    println!("  It is NOT the phone number itself. 15-17 digits.");
    let current_phone = current("phone_number_id");
    match prompt_validated(
        "Phone Number ID",
        &current_phone,
        false,
        validate_phone_number_id,
    )? {
        Some(v) => {
            write("phone_number_id", &v)?;
            wrote_any = true;
            println!("  ✓ Saved: {v}");
        }
        None if !current_phone.is_empty() => println!("  ✓ Keeping existing: {current_phone}"),
        None => {
            println!("  ✗ Phone Number ID is required. Aborting.");
            return Ok(if wrote_any { 2 } else { 1 });
        }
    }

    // STEP 2 — Access Token (required)
    println!();
    println!("  ── STEP 2 — Access Token ──");
    println!("  Temp token: API Setup → 'Generate access token' (lasts 24h).");
    println!("  Permanent: Business Settings → System users → Generate token");
    println!("  (business_management, whatsapp_business_messaging,");
    println!("  whatsapp_business_management). Tokens start with 'EAA'.");
    let current_token = current("access_token");
    let display = mask_preview(&current_token, 15);
    match prompt_validated("Access Token", &display, true, validate_access_token)? {
        Some(v) => {
            write("access_token", &v)?;
            println!("  ✓ Saved (token hidden)");
        }
        None if !current_token.is_empty() => println!("  ✓ Keeping existing token"),
        None => {
            println!("  ✗ Access Token is required. Aborting.");
            return Ok(if wrote_any { 2 } else { 1 });
        }
    }

    // STEP 3 — App Secret (strongly recommended)
    println!();
    println!("  ── STEP 3 — App Secret (webhook signature verification) ──");
    println!("  Found in: App Dashboard → Settings → Basic → 'App secret'.");
    println!("  Without it, inbound webhook POSTs are refused (unverifiable).");
    let current_secret = current("app_secret");
    let display = mask_preview(&current_secret, 8);
    match prompt_validated("App Secret", &display, true, validate_app_secret)? {
        Some(v) => {
            write("app_secret", &v)?;
            println!("  ✓ Saved (secret hidden)");
        }
        None if !current_secret.is_empty() => println!("  ✓ Keeping existing App Secret"),
        None => {
            println!("  ⚠ Skipping App Secret — inbound webhooks will be refused");
            println!("    until you set messaging.whatsapp_cloud.app_secret.");
        }
    }

    // STEP 4 — Verify Token (auto-generated)
    println!();
    println!("  ── STEP 4 — Verify Token (auto-generated) ──");
    let current_verify = current("verify_token");
    let verify_token = if !current_verify.is_empty() {
        println!(
            "  An existing verify token is already set ({}...).",
            mask_preview(&current_verify, 8)
        );
        if setup_cmd::prompt_yes_no("Generate a new one?", false)? {
            let t = mint_verify_token();
            write("verify_token", &t)?;
            println!("  ✓ New verify token: {t}");
            t
        } else {
            println!("  ✓ Keeping existing verify token");
            current_verify
        }
    } else {
        let t = mint_verify_token();
        write("verify_token", &t)?;
        println!("  ✓ Generated: {t}");
        t
    };
    println!();
    println!("  → COPY THIS TOKEN NOW. You'll paste it into Meta's webhook");
    println!("    configuration dialog (step 5 below).");

    // STEP 5 — Recipient Allowlist
    println!();
    println!("  ── STEP 5 — Recipient Allowlist ──");
    println!("  Who may message the bot? Comma-separated phone numbers with");
    println!("  country code (no +/spaces/dashes). Blank = pairing-only (fail");
    println!("  closed until someone pairs via `ulnclaw pairing`).");
    let current_allow = current_allowlist();
    let allowed = setup_cmd::prompt_line("Allowed users", &current_allow)?;
    let allowed = normalize_allowlist(&allowed);
    if allowed.is_empty() {
        println!("  ⚠ No allowlist — pairing-only mode (unknown senders get a pair code).");
    } else {
        write_allowlist(&allowed)?;
        println!("  ✓ Saved: {allowed}");
    }

    // Enable the platform.
    config_cmd::set_config_value("messaging.whatsapp_cloud.enabled", "true", true).map(|_| ())?;
    println!("  ✓ messaging.whatsapp_cloud.enabled = true");

    // Final follow-up block (ulnclaw paths).
    let config = crate::config::UlncLawConfig::load(None).unwrap_or_default();
    let port = config.gateway.port;
    println!();
    println!("  ── SETUP COMPLETE — Next steps ──");
    println!();
    println!("  ulnclaw needs a public HTTPS URL to receive WhatsApp messages.");
    println!("  Recommended: Cloudflare Tunnel (free, no port forwarding).");
    println!();
    println!("    1. Install cloudflared (one-time):");
    println!("         macOS:  brew install cloudflared");
    println!("         Linux:  https://github.com/cloudflare/cloudflared/releases");
    println!();
    println!("    2. Start the tunnel in a separate terminal:");
    println!("         cloudflared tunnel --url http://localhost:{port}");
    println!();
    println!("    3. Start the gateway in another terminal:  ulnclaw gateway");
    println!();
    println!("    4. Verify from a third terminal (substitute your tunnel URL):");
    println!("         curl 'https://YOUR-TUNNEL.trycloudflare.com/webhooks/whatsapp?\\");
    println!("               hub.mode=subscribe&hub.verify_token={verify_token}&\\");
    println!("               hub.challenge=hello'");
    println!("       Expected: HTTP 200 with body 'hello'.");
    println!();
    println!("    5. Point Meta at your tunnel:");
    println!("         App Dashboard → WhatsApp → Configuration → Edit webhook");
    println!("         Callback URL: <tunnel-url>/webhooks/whatsapp");
    println!("         Verify Token: {verify_token}");
    println!("         → 'Verify and save' → subscribe to the 'messages' field");
    println!();
    println!("    6. Add your phone to Meta's recipient list (API Setup → 'To'");
    println!("       → 'Manage phone number list'), then DM the bot's number.");
    println!();
    Ok(0)
}

fn current_allowlist() -> String {
    let key = "messaging.whatsapp_cloud.allowed_sender_ids";
    config_cmd::get_config_value(key, true)
        .ok()
        .map(|v| {
            v.trim()
                .trim_start_matches('[')
                .trim_end_matches(']')
                .replace('"', "")
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(",")
        })
        .filter(|v| !v.is_empty())
        .unwrap_or_default()
}

fn write_allowlist(csv: &str) -> Result<(), String> {
    let items: Vec<String> = csv
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let path = config_cmd::config_path();
    let mut doc = config_cmd::load_toml(&path)?;
    let arr = toml::Value::Array(items.into_iter().map(toml::Value::String).collect());
    config_cmd::set_nested(&mut doc, "messaging.whatsapp_cloud.allowed_sender_ids", arr)?;
    config_cmd::save_toml(&path, &doc)
}

/// Repeat-a-validated-prompt (hermes `_prompt_validated`): up to 3
/// attempts, then offer skip. Returns None when the user gives up.
fn prompt_validated(
    message: &str,
    current_display: &str,
    secret: bool,
    validate: fn(&str) -> Result<(), String>,
) -> Result<Option<String>, String> {
    use crate::setup_cmd;
    let mut attempts = 0;
    loop {
        attempts += 1;
        let prompt = if current_display.is_empty() {
            message.to_string()
        } else {
            format!("{message} [{current_display}]")
        };
        let value = if secret {
            setup_cmd::prompt_hidden(&prompt)?
        } else {
            setup_cmd::prompt_line(&prompt, "")?
        };
        if value.trim().is_empty() {
            return Ok(None);
        }
        match validate(&value) {
            Ok(()) => return Ok(Some(value.trim().to_string())),
            Err(reason) => {
                println!("    ✗ {reason}");
                if attempts >= 3 {
                    if !setup_cmd::prompt_yes_no("Try again?", false)? {
                        return Ok(None);
                    }
                    attempts = 0;
                }
            }
        }
    }
}

/// Masked preview of an existing value (`abc12345...`), empty when unset.
pub fn mask_preview(value: &str, chars: usize) -> String {
    if value.is_empty() {
        return String::new();
    }
    let take: String = value.chars().take(chars).collect();
    format!("{take}...")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phone_number_id_validator_catches_phone_numbers() {
        assert!(validate_phone_number_id("7794189252778687").is_ok());
        let err = validate_phone_number_id("15556422442").unwrap_err();
        assert!(err.contains("looks like a phone number"), "{err}");
        assert!(validate_phone_number_id("+1 555").is_err());
        assert!(validate_phone_number_id("").is_err());
        assert!(validate_phone_number_id("12345").is_err());
        assert!(validate_phone_number_id(&"9".repeat(21)).is_err());
    }

    #[test]
    fn access_token_validator_diagnoses_foreign_tokens() {
        let long_eaa = format!("EAA{}", "x".repeat(120));
        assert!(validate_access_token(&long_eaa).is_ok());
        assert!(validate_access_token("EAAshort")
            .unwrap_err()
            .contains("too short"));
        assert!(validate_access_token("sk-abc")
            .unwrap_err()
            .contains("OpenAI"));
        assert!(validate_access_token("xoxb-abc")
            .unwrap_err()
            .contains("Slack"));
        assert!(validate_access_token("ghp_abc")
            .unwrap_err()
            .contains("GitHub"));
        assert!(validate_access_token("random")
            .unwrap_err()
            .contains("start with 'EAA'"));
    }

    #[test]
    fn app_secret_validator_requires_32_hex() {
        assert!(validate_app_secret(&"a1".repeat(16)).is_ok());
        assert!(validate_app_secret("zz").is_err());
        assert!(validate_app_secret("abc").unwrap_err().contains("32 hex"));
        assert!(validate_app_secret("").is_err());
    }

    #[test]
    fn allowlist_normalization_strips_formatting() {
        assert_eq!(
            normalize_allowlist("+1 555-123-4567, +44 20 7946 0958"),
            "15551234567,442079460958"
        );
        assert_eq!(normalize_allowlist(" , ,"), "");
        assert_eq!(normalize_allowlist("*"), "*");
    }

    #[test]
    fn verify_token_is_minted_like_route_secrets() {
        let t = mint_verify_token();
        assert_eq!(t.len(), 64);
        assert_ne!(t, mint_verify_token());
    }

    #[test]
    fn mask_preview_truncates_and_hides() {
        assert_eq!(mask_preview("", 8), "");
        assert_eq!(mask_preview("abcdefghij", 4), "abcd...");
        assert_eq!(mask_preview("ab", 8), "ab...");
    }
}

//! Sandbox environment scrubbing + passthrough — port of the credential
//! scrubbing in hermes' `tools/environments/local.py` and the allowlist
//! registry in `tools/env_passthrough.py`.
//!
//! Terminal and execute_code child processes run with the agent
//! provider/tool credentials stripped out of their environment. Two
//! sources can put a variable back on the allowlist:
//!
//! 1. **Skill declarations** — when a skill is loaded via `skill_view`,
//!    its `required_environment_variables` are registered (provider
//!    credentials are refused; see below).
//! 2. **User config** — `[terminal] env_passthrough` explicitly
//!    allowlists variables for non-skill use cases.
//!
//! Security note (hermes GHSA-rhgp-j443-p4rf): skill-declared
//! passthrough must NOT be able to override the provider-credential
//! blocklist — a malicious skill registering `ANTHROPIC_TOKEN` /
//! `OPENAI_API_KEY` would otherwise receive the credential in the
//! sandboxed child process, defeating the scrubbing guarantee. Fail
//! closed: dynamically-named internal secrets are refused too.

use std::collections::HashSet;
use std::sync::OnceLock;

/// Provider/tool credentials and platform settings that never enter a
/// sandboxed child environment (port of hermes
/// `_HERMES_PROVIDER_ENV_BLOCKLIST`, adapted for ulnclaw).
pub fn provider_env_blocklist() -> &'static HashSet<&'static str> {
    static LIST: OnceLock<HashSet<&'static str>> = OnceLock::new();
    LIST.get_or_init(|| {
        HashSet::from([
            // LLM provider credentials
            "OPENAI_API_KEY",
            "OPENAI_BASE_URL",
            "OPENAI_API_BASE",
            "OPENAI_ORG_ID",
            "OPENAI_ORGANIZATION",
            "OPENROUTER_API_KEY",
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_BASE_URL",
            "ANTHROPIC_TOKEN",
            "GOOGLE_API_KEY",
            "VERTEX_CREDENTIALS_PATH",
            "GOOGLE_APPLICATION_CREDENTIALS",
            "DEEPSEEK_API_KEY",
            "MISTRAL_API_KEY",
            "GROQ_API_KEY",
            "TOGETHER_API_KEY",
            "PERPLEXITY_API_KEY",
            "COHERE_API_KEY",
            "FIREWORKS_API_KEY",
            "XAI_API_KEY",
            "HELICONE_API_KEY",
            "PARALLEL_API_KEY",
            "OLLAMA_BASE_URL",
            // ulnclaw-managed keys
            "ULNCLAW_API_KEY",
            "ULNCLAW_GATEWAY_KEY",
            "ULNCLAW_TTS_KEY",
            // Tool backends
            "TAVILY_API_KEY",
            "BRAVE_API_KEY",
            "FIRECRAWL_API_KEY",
            "FIRECRAWL_API_URL",
            "HASS_TOKEN",
            "HASS_URL",
            // Messaging platforms (parity with hermes)
            "TELEGRAM_HOME_CHANNEL",
            "TELEGRAM_HOME_CHANNEL_NAME",
            "DISCORD_HOME_CHANNEL",
            "DISCORD_HOME_CHANNEL_NAME",
            "DISCORD_REQUIRE_MENTION",
            "DISCORD_FREE_RESPONSE_CHANNELS",
            "DISCORD_AUTO_THREAD",
            "SLACK_HOME_CHANNEL",
            "SLACK_HOME_CHANNEL_NAME",
            "SLACK_ALLOWED_USERS",
            "WHATSAPP_ENABLED",
            "WHATSAPP_MODE",
            "WHATSAPP_ALLOWED_USERS",
            "SIGNAL_HTTP_URL",
            "SIGNAL_ACCOUNT",
            "SIGNAL_ALLOWED_USERS",
            "SIGNAL_GROUP_ALLOWED_USERS",
            "SIGNAL_HOME_CHANNEL",
            "SIGNAL_HOME_CHANNEL_NAME",
            "SIGNAL_IGNORE_STORIES",
            // Email / infra credentials
            "EMAIL_ADDRESS",
            "EMAIL_PASSWORD",
            "EMAIL_IMAP_HOST",
            "EMAIL_SMTP_HOST",
            "EMAIL_HOME_ADDRESS",
            "EMAIL_HOME_ADDRESS_NAME",
            "GH_TOKEN",
            "GITHUB_APP_ID",
            "GITHUB_APP_PRIVATE_KEY_PATH",
            "GITHUB_APP_INSTALLATION_ID",
            "MODAL_TOKEN_ID",
            "MODAL_TOKEN_SECRET",
            "DAYTONA_API_KEY",
            "GATEWAY_RELAY_ID",
            "GATEWAY_RELAY_SECRET",
            "GATEWAY_RELAY_DELIVERY_KEY",
            "VERCEL_OIDC_TOKEN",
            "VERCEL_TOKEN",
            "VERCEL_PROJECT_ID",
            "VERCEL_TEAM_ID",
            "GATEWAY_ALLOWED_USERS",
        ])
    })
}

/// True for internal secrets injected under *dynamic* names no static
/// blocklist can enumerate (port of hermes `_is_hermes_internal_secret`):
/// `AUXILIARY_<TASK>_API_KEY` / `AUXILIARY_<TASK>_BASE_URL` side-LLM
/// credentials and `GATEWAY_RELAY_*` relay auth.
pub fn is_internal_secret(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    if upper.starts_with("AUXILIARY_")
        && (upper.ends_with("_API_KEY") || upper.ends_with("_BASE_URL"))
    {
        return true;
    }
    upper.starts_with("GATEWAY_RELAY_")
}

/// True if `name` is a protected credential that skills must never be
/// able to register as passthrough. Fails closed (hermes GHSA fix).
pub fn is_protected_credential(name: &str) -> bool {
    is_internal_secret(name)
        || provider_env_blocklist().contains(name.to_ascii_uppercase().as_str())
}

/// Register env var names as allowed in sandboxed environments.
///
/// Returns the names actually accepted; protected provider credentials
/// are refused (with a warning) to preserve the sandbox's
/// credential-scrubbing guarantee.
pub fn register_env_passthrough(
    allowlist: &mut HashSet<String>,
    var_names: &[String],
) -> Vec<String> {
    let mut accepted = Vec::new();
    for raw in var_names {
        let name = raw.trim().to_string();
        if name.is_empty() {
            continue;
        }
        if is_protected_credential(&name) {
            tracing::warn!(
                "env passthrough: refusing to register protected credential {name:?} \
                 (skills must not override the sandbox credential scrubbing; \
                 hermes GHSA-rhgp-j443-p4rf)"
            );
            continue;
        }
        if allowlist.insert(name.clone()) {
            accepted.push(name);
        }
    }
    accepted
}

/// Active-virtualenv markers that must not leak into terminal
/// subprocesses (hermes #23473): the agent's own venv would otherwise be
/// treated as active by uv/poetry in unrelated projects, clobbering it.
const ACTIVE_VENV_MARKER_VARS: &[&str] = &["VIRTUAL_ENV", "CONDA_PREFIX"];

/// Build the scrubbed child environment: the current process env minus
/// protected credentials (unless explicitly allowed) and venv markers.
pub fn scrubbed_env(allowlist: &HashSet<String>) -> Vec<(String, String)> {
    let blocklist = provider_env_blocklist();
    std::env::vars()
        .filter(|(key, _)| {
            if ACTIVE_VENV_MARKER_VARS.contains(&key.as_str()) {
                return false;
            }
            let upper = key.to_ascii_uppercase();
            if allowlist.contains(key.as_str()) || allowlist.contains(&upper) {
                return true;
            }
            if is_internal_secret(&upper) {
                return false;
            }
            !blocklist.contains(upper.as_str())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocklist_covers_known_credentials() {
        let list = provider_env_blocklist();
        for key in [
            "OPENAI_API_KEY",
            "ANTHROPIC_TOKEN",
            "TAVILY_API_KEY",
            "ULNCLAW_GATEWAY_KEY",
            "GH_TOKEN",
        ] {
            assert!(list.contains(key), "missing {key}");
        }
    }

    #[test]
    fn internal_secret_patterns() {
        assert!(is_internal_secret("AUXILIARY_COMPRESSION_API_KEY"));
        assert!(is_internal_secret("auxiliary_vision_base_url"));
        assert!(is_internal_secret("GATEWAY_RELAY_SECRET"));
        assert!(!is_internal_secret("AUXILIARY_MODEL"));
        assert!(!is_internal_secret("TENOR_API_KEY"));
    }

    #[test]
    fn registration_refuses_protected_credentials() {
        let mut allow = HashSet::new();
        let accepted = register_env_passthrough(
            &mut allow,
            &[
                "TENOR_API_KEY".to_string(),       // third-party: fine
                "OPENAI_API_KEY".to_string(),      // provider credential: refused
                "AUXILIARY_X_API_KEY".to_string(), // dynamic internal secret: refused
                "  ".to_string(),                  // blank: skipped
                "NOTION_TOKEN".to_string(),        // third-party: fine
            ],
        );
        assert_eq!(accepted, vec!["TENOR_API_KEY", "NOTION_TOKEN"]);
        assert!(allow.contains("TENOR_API_KEY"));
        assert!(!allow.contains("OPENAI_API_KEY"));
    }

    #[test]
    fn scrubbing_strips_credentials_and_venv_markers() {
        // Serialize with other env-mutating tests — OPENAI_API_KEY churn
        // here races env-sensitive provider resolution tests.
        let _guard = crate::models_dev::test_env_lock();
        // Use uniquely-named vars so parallel tests don't interfere.
        std::env::set_var("OPENAI_API_KEY", "secret-abc");
        std::env::set_var("VIRTUAL_ENV", "/some/venv");
        std::env::set_var("ULNCLAW_TEST_SAFE_VAR", "visible");
        let env = scrubbed_env(&HashSet::new());
        let keys: HashSet<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert!(!keys.contains("OPENAI_API_KEY"));
        assert!(!keys.contains("VIRTUAL_ENV"));
        assert!(keys.contains("ULNCLAW_TEST_SAFE_VAR"));
        assert!(keys.contains("PATH"));
        std::env::remove_var("OPENAI_API_KEY");
        std::env::remove_var("VIRTUAL_ENV");
        std::env::remove_var("ULNCLAW_TEST_SAFE_VAR");
    }

    #[test]
    fn allowlist_rescues_non_protected_var() {
        std::env::set_var("ULNCLAW_TEST_GATED_VAR", "gated");
        std::env::set_var("HASS_TOKEN", "hass-secret");
        let mut allow = HashSet::new();
        // A user-config passthrough for a blocked var is honored only for
        // non-protected names; HASS_TOKEN stays blocked at registration.
        register_env_passthrough(
            &mut allow,
            &["ULNCLAW_TEST_GATED_VAR".into(), "HASS_TOKEN".into()],
        );
        let env = scrubbed_env(&allow);
        let keys: HashSet<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert!(keys.contains("ULNCLAW_TEST_GATED_VAR"));
        assert!(!keys.contains("HASS_TOKEN"));
        std::env::remove_var("ULNCLAW_TEST_GATED_VAR");
        std::env::remove_var("HASS_TOKEN");
    }
}

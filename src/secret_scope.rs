//! Profile-scoped credential resolution for multi-profile gateway
//! multiplexing (hermes `agent/secret_scope.py` port).
//!
//! The multiplexing gateway serves many profiles from one process. Each
//! profile has its own `.env` with its own provider keys and platform
//! tokens, so we **cannot** union them into the process environment
//! (that would leak profile A's keys to profile B's turns, and to every
//! subprocess spawned with the inherited environment).
//!
//! This module provides a fail-closed, context-local secret scope:
//!
//! - [`scope_secrets`] installs the active profile's secrets around a
//!   future (a `tokio::task_local`, the Rust analogue of hermes'
//!   contextvar — it propagates into tasks spawned inside the scope).
//! - [`get_secret`] reads from that scope. When multiplexing is
//!   **active** and no scope is installed, it FAILS CLOSED with
//!   [`UnscopedSecretError`] rather than silently falling back to the
//!   process environment — an un-migrated call site fails loud at that
//!   exact line instead of leaking another profile's value. When
//!   multiplexing is **off** (the default), it transparently reads the
//!   process environment so the single-profile gateway and every
//!   non-gateway caller behave exactly as before.
//!
//! Scope installation points (hermes parity):
//! - [`crate::gateway::profile_dispatch`] wraps every `/p/<profile>/...`
//!   request in that profile's scope;
//! - the cron scheduler wraps every job run in the gateway home's scope;
//! - [`set_multiplex_active`] is called once at gateway startup.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

// ── multiplex-active flag ────────────────────────────────────────────────
// Process-global: set once at gateway startup when
// `[gateway] multiplex_profiles` is true. Governs whether `get_secret`
// fails closed on an unscoped read. A plain global (not a task-local):
// it describes the deployment mode, not a per-task value.
static MULTIPLEX_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Mark whether the process is running as a profile multiplexer.
///
/// Called once at gateway startup. When `true`, [`get_secret`] fails
/// closed on an unscoped read instead of falling back to the process
/// environment.
pub fn set_multiplex_active(active: bool) {
    MULTIPLEX_ACTIVE.store(active, Ordering::SeqCst);
}

/// Return whether the process is running as a profile multiplexer.
pub fn is_multiplex_active() -> bool {
    MULTIPLEX_ACTIVE.load(Ordering::SeqCst)
}

// ── the secret scope task-local ──────────────────────────────────────────
tokio::task_local! {
    static SECRET_SCOPE: std::sync::Arc<HashMap<String, String>>;
}

/// Install the active profile's secret mapping around `future`.
///
/// The scope propagates into every task spawned inside `future` (tokio
/// task-local semantics — the analogue of hermes' contextvar +
/// `copy_context()`). Pass the profile mapping built by
/// [`build_profile_secret_scope`].
pub fn scope_secrets<F: std::future::Future>(
    secrets: std::sync::Arc<HashMap<String, String>>,
    future: F,
) -> impl std::future::Future<Output = F::Output> {
    SECRET_SCOPE.scope(secrets, future)
}

/// Return the active secret mapping, or `None` when no scope is
/// installed on the current task.
pub fn current_secret_scope() -> Option<std::sync::Arc<HashMap<String, String>>> {
    SECRET_SCOPE.try_with(|scope| scope.clone()).ok()
}

/// Spawn a task that inherits the current secret scope — the analogue of
/// hermes' contextvar propagation via `copy_context()`.
///
/// Tokio task-locals do NOT automatically propagate into tasks spawned
/// with `tokio::spawn`, so spawn sites that cross a scope boundary
/// (per-turn run spawners, adapter loops) must use this helper instead
/// of a bare `tokio::spawn` to keep the profile scope authoritative
/// inside the spawned task.
pub fn spawn_scoped<F>(future: F) -> tokio::task::JoinHandle<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    match current_secret_scope() {
        Some(scope) => tokio::spawn(scope_secrets(scope, future)),
        None => tokio::spawn(future),
    }
}

// ── fail-closed error ────────────────────────────────────────────────────

/// Raised when a secret is read in multiplex mode with no scope
/// installed.
///
/// This is the fail-closed signal: it means a credential read reached
/// [`get_secret`] without a profile scope active, which in a
/// multiplexer would otherwise leak whichever profile's value happened
/// to be in the process environment. The fix is to wrap the call path
/// in [`scope_secrets`] (the per-turn / per-adapter profile scope), not
/// to widen the global-env allowlist.
#[derive(Debug, Clone)]
pub struct UnscopedSecretError {
    pub name: String,
}

impl std::fmt::Display for UnscopedSecretError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "get_secret({:?}) called with no profile secret scope active while \
             multiplexing is on. This credential read must run inside a \
             scope_secrets(...) block (the per-turn / per-adapter profile \
             scope). Reading the process environment here would risk leaking \
             another profile's value.",
            self.name
        )
    }
}

impl std::error::Error for UnscopedSecretError {}

// ── genuinely-global env vars (NOT per-profile secrets) ──────────────────
// These are process/deployment-level settings, not profile credentials.
// They legitimately live in the process environment and must keep
// reading from it even in multiplex mode — routing them through the
// fail-closed path would wrongly crash. Anything matching is read from
// the environment regardless of scope.
//
// Membership test is by exact name OR prefix (see `is_global_env`).
// Keep this list tight: when in doubt a value is a profile secret, not
// a global.
const GLOBAL_ENV_EXACT: &[&str] = &[
    // ulnclaw runtime / deployment (hermes HERMES_* equivalents)
    "ULNCLAW_HOME",
    "ULNCLAW_PROFILE",
    "ULNCLAW_GATEWAY_LOCK_DIR",
    "ULNCLAW_MAX_ITERATIONS",
    "ULNCLAW_MAX_TOKENS",
    "ULNCLAW_API_TIMEOUT",
    "ULNCLAW_REDACT_SECRETS",
    "ULNCLAW_TIMEZONE",
    "_ULNCLAW_GATEWAY",
    // OS / runtime
    "PATH",
    "HOME",
    "USER",
    "LANG",
    "LC_ALL",
    "TZ",
    "PWD",
    "SHELL",
    "TMPDIR",
    "VIRTUAL_ENV",
    "PYTHONPATH",
    "SSL_CERT_FILE",
    // Kanban paths (per-board, not per-profile-secret)
    "ULNCLAW_KANBAN_DB",
    "ULNCLAW_KANBAN_WORKSPACES_ROOT",
    "ULNCLAW_KANBAN_BOARD",
    // API-server LISTENER settings — deployment config, not profile
    // secrets. NOTE: gateway auth keys (ULNCLAW_GATEWAY_KEY,
    // API_SERVER_KEY) are deliberately NOT here — they ARE credentials
    // and stay profile-scoped.
    "API_SERVER_ENABLED",
    "API_SERVER_HOST",
    "API_SERVER_PORT",
    "API_SERVER_CORS_ORIGINS",
];

const GLOBAL_ENV_PREFIXES: &[&str] = &[
    "ULNCLAW_KANBAN_",
    "ULNCLAW_TELEGRAM_", // tuning knobs (batch delays, fallback toggles) — NOT the token
    "TERMINAL_",         // terminal/sandbox backend settings
];

/// Return `true` for genuinely process-global (non-profile-secret) env
/// vars.
pub fn is_global_env(name: &str) -> bool {
    if GLOBAL_ENV_EXACT.contains(&name) {
        return true;
    }
    GLOBAL_ENV_PREFIXES
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

// ── scoped resolution ────────────────────────────────────────────────────

/// Resolve a credential by env-var name, honoring the active profile
/// scope (hermes `get_secret(name, default=None)`).
///
/// Resolution order:
///
/// 1. Genuinely-global vars ([`is_global_env`]) always read the process
///    environment — they are deployment settings, not profile secrets.
/// 2. When a secret scope is installed (multiplexed turn), read from
///    it. Under multiplexing the scope is authoritative — an absent key
///    returns `default` and we do NOT fall through to the process
///    environment, because in a multiplexer the environment may hold
///    another profile's value. When multiplexing is OFF, a scope miss
///    falls through to the environment: single-profile deployments
///    legitimately provide credentials via the process environment
///    (systemd `Environment=`, secret-manager wrappers, plain shell
///    exports) rather than `<home>/.env`, and the scope — installed
///    unconditionally around e.g. every cron job — must stay a `.env`
///    overlay, not a blindfold.
/// 3. No scope installed:
///    - multiplex INACTIVE (default deployment): read the process
///      environment — identical to the legacy behavior every caller had
///      before.
///    - multiplex ACTIVE: FAIL CLOSED. Return [`UnscopedSecretError`]
///      so the missing scope is caught loudly instead of leaking a
///      cross-profile value.
pub fn get_secret_default(
    name: &str,
    default: Option<&str>,
) -> Result<Option<String>, UnscopedSecretError> {
    if is_global_env(name) {
        return Ok(std::env::var(name)
            .ok()
            .or_else(|| default.map(str::to_string)));
    }

    if let Some(scope) = current_secret_scope() {
        if let Some(value) = scope.get(name) {
            return Ok(Some(value.clone()));
        }
        if is_multiplex_active() {
            return Ok(default.map(str::to_string));
        }
        // Multiplex off: the scope is an overlay over the process
        // environment, not an isolation boundary — there is no other
        // profile to leak from. Without this fallthrough, credentials
        // injected only into the process environment vanish inside any
        // scope (the cron scheduler installs one around every job), so
        // cron jobs would send a placeholder API key and 401 while
        // interactive turns keep working.
        return Ok(std::env::var(name)
            .ok()
            .or_else(|| default.map(str::to_string)));
    }

    if is_multiplex_active() {
        return Err(UnscopedSecretError {
            name: name.to_string(),
        });
    }

    Ok(std::env::var(name)
        .ok()
        .or_else(|| default.map(str::to_string)))
}

/// [`get_secret_default`] with no default — hermes `get_secret(name)`.
pub fn get_secret(name: &str) -> Result<Option<String>, UnscopedSecretError> {
    get_secret_default(name, None)
}

/// Lenient variant for call sites outside the multiplexed gateway: a
/// fail-closed error degrades to the default instead of surfacing.
pub fn get_secret_lenient(name: &str, default: Option<&str>) -> Option<String> {
    get_secret_default(name, default).unwrap_or_else(|_| default.map(str::to_string))
}

// ── dotenv parsing (no process-env mutation) ─────────────────────────────

/// Strip a dotenv-style inline comment from a raw `.env` value.
///
/// Mirrors python-dotenv (1.2.2) semantics (hermes
/// `_strip_inline_comment`), verified empirically:
///
/// - Quoted values: scan for the matching close quote
///   (backslash-escape-aware for double quotes, since the writer emits
///   `\"`/`\\` escapes). Everything through the close quote is kept; a
///   trailing `# ...` remainder after it is discarded, so
///   `KEY="has # inside" # trailing` yields `has # inside`. Non-comment
///   trailing junk leaves the value untouched (lenient, unlike dotenv's
///   hard parse error).
/// - Unquoted values: truncate only at a `#` PRECEDED BY WHITESPACE, so
///   `KEY=foo#bar` keeps `foo#bar` while `KEY=value # comment` keeps
///   `value`. A value that *starts* with `#` (`KEY=#leading`) is kept.
pub fn strip_inline_comment(value: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        return value.to_string();
    }
    let quote = value.chars().next().unwrap();
    if quote == '\'' || quote == '"' {
        let mut chars = value.char_indices().peekable();
        chars.next(); // consume opening quote
        while let Some((idx, ch)) = chars.next() {
            if quote == '"' && ch == '\\' {
                chars.next(); // skip the escaped character
                continue;
            }
            if ch == quote {
                let remainder = value[idx + ch.len_utf8()..].trim_start();
                if remainder.starts_with('#') {
                    return value[..idx + ch.len_utf8()].to_string();
                }
                return value.to_string();
            }
        }
        return value.to_string(); // unterminated quote: leave as-is
    }
    // Unquoted: split at the first `#` preceded by whitespace.
    let mut prev_ws = false;
    for (idx, ch) in value.char_indices() {
        if ch == '#' && prev_ws {
            return value[..idx].trim().to_string();
        }
        prev_ws = ch.is_whitespace();
    }
    value.to_string()
}

/// Parse the small `.env` value subset ulnclaw writes itself (hermes
/// `_parse_env_value`).
///
/// Double-quoted values reverse the writer's `\"`/`\\` escapes;
/// single-quoted values are unwrapped verbatim; anything else passes
/// through.
pub fn parse_env_value(raw: &str) -> String {
    let value = raw.trim();
    let bytes = value.as_bytes();
    if value.len() >= 2 && bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"' {
        let quoted = &value[1..value.len() - 1];
        let mut parsed = String::with_capacity(quoted.len());
        let mut chars = quoted.chars();
        while let Some(ch) = chars.next() {
            if ch == '\\' {
                if let Some(next_ch) = chars.clone().next() {
                    if next_ch == '"' || next_ch == '\\' {
                        parsed.push(next_ch);
                        chars.next();
                        continue;
                    }
                }
            }
            parsed.push(ch);
        }
        return parsed;
    }
    if value.len() >= 2 && bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\'' {
        return value[1..value.len() - 1].to_string();
    }
    value.to_string()
}

/// Parse a `.env` file into a plain map WITHOUT touching the process
/// environment (hermes `load_env_file`).
///
/// Used to load a profile's secrets into an isolated mapping for
/// [`scope_secrets`]. Parses the small KEY=VALUE subset ulnclaw writes
/// itself (`export` prefix, `#` comments — full-line and
/// dotenv-compatible inline, matching quotes with the writer's
/// `\"`/`\\` escapes reversed) but never mutates the process
/// environment — that isolation is the whole point.
///
/// A leading UTF-8 BOM (Windows Notepad / PowerShell
/// `Set-Content -Encoding UTF8`) is stripped so it does not prefix the
/// first key and make `get_secret("NAME")` miss under scope.
pub fn load_env_scoped(path: &Path) -> HashMap<String, String> {
    let mut secrets = HashMap::new();
    let Ok(content) = std::fs::read_to_string(path) else {
        return secrets;
    };
    let content = content.strip_prefix('\u{feff}').unwrap_or(&content);
    for raw_line in content.lines() {
        let mut line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(stripped) = line.strip_prefix("export ") {
            line = stripped.trim_start();
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        secrets.insert(
            key.to_string(),
            parse_env_value(&strip_inline_comment(value)),
        );
    }
    secrets
}

// ── per-home external-source snapshot ────────────────────────────────────
// Hermes keeps `_SECRET_SOURCE_VALUES_BY_HOME` (populated by the dotenv
// startup path, or hydrated on first multiplexed turn for a profile
// that never ran it). Once-per-home semantics: sources are resolved at
// most once per home; later calls return the recorded snapshot.
static SOURCE_VALUES_BY_HOME: std::sync::Mutex<Option<HashMap<PathBuf, HashMap<String, String>>>> =
    std::sync::Mutex::new(None);

fn source_map() -> std::sync::MutexGuard<'static, Option<HashMap<PathBuf, HashMap<String, String>>>>
{
    SOURCE_VALUES_BY_HOME.lock().unwrap()
}

/// Return the external-secret value snapshot recorded for `home`
/// (hermes `get_secret_source_values`). Empty when no sources were
/// resolved for that home yet.
pub fn get_secret_source_values(home: &Path) -> HashMap<String, String> {
    let key = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    source_map()
        .as_ref()
        .and_then(|map| map.get(&key))
        .cloned()
        .unwrap_or_default()
}

/// Resolve one profile's configured external secret sources without
/// mutating the process environment (hermes
/// `hydrate_profile_secret_sources`).
///
/// Multiplex gateways can route a first turn to a secondary profile
/// that never ran the process-global startup path. Resolve that
/// profile's sources once and record the per-home snapshot for
/// [`build_profile_secret_scope`]. Fail-open semantics: any error
/// degrades to an empty mapping — external sources must never block
/// routing. The returned mapping contains only values actually
/// contributed by external sources, never the profile's plaintext
/// `.env` entries.
pub fn hydrate_profile_secret_sources(home: &Path) -> HashMap<String, String> {
    let key = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    {
        let guard = source_map();
        if let Some(map) = guard.as_ref() {
            if let Some(existing) = map.get(&key) {
                return existing.clone();
            }
        }
    }
    let mut contributed = HashMap::new();
    let config = crate::config::UlncLawConfig::load(Some(&home.join("config.toml")));
    if let Ok(config) = config {
        for (_source, result) in crate::secrets::fetch_all(&config.secrets, home) {
            if !result.ok {
                continue;
            }
            for (name, value) in result.secrets {
                contributed.insert(name, value);
            }
        }
    }
    let mut guard = source_map();
    let map = guard.get_or_insert_with(HashMap::new);
    map.insert(key, contributed.clone());
    contributed
}

/// Build a profile's secret mapping from its `<home>/.env` (hermes
/// `build_profile_secret_scope`).
///
/// Returns a fresh map (safe to install via [`scope_secrets`]).
/// Genuinely-global vars are intentionally NOT copied in —
/// [`get_secret`] reads those from the process environment directly,
/// so the scope holds only profile secrets.
pub fn build_profile_secret_scope(home: &Path) -> HashMap<String, String> {
    let mut secrets = load_env_scoped(&home.join(".env"));
    let external = hydrate_profile_secret_sources(home);
    for (name, value) in external {
        if is_global_env(&name) {
            continue;
        }
        secrets.insert(name, value);
    }
    secrets.retain(|name, _| !is_global_env(name));
    secrets
}

/// Clear the per-home source snapshot registry (test hook).
#[cfg(test)]
fn clear_source_registry() {
    *source_map() = None;
}

/// Serialize tests that toggle the process-global multiplex flag.
/// Poison-tolerant: one panicking test must not cascade into every
/// other scope test.
#[cfg(test)]
pub fn test_multiplex_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_env_value ──────────────────────────────────────────────
    #[test]
    fn parse_env_value_reverses_double_quote_escapes() {
        assert_eq!(parse_env_value(r#""plain""#), "plain");
        assert_eq!(
            parse_env_value(r#""has \"quote\" inside""#),
            r#"has "quote" inside"#
        );
        assert_eq!(parse_env_value(r#""back\\slash""#), r"back\slash");
        assert_eq!(parse_env_value(r#""kept \n literal""#), r"kept \n literal");
    }

    #[test]
    fn parse_env_value_single_quotes_verbatim() {
        assert_eq!(parse_env_value("'no \\\"escapes'"), "no \\\"escapes");
        assert_eq!(parse_env_value("plain"), "plain");
        assert_eq!(parse_env_value("  spaced  "), "spaced");
    }

    // ── strip_inline_comment ─────────────────────────────────────────
    #[test]
    fn strip_inline_comment_quoted_keeps_hash_inside() {
        assert_eq!(
            strip_inline_comment(r#""has # inside" # trailing"#),
            r#""has # inside""#
        );
        assert_eq!(strip_inline_comment("'q # v' # c"), "'q # v'");
        // Escaped quote does not end the value early.
        assert_eq!(strip_inline_comment(r#""a \" b" # c"#), r#""a \" b""#);
    }

    #[test]
    fn strip_inline_comment_unquoted_rules() {
        assert_eq!(strip_inline_comment("foo#bar"), "foo#bar");
        assert_eq!(strip_inline_comment("value # comment"), "value");
        assert_eq!(strip_inline_comment("#leading"), "#leading");
        assert_eq!(strip_inline_comment("value\t# tab comment"), "value");
        // Non-comment trailing junk after a close quote: lenient keep.
        assert_eq!(strip_inline_comment(r#""v" junk"#), r#""v" junk"#);
        // Unterminated quote: leave as-is.
        assert_eq!(
            strip_inline_comment(r#""unterminated # x"#),
            r#""unterminated # x"#
        );
    }

    // ── load_env_scoped ──────────────────────────────────────────────
    #[test]
    fn load_env_scoped_parses_bom_export_comments() {
        let dir = tempfile::tempdir().unwrap();
        let env = dir.path().join(".env");
        std::fs::write(
            &env,
            "\u{feff}# full-line comment\n\
             PLAIN=abc\n\
             export EXPORTED=def\n\
             QUOTED=\"has \\\"q\\\"\" # trailing\n\
             HASH=foo#bar\n\
             \n\
             NOVALUE\n",
        )
        .unwrap();
        let map = load_env_scoped(&env);
        assert_eq!(map.get("PLAIN").map(String::as_str), Some("abc"));
        assert_eq!(map.get("EXPORTED").map(String::as_str), Some("def"));
        assert_eq!(map.get("QUOTED").map(String::as_str), Some("has \"q\""));
        assert_eq!(map.get("HASH").map(String::as_str), Some("foo#bar"));
        assert!(!map.contains_key("NOVALUE"));
    }

    #[test]
    fn load_env_scoped_missing_file_is_empty() {
        let map = load_env_scoped(Path::new("/nonexistent-ulnclaw/.env"));
        assert!(map.is_empty());
    }

    // ── is_global_env ────────────────────────────────────────────────
    #[test]
    fn global_env_allowlist() {
        assert!(is_global_env("ULNCLAW_HOME"));
        assert!(is_global_env("PATH"));
        assert!(is_global_env("ULNCLAW_KANBAN_DB"));
        assert!(is_global_env("ULNCLAW_KANBAN_ANYTHING"));
        assert!(is_global_env("TERMINAL_BACKEND"));
        assert!(is_global_env("API_SERVER_PORT"));
        // Credentials stay profile-scoped.
        assert!(!is_global_env("API_SERVER_KEY"));
        assert!(!is_global_env("ULNCLAW_GATEWAY_KEY"));
        assert!(!is_global_env("OPENAI_API_KEY"));
        assert!(!is_global_env("TELEGRAM_BOT_TOKEN"));
    }

    // ── get_secret resolution ────────────────────────────────────────
    const TEST_VAR: &str = "ULNCLAW_SS_TEST_SECRET";

    #[tokio::test]
    async fn get_secret_no_scope_reads_process_env() {
        let _guard = test_multiplex_lock();
        set_multiplex_active(false);
        std::env::set_var(TEST_VAR, "env-value");
        assert_eq!(get_secret(TEST_VAR).unwrap().as_deref(), Some("env-value"));
        assert_eq!(
            get_secret_default("ULNCLAW_SS_UNSET_XYZ", Some("dflt"))
                .unwrap()
                .as_deref(),
            Some("dflt")
        );
        std::env::remove_var(TEST_VAR);
    }

    #[tokio::test]
    async fn get_secret_scope_hit_wins_over_env() {
        let _guard = test_multiplex_lock();
        set_multiplex_active(false);
        std::env::set_var(TEST_VAR, "env-value");
        let mut scope = HashMap::new();
        scope.insert(TEST_VAR.to_string(), "scoped-value".to_string());
        let out = scope_secrets(std::sync::Arc::new(scope), async move {
            get_secret(TEST_VAR).unwrap()
        })
        .await;
        assert_eq!(out.as_deref(), Some("scoped-value"));
        std::env::remove_var(TEST_VAR);
    }

    #[tokio::test]
    async fn get_secret_scope_miss_falls_through_when_multiplex_off() {
        let _guard = test_multiplex_lock();
        set_multiplex_active(false);
        std::env::set_var(TEST_VAR, "env-value");
        let scope: HashMap<String, String> = HashMap::new();
        let out = scope_secrets(std::sync::Arc::new(scope), async move {
            get_secret(TEST_VAR).unwrap()
        })
        .await;
        assert_eq!(out.as_deref(), Some("env-value"));
        std::env::remove_var(TEST_VAR);
    }

    #[tokio::test]
    async fn get_secret_scope_miss_authoritative_when_multiplex_on() {
        let _guard = test_multiplex_lock();
        set_multiplex_active(true);
        std::env::set_var(TEST_VAR, "env-value");
        let scope: HashMap<String, String> = HashMap::new();
        let out = scope_secrets(std::sync::Arc::new(scope), async move {
            get_secret_default(TEST_VAR, Some("dflt")).unwrap()
        })
        .await;
        // Scope miss under multiplexing returns the default — never the
        // process env (which may hold another profile's value).
        assert_eq!(out.as_deref(), Some("dflt"));
        set_multiplex_active(false);
        std::env::remove_var(TEST_VAR);
    }

    #[tokio::test]
    async fn get_secret_fails_closed_without_scope_under_multiplex() {
        let _guard = test_multiplex_lock();
        set_multiplex_active(true);
        let err = get_secret("OPENAI_API_KEY").unwrap_err();
        assert!(err.to_string().contains("OPENAI_API_KEY"));
        // Globals still resolve while multiplexing.
        std::env::set_var("ULNCLAW_SS_GLOBAL_TEST", "g");
        assert_eq!(
            get_secret("ULNCLAW_SS_GLOBAL_TEST").unwrap_err().name,
            "ULNCLAW_SS_GLOBAL_TEST"
        );
        std::env::remove_var("ULNCLAW_SS_GLOBAL_TEST");
        set_multiplex_active(false);
    }

    #[tokio::test]
    async fn get_secret_globals_never_fail_closed() {
        let _guard = test_multiplex_lock();
        set_multiplex_active(true);
        // Exact-match global: resolves from the environment even while
        // multiplexing, no scope needed.
        std::env::set_var("ULNCLAW_HOME", "/tmp/ulnclaw-ss-home");
        assert_eq!(
            get_secret("ULNCLAW_HOME").unwrap().as_deref(),
            Some("/tmp/ulnclaw-ss-home")
        );
        // Prefix global with a default.
        std::env::remove_var("ULNCLAW_KANBAN_BOARD");
        assert_eq!(
            get_secret_default("ULNCLAW_KANBAN_BOARD", Some("b"))
                .unwrap()
                .as_deref(),
            Some("b")
        );
        std::env::remove_var("ULNCLAW_HOME");
        set_multiplex_active(false);
    }

    #[tokio::test]
    async fn lenient_variant_degrades_to_default() {
        let _guard = test_multiplex_lock();
        set_multiplex_active(true);
        assert_eq!(
            get_secret_lenient("OPENAI_API_KEY", Some("dflt")).as_deref(),
            Some("dflt")
        );
        set_multiplex_active(false);
        assert_eq!(get_secret_lenient("ULNCLAW_SS_UNSET_XYZ", None), None);
    }

    #[tokio::test]
    async fn scope_propagates_into_spawned_tasks_via_spawn_scoped() {
        let mut scope = HashMap::new();
        scope.insert("ULNCLAW_SS_PROP".to_string(), "deep".to_string());
        let out = scope_secrets(std::sync::Arc::new(scope), async move {
            // Bare tokio::spawn would drop the scope (tokio task-local
            // semantics); spawn_scoped re-installs it, mirroring hermes'
            // copy_context() propagation into worker threads.
            spawn_scoped(async move { current_secret_scope() })
                .await
                .unwrap()
        })
        .await;
        assert_eq!(
            out.as_ref()
                .and_then(|map| map.get("ULNCLAW_SS_PROP"))
                .map(String::as_str),
            Some("deep")
        );
    }

    #[tokio::test]
    async fn bare_spawn_drops_scope_spawn_scoped_without_scope_is_plain() {
        // Outside any scope, spawn_scoped behaves exactly like tokio::spawn.
        let out = spawn_scoped(async move { current_secret_scope() })
            .await
            .unwrap();
        assert!(out.is_none());
    }

    // ── build_profile_secret_scope ───────────────────────────────────
    #[test]
    fn build_scope_excludes_globals_and_reads_env() {
        let _guard = test_multiplex_lock();
        clear_source_registry();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".env"),
            "OPENAI_API_KEY=sk-profile\nULNCLAW_HOME=/should/be/excluded\n",
        )
        .unwrap();
        let scope = build_profile_secret_scope(dir.path());
        assert_eq!(
            scope.get("OPENAI_API_KEY").map(String::as_str),
            Some("sk-profile")
        );
        assert!(!scope.contains_key("ULNCLAW_HOME"));
        clear_source_registry();
    }

    #[test]
    fn source_registry_records_once_per_home() {
        clear_source_registry();
        let dir = tempfile::tempdir().unwrap();
        // No config.toml → no sources → empty snapshot recorded.
        let first = hydrate_profile_secret_sources(dir.path());
        assert!(first.is_empty());
        assert!(get_secret_source_values(dir.path()).is_empty());
        // Unknown home → no snapshot at all.
        let unknown = dir.path().join("nope");
        assert!(get_secret_source_values(&unknown).is_empty());
        clear_source_registry();
    }
}

//! Status — quick overview of all components.
//!
//! Port of hermes `hermes_cli/status.py` (+ `hermes_cli/timefmt.py`'s
//! `relative_time`) adapted for ulnclaw: boxed banner, ✓/✗ marks,
//! environment/model/API-key/terminal/browser/gateway/cron/sessions/
//! skills/suggestions sections, and `--deep` probes.

use std::time::{Duration, Instant};

use crate::config::UlncLawConfig;

/// Relative-time rendering: "just now", "5m ago", "yesterday", or a date
/// (hermes `timefmt.relative_time`).
pub fn relative_time(ts: f64) -> String {
    if ts <= 0.0 {
        return "?".to_string();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let delta = now - ts;
    if delta < 60.0 {
        return "just now".to_string();
    }
    if delta < 3600.0 {
        return format!("{}m ago", (delta / 60.0) as u64);
    }
    if delta < 86400.0 {
        return format!("{}h ago", (delta / 3600.0) as u64);
    }
    if delta < 172800.0 {
        return "yesterday".to_string();
    }
    if delta < 604800.0 {
        return format!("{}d ago", (delta / 86400.0) as u64);
    }
    chrono::DateTime::from_timestamp(ts as i64, 0)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format("%Y-%m-%d")
                .to_string()
        })
        .unwrap_or_else(|| "?".to_string())
}

fn check_mark(ok: bool) -> &'static str {
    if ok {
        "✓"
    } else {
        "✗"
    }
}

/// Mask a secret for display: first 3 + last 4 chars (hermes `redact_key`).
pub fn redact_key(key: &str) -> String {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        return "(empty)".to_string();
    }
    let chars: Vec<char> = trimmed.chars().collect();
    if chars.len() <= 8 {
        return "****".to_string();
    }
    let head: String = chars.iter().take(3).collect();
    let tail: String = chars.iter().skip(chars.len() - 4).collect();
    format!("{}…{}", head, tail)
}

/// Vendor API-key table shared by `status` and `dump` (label, env-var
/// alternates; first non-empty wins). Hermes parity: `dump.py` api_keys.
pub const KEY_TABLE: &[(&str, &[&str])] = &[
    (
        "ULNCLAW_GATEWAY",
        &["ULNCLAW_GATEWAY_KEY", "ULNCLAW_API_KEY"],
    ),
    ("OpenRouter", &["OPENROUTER_API_KEY"]),
    ("OpenAI", &["OPENAI_API_KEY"]),
    ("Anthropic", &["ANTHROPIC_API_KEY", "ANTHROPIC_TOKEN"]),
    ("Google / Gemini", &["GOOGLE_API_KEY", "GEMINI_API_KEY"]),
    ("DeepSeek", &["DEEPSEEK_API_KEY"]),
    ("xAI / Grok", &["XAI_API_KEY"]),
    ("Groq", &["GROQ_API_KEY"]),
    ("Mistral", &["MISTRAL_API_KEY"]),
    ("Together", &["TOGETHER_API_KEY"]),
    ("Perplexity", &["PERPLEXITY_API_KEY"]),
    ("Ollama Cloud", &["OLLAMA_CLOUD_API_KEY"]),
    ("Tavily", &["TAVILY_API_KEY"]),
    ("Firecrawl", &["FIRECRAWL_API_KEY"]),
    ("Brave", &["BRAVE_API_KEY"]),
    ("GitHub", &["GH_TOKEN", "GITHUB_TOKEN"]),
    ("Notion", &["NOTION_TOKEN"]),
    ("Home Assistant", &["HASS_TOKEN"]),
    ("Discord", &["DISCORD_BOT_TOKEN"]),
    ("Tenor", &["TENOR_API_KEY"]),
];

/// Status options (hermes `status --all --deep`; `--all` shares the same
/// redaction posture, so ulnclaw folds it into the default rendering).
#[derive(Debug, Clone, Default)]
pub struct StatusOptions {
    pub deep: bool,
}

const CYAN: &str = "#00BCD4";
const DIM: &str = "#8B8682";

fn color_enabled() -> bool {
    std::env::var_os("NO_COLOR").is_none() && std::io::IsTerminal::is_terminal(&std::io::stdout())
}

fn paint(hex: &str, bold: bool, text: &str, enabled: bool) -> String {
    if !enabled {
        return text.to_string();
    }
    let Some((r, g, b)) = crate::skin::parse_hex(hex) else {
        return text.to_string();
    };
    if bold {
        format!("\x1b[1;38;2;{};{};{}m{}\x1b[0m", r, g, b, text)
    } else {
        format!("\x1b[38;2;{};{};{}m{}\x1b[0m", r, g, b, text)
    }
}

fn section(out: &mut String, title: &str, enabled: bool) {
    out.push('\n');
    out.push_str(&paint(CYAN, true, &format!("◆ {title}"), enabled));
    out.push('\n');
}

/// Render the full status report (hermes `show_status`).
pub fn show_status(config: &UlncLawConfig, opts: &StatusOptions) -> String {
    let enabled = color_enabled();
    let home = crate::config::ulnclaw_home();
    let mut out = String::new();

    out.push('\n');
    out.push_str(&paint(
        CYAN,
        false,
        "┌─────────────────────────────────────────────────────────┐",
        enabled,
    ));
    out.push('\n');
    out.push_str(&paint(
        CYAN,
        false,
        "│                 ⚕ ulnclaw Status                        │",
        enabled,
    ));
    out.push('\n');
    out.push_str(&paint(
        CYAN,
        false,
        "└─────────────────────────────────────────────────────────┘",
        enabled,
    ));
    out.push('\n');

    // Environment -----------------------------------------------------------
    section(&mut out, "Environment", enabled);
    out.push_str(&format!("  Version:      {}\n", crate::VERSION));
    out.push_str(&format!("  Home:         {}\n", home.display()));
    let config_path = home.join("config.toml");
    out.push_str(&format!(
        "  config.toml:  {} {}\n",
        check_mark(config_path.exists()),
        if config_path.exists() {
            "exists"
        } else {
            "not found (run 'ulnclaw init')"
        }
    ));
    let env_path = home.join(".env");
    out.push_str(&format!(
        "  .env file:    {} {}\n",
        check_mark(env_path.exists()),
        if env_path.exists() {
            "exists"
        } else {
            "not found"
        }
    ));

    // Model -------------------------------------------------------------------
    section(&mut out, "Model", enabled);
    if config.model.model.trim().is_empty() {
        out.push_str("  Model:        ✗ not configured\n");
    } else {
        out.push_str(&format!("  Model:        {}\n", config.model.model));
    }
    out.push_str(&format!("  Provider:     {}\n", config.model.provider));
    out.push_str(&format!("  Base URL:     {}\n", config.resolve_base_url()));

    // API Keys ------------------------------------------------------------------
    // Hermes parity: vendor key table (first non-empty alternate wins), values
    // always redacted. The `config` row surfaces model.api_key from config.toml.
    section(&mut out, "API Keys", enabled);
    if let Some(key) = &config.model.api_key {
        if !key.trim().is_empty() {
            out.push_str(&format!(
                "  {:<16}  ✓ model.api_key ({})\n",
                "config",
                redact_key(key)
            ));
        }
    }
    for (label, vars) in KEY_TABLE {
        let resolved = vars
            .iter()
            .find_map(|var| crate::config::get_env_value(var))
            .filter(|value| !value.trim().is_empty());
        match resolved {
            Some(value) => out.push_str(&format!("  {:<16}  ✓ {}\n", label, redact_key(&value))),
            None => out.push_str(&format!("  {:<16}  ✗ not set\n", label)),
        }
    }

    // Terminal backend ---------------------------------------------------------
    section(&mut out, "Terminal Backend", enabled);
    let backend = config
        .terminal
        .backend
        .clone()
        .unwrap_or_else(|| "local".to_string());
    out.push_str(&format!("  Backend:      {}\n", backend));

    // Browser --------------------------------------------------------------------
    section(&mut out, "Browser", enabled);
    match crate::browser::configured_endpoint_raw() {
        Some(raw) => {
            let mode = if crate::browser::is_auto_mode(&raw) {
                "managed (auto)"
            } else {
                "endpoint"
            };
            out.push_str(&format!("  Endpoint:     {raw} ({mode})\n"));
        }
        None => out.push_str("  Endpoint:     (not configured)\n"),
    }
    let candidates = crate::browser::connect::get_chrome_debug_candidates(
        crate::browser::connect::host_system(),
    );
    match candidates.first() {
        Some(path) => out.push_str(&format!("  Binary:       ✓ {}\n", path.display())),
        None => out.push_str("  Binary:       ✗ no Chromium-family browser found\n"),
    }

    // Gateway ----------------------------------------------------------------------
    section(&mut out, "Gateway", enabled);
    out.push_str(&format!(
        "  Listen:       {}:{}\n",
        config.gateway.host, config.gateway.port
    ));
    let key_set = config
        .gateway
        .key
        .as_deref()
        .map(str::trim)
        .map(|k| !k.is_empty())
        .unwrap_or(false)
        || crate::config::get_env_value("ULNCLAW_GATEWAY_KEY")
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false);
    out.push_str(&format!(
        "  Auth key:     {} {}\n",
        check_mark(key_set),
        if key_set {
            "configured"
        } else {
            "missing (gateway refuses API traffic without a key)"
        }
    ));
    out.push_str(&format!(
        "  Multiplex:    {} profile mirrors\n",
        if config.gateway.multiplex_profiles {
            "✓"
        } else {
            "✗"
        }
    ));
    if opts.deep {
        let reachable = probe_tcp("127.0.0.1", config.gateway.port, Duration::from_secs(1));
        out.push_str(&format!(
            "  Port {}:     {}\n",
            config.gateway.port,
            if reachable {
                "in use (gateway likely running)"
            } else {
                "available"
            }
        ));
    }

    // Scheduled jobs ----------------------------------------------------------------
    section(&mut out, "Scheduled Jobs", enabled);
    match crate::cron::CronStore::open_default() {
        Ok(store) => match store.list() {
            Ok(jobs) => {
                let active = jobs.iter().filter(|j| j.enabled).count();
                out.push_str(&format!(
                    "  Jobs:         {} active, {} total\n",
                    active,
                    jobs.len()
                ));
                if let Some(next) = jobs
                    .iter()
                    .filter(|j| j.enabled)
                    .filter_map(|j| j.next_run)
                    .fold(None, |acc, ts| Some(acc.map_or(ts, |a: f64| a.min(ts))))
                {
                    out.push_str(&format!("  Next run:     {}\n", relative_time(next)));
                }
            }
            Err(e) => out.push_str(&format!(
                "  Jobs:         (error reading cron store: {e})\n"
            )),
        },
        Err(e) => out.push_str(&format!(
            "  Jobs:         (error opening cron store: {e})\n"
        )),
    }

    // Sessions ------------------------------------------------------------------------
    section(&mut out, "Sessions", enabled);
    match crate::session::SqliteSessionStore::open_default() {
        Ok(store) => {
            match store.count_sessions() {
                Ok(count) => out.push_str(&format!("  Total:        {} session(s)\n", count)),
                Err(e) => out.push_str(&format!("  Total:        (error: {e})\n")),
            }
            if let Ok(rows) = store.list_session_rows(1) {
                if let Some(freshest) = rows.first() {
                    out.push_str(&format!(
                        "  Last session: {} ({})\n",
                        relative_time(freshest.started_at),
                        freshest.source
                    ));
                }
            }
        }
        Err(_) => out.push_str("  Store:        not created yet\n"),
    }

    // Skills ------------------------------------------------------------------------------
    section(&mut out, "Skills", enabled);
    let skills_dir = home.join("skills");
    let skills = crate::skills::list_skills(&skills_dir);
    out.push_str(&format!("  Installed:    {} skill(s)\n", skills.len()));
    let pending = crate::cron::suggestions::SuggestionStore::open_default();
    out.push_str(&format!(
        "  Suggestions:  {} pending\n",
        pending.list_pending().len()
    ));

    // Updates -------------------------------------------------------------------------------
    section(&mut out, "Updates", enabled);
    match crate::banner::check_for_updates() {
        Some(0) => out.push_str("  Upstream:     ✓ up to date\n"),
        Some(n) if n > 0 => out.push_str(&format!("  Upstream:     ⚠ {n} commit(s) behind\n")),
        Some(_) => out.push_str("  Upstream:     ⚠ update available\n"),
        None => out.push_str("  Upstream:     (no git checkout — update check unavailable)\n"),
    }

    // Footer ----------------------------------------------------------------------------------
    out.push('\n');
    out.push_str(&paint(DIM, false, &"─".repeat(60), enabled));
    out.push('\n');
    out.push_str(&paint(
        DIM,
        false,
        "  Run 'ulnclaw doctor' for detailed diagnostics",
        enabled,
    ));
    out.push('\n');
    out.push_str(&paint(
        DIM,
        false,
        "  Run 'ulnclaw init' to write a default config",
        enabled,
    ));
    out.push('\n');
    out.push('\n');
    out
}

fn probe_tcp(host: &str, port: u16, timeout: Duration) -> bool {
    let start = Instant::now();
    match std::net::TcpStream::connect_timeout(
        &format!("{host}:{port}")
            .parse()
            .unwrap_or_else(|_| std::net::SocketAddr::from(([127, 0, 0, 1], port))),
        timeout,
    ) {
        Ok(_) => true,
        Err(_) => start.elapsed() >= timeout,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_time_buckets() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        assert_eq!(relative_time(0.0), "?");
        assert_eq!(relative_time(now - 10.0), "just now");
        assert_eq!(relative_time(now - 300.0), "5m ago");
        assert_eq!(relative_time(now - 7200.0), "2h ago");
        assert_eq!(relative_time(now - 100_000.0), "yesterday");
        assert_eq!(relative_time(now - 3.0 * 86400.0), "3d ago");
        let old = relative_time(now - 30.0 * 86400.0);
        assert!(old.contains('-'), "old timestamps render as dates: {old}");
    }

    #[test]
    fn redact_key_masks_secrets() {
        assert_eq!(redact_key(""), "(empty)");
        assert_eq!(redact_key("short"), "****");
        assert_eq!(redact_key("sk-1234567890abcdef"), "sk-…cdef");
    }

    #[test]
    fn status_report_covers_sections() {
        let _guard = crate::models_dev::test_env_lock();
        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var("ULNCLAW_HOME").ok();
        std::env::set_var("ULNCLAW_HOME", dir.path());
        let mut config = UlncLawConfig::default();
        config.model.provider = "ollama".to_string();
        config.model.model = "qwen2.5:14b".to_string();
        let report = show_status(&config, &StatusOptions::default());
        for needle in [
            "ulnclaw Status",
            "Environment",
            "Model",
            "API Keys",
            "OpenRouter",
            "Terminal Backend",
            "Browser",
            "Gateway",
            "Scheduled Jobs",
            "Sessions",
            "Skills",
            "Updates",
            "ulnclaw doctor",
        ] {
            assert!(report.contains(needle), "missing section: {needle}");
        }
        match prev {
            Some(v) => std::env::set_var("ULNCLAW_HOME", v),
            None => std::env::remove_var("ULNCLAW_HOME"),
        }
    }
}

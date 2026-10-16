//! `ulnclaw dump` + rich `ulnclaw version` (hermes `hermes_cli/dump.py`
//! and `hermes_cli/build_info.py` parity).
//!
//! `dump` outputs a compact, plain-text summary of the user's setup that can
//! be copy-pasted into a bug report or chat for support context. No ANSI
//! colors, no checkmarks — just data.

use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

use crate::config::UlncLawConfig;

/// Crate version baked in at build time.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Build-time SHA marker file (hermes `.hermes_build_sha`). Release packaging
/// writes the commit hash into `<repo_root>/.ulnclaw_build_sha` so installs
/// without a `.git` directory can still identify their build.
const BUILD_SHA_FILE: &str = ".ulnclaw_build_sha";

/// Return the baked-in build SHA truncated to `short` chars, or `None`.
///
/// Aligned with `build_info.get_build_sha`: returns `None` when the file is
/// absent, unreadable, or empty — the build SHA is a nice-to-have for
/// support triage and must never crash the CLI.
pub fn get_build_sha(root: &Path, short: usize) -> Option<String> {
    let path = root.join(BUILD_SHA_FILE);
    let sha = std::fs::read_to_string(path).ok()?.trim().to_string();
    if sha.is_empty() {
        return None;
    }
    if short == 0 {
        Some(sha)
    } else {
        Some(sha.chars().take(short).collect())
    }
}

/// Short SHA of HEAD, falling back to the baked build SHA (hermes
/// `_get_git_commit`). Returns `"(unknown)"` when neither resolves.
pub fn git_short_sha(root: &Path) -> String {
    if let Some(sha) = run_git(root, &["rev-parse", "--short=8", "HEAD"]) {
        return sha;
    }
    get_build_sha(root, 8).unwrap_or_else(|| "(unknown)".to_string())
}

/// Date HEAD was authored (`YYYY-MM-DD`), or `""` when unavailable (hermes
/// `_get_git_commit_date`).
pub fn git_commit_date(root: &Path) -> String {
    run_git(root, &["log", "-1", "--format=%cd", "--date=short", "HEAD"]).unwrap_or_default()
}

fn run_git(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

/// Env-var names assigned a non-empty value in `<home>/.env` (hermes
/// `_dotenv_key_names`). Managed backends load credentials from this file,
/// not from an interactive shell's exports; comparing against this set lets
/// the dump flag keys that are shell-only.
pub fn dotenv_key_names(home: &Path) -> HashSet<String> {
    crate::config::load_env_file(&home.join(".env"))
        .into_iter()
        .filter(|(_, value)| !value.trim().is_empty())
        .map(|(name, _)| name)
        .collect()
}

/// One-line version identifier: `X.Y.Z [sha] (commit-date)` (hermes dump
/// version line). The commit date is the real "as-of" date.
pub fn version_string(root: &Path) -> String {
    let sha = git_short_sha(root);
    let date = git_commit_date(root);
    if date.is_empty() {
        format!("{VERSION} [{sha}]")
    } else {
        format!("{VERSION} [{sha}] ({date})")
    }
}

/// Best-effort install-method label (hermes `detect_install_method`,
/// simplified for a Rust/cargo distribution).
pub fn detect_install_method(root: &Path) -> &'static str {
    if root.join(".git").exists() {
        "git checkout (source build)"
    } else {
        "prebuilt"
    }
}

/// Rich version report (hermes `_print_version_info` / `cmd_version`):
/// version line, install directory/method, and live update status.
pub fn build_version_report(root: Option<&Path>, check_updates: bool) -> String {
    let mut lines = Vec::new();
    match root {
        Some(root) => {
            lines.push(format!("ulnclaw {}", version_string(root)));
            lines.push(format!("Install directory: {}", root.display()));
            lines.push(format!("Install method: {}", detect_install_method(root)));
            if check_updates {
                lines.push(update_status_line(root));
            }
        }
        None => {
            lines.push(format!("ulnclaw {VERSION} [(unknown)]"));
            lines.push("Install directory: (unknown — not run from a checkout)".to_string());
        }
    }
    lines.join("\n") + "\n"
}

/// One-line update status for the version report (hermes
/// `check_for_updates` block in `_print_version_info`).
fn update_status_line(root: &Path) -> String {
    let opts = crate::update::UpdateOptions::default();
    match crate::update::check_update(root, &opts) {
        Ok((crate::update::CheckOutcome::UpToDate, _)) => "Update status: up to date".to_string(),
        Ok((crate::update::CheckOutcome::Behind { count, .. }, _)) => {
            let word = if count == 1 { "commit" } else { "commits" };
            format!("Update available: {count} {word} behind — run 'ulnclaw update'")
        }
        Ok((crate::update::CheckOutcome::BehindShallow { .. }, _)) => {
            "Update available: behind upstream (shallow clone) — run 'ulnclaw update'".to_string()
        }
        Err(err) => {
            let first = err.lines().next().unwrap_or("unknown error").to_string();
            format!("Update status: unavailable ({first})")
        }
    }
}

/// OS info line: `<sysname> <release> <machine>` (hermes
/// `platform.system()/release()/machine()`).
fn os_info() -> String {
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_default();
    if release.is_empty() {
        format!("{} {}", std::env::consts::OS, std::env::consts::ARCH)
    } else {
        format!(
            "{} {} {}",
            std::env::consts::OS,
            release,
            std::env::consts::ARCH
        )
    }
}

fn cron_summary() -> String {
    match crate::cron::CronStore::open_default() {
        Ok(store) => match store.list() {
            Ok(jobs) => {
                let active = jobs.iter().filter(|j| j.enabled).count();
                format!("{} active / {} total", active, jobs.len())
            }
            Err(_) => "(error reading)".to_string(),
        },
        Err(_) => "0".to_string(),
    }
}

fn count_skills() -> usize {
    let skills_dir = crate::config::ulnclaw_home().join("skills");
    crate::skills::list_skills(&skills_dir).len()
}

/// Non-default config values worth reporting (hermes `_config_overrides`).
fn config_overrides(config: &UlncLawConfig) -> Vec<(String, String)> {
    let defaults = UlncLawConfig::default();
    let mut overrides: Vec<(String, String)> = Vec::new();
    let mut push = |path: &str, value: String| overrides.push((path.to_string(), value));

    if config.agent.max_iterations != defaults.agent.max_iterations {
        push(
            "agent.max_iterations",
            config.agent.max_iterations.to_string(),
        );
    }
    if config.agent.approval != defaults.agent.approval {
        push("agent.approval", config.agent.approval.to_string());
    }
    if config.agent.concurrent_tool_execution != defaults.agent.concurrent_tool_execution {
        push(
            "agent.concurrent_tool_execution",
            config.agent.concurrent_tool_execution.to_string(),
        );
    }
    if config.agent.context_budget_tokens != defaults.agent.context_budget_tokens {
        push(
            "agent.context_budget_tokens",
            config.agent.context_budget_tokens.to_string(),
        );
    }
    if config.terminal.backend != defaults.terminal.backend {
        let backend = config
            .terminal
            .backend
            .clone()
            .unwrap_or_else(|| "local".to_string());
        push("terminal.backend", backend);
    }
    if let Some(image) = &config.terminal.image {
        push("terminal.image", image.clone());
    }
    if let Some(container) = &config.terminal.container {
        push("terminal.container", container.clone());
    }
    if config.checkpoints.enabled != defaults.checkpoints.enabled {
        push(
            "checkpoints.enabled",
            config.checkpoints.enabled.to_string(),
        );
    }
    if config.memory.memory_char_limit != defaults.memory.memory_char_limit {
        push(
            "memory.memory_char_limit",
            config.memory.memory_char_limit.to_string(),
        );
    }
    if config.memory.user_char_limit != defaults.memory.user_char_limit {
        push(
            "memory.user_char_limit",
            config.memory.user_char_limit.to_string(),
        );
    }
    if let Some(backend) = &config.web.search_backend {
        push("web.search_backend", backend.clone());
    }
    if let Some(backend) = &config.web.extract_backend {
        push("web.extract_backend", backend.clone());
    }
    if config.gateway.port != defaults.gateway.port {
        push("gateway.port", config.gateway.port.to_string());
    }
    if let Some(cap) = config.gateway.max_concurrent_sessions {
        push("gateway.max_concurrent_sessions", cap.to_string());
    }
    if let Some(timezone) = &config.timezone {
        if !timezone.trim().is_empty() {
            push("timezone", timezone.clone());
        }
    }
    if let Some(temperature) = config.model.temperature {
        push("model.temperature", temperature.to_string());
    }
    if let Some(max_tokens) = config.model.max_tokens {
        push("model.max_tokens", max_tokens.to_string());
    }
    if config.model.max_retries != defaults.model.max_retries {
        push("model.max_retries", config.model.max_retries.to_string());
    }
    if !config.model.fallbacks.is_empty() {
        push("model.fallbacks", config.model.fallbacks.join(", "));
    }
    if !config.enabled_toolsets.is_empty() {
        push("enabled_toolsets", config.enabled_toolsets.join(", "));
    }
    if !config.disabled_toolsets.is_empty() {
        push("disabled_toolsets", config.disabled_toolsets.join(", "));
    }
    overrides
}

/// Effective terminal backend, surfacing a `TERMINAL_ENV` override (hermes
/// dump parity: the env var wins over `terminal.backend` in config).
fn effective_terminal_backend(config: &UlncLawConfig) -> String {
    let config_backend = config
        .terminal
        .backend
        .clone()
        .unwrap_or_else(|| "local".to_string());
    let env_backend = std::env::var("TERMINAL_ENV").unwrap_or_default();
    let env_backend = env_backend.trim().to_lowercase();
    if !env_backend.is_empty() && env_backend != config_backend.trim().to_lowercase() {
        format!("{env_backend}  (TERMINAL_ENV overrides config terminal.backend={config_backend})")
    } else {
        config_backend
    }
}

/// Build the plain-text dump (hermes `run_dump`). `profile` is the
/// `--profile` name applied to this invocation, if any.
pub fn build_dump(config: &UlncLawConfig, profile: Option<&str>, show_keys: bool) -> String {
    let root = crate::update::find_repo_root();
    let home = crate::config::ulnclaw_home();

    let mut lines: Vec<String> = Vec::new();
    lines.push("--- ulnclaw dump ---".to_string());
    match root.as_deref() {
        Some(root) => lines.push(format!("version:          {}", version_string(root))),
        None => lines.push(format!("version:          {VERSION} [(unknown)]")),
    }
    lines.push(format!("os:               {}", os_info()));
    lines.push(format!(
        "profile:          {}",
        profile.unwrap_or("(default)")
    ));
    lines.push(format!("ulnclaw_home:     {}", crate::logs::display_home()));
    let model = if config.model.model.trim().is_empty() {
        "(not set)".to_string()
    } else {
        config.model.model.clone()
    };
    lines.push(format!("model:            {model}"));
    lines.push(format!("provider:         {}", config.model.provider));
    lines.push(format!(
        "terminal:         {}",
        effective_terminal_backend(config)
    ));

    // API keys ----------------------------------------------------------------
    lines.push(String::new());
    lines.push("api_keys:".to_string());
    if let Some(key) = &config.model.api_key {
        if !key.trim().is_empty() {
            let display = if show_keys {
                crate::status::redact_key(key)
            } else {
                "set".to_string()
            };
            lines.push(format!(
                "  {:<20} {} (config.toml model.api_key)",
                "config", display
            ));
        }
    }
    let dotenv_keys = dotenv_key_names(&home);
    for (label, vars) in crate::status::KEY_TABLE {
        // Process env first, then .env (mirrors `config::get_env_value`).
        let from_env = vars.iter().find_map(|var| {
            std::env::var(var)
                .ok()
                .filter(|value| !value.trim().is_empty())
                .map(|value| (var, value))
        });
        let from_dotenv = vars.iter().find_map(|var| {
            crate::config::load_env_file(&home.join(".env"))
                .remove(*var)
                .filter(|value| !value.trim().is_empty())
                .map(|value| (var, value))
        });
        let mut display = match from_env.as_ref().or(from_dotenv.as_ref()) {
            Some((_, value)) if show_keys => crate::status::redact_key(value),
            Some(_) => "set".to_string(),
            None => "not set".to_string(),
        };
        // Set in this (shell) process but absent from ~/.ulnclaw/.env: a
        // managed backend (systemd / gateway service) loads .env, not the
        // login shell, so it likely can't see this key. Flag it (hermes
        // parity, the #48504-style phantom-key trap).
        if let Some((var, _)) = &from_env {
            if !dotenv_keys.contains(**var) {
                display.push_str(
                    " (shell only — not in .env; managed/gateway backend may not see it)",
                );
            }
        }
        lines.push(format!("  {:<20} {display}", label));
    }

    // Features ------------------------------------------------------------------
    lines.push(String::new());
    lines.push("features:".to_string());
    let toolsets = if config.enabled_toolsets.is_empty() {
        "(default)".to_string()
    } else {
        config.enabled_toolsets.join(", ")
    };
    lines.push(format!("  toolsets:           {toolsets}"));
    if !config.disabled_toolsets.is_empty() {
        lines.push(format!(
            "  disabled_toolsets:  {}",
            config.disabled_toolsets.join(", ")
        ));
    }
    lines.push(format!(
        "  mcp_servers:        {}",
        config.mcp.servers.len()
    ));
    lines.push("  memory_provider:    built-in".to_string());
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
    lines.push(format!(
        "  gateway:            {}:{} (auth key {})",
        config.gateway.host,
        config.gateway.port,
        if key_set { "set" } else { "missing" }
    ));
    if let Some(cap) = config.gateway.max_concurrent_sessions {
        lines.push(format!("  max_sessions:       {cap}"));
    }
    lines.push(format!("  cron_jobs:          {}", cron_summary()));
    lines.push(format!("  skills:             {}", count_skills()));
    lines.push(format!(
        "  checkpoints:        {}",
        if config.checkpoints.enabled {
            "enabled"
        } else {
            "disabled"
        }
    ));

    // Config overrides ----------------------------------------------------------
    let overrides = config_overrides(config);
    if !overrides.is_empty() {
        lines.push(String::new());
        lines.push("config_overrides:".to_string());
        for (key, value) in overrides {
            lines.push(format!("  {key}: {value}"));
        }
    }

    lines.push("--- end dump ---".to_string());
    lines.join("\n") + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_home<F: FnOnce()>(dir: &Path, f: F) {
        let _guard = crate::models_dev::test_env_lock();
        let prev = std::env::var("ULNCLAW_HOME").ok();
        std::env::set_var("ULNCLAW_HOME", dir);
        f();
        match prev {
            Some(v) => std::env::set_var("ULNCLAW_HOME", v),
            None => std::env::remove_var("ULNCLAW_HOME"),
        }
    }

    #[test]
    fn build_sha_reads_marker_file() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(get_build_sha(dir.path(), 8), None);

        std::fs::write(
            dir.path().join(".ulnclaw_build_sha"),
            "0123456789abcdef0123456789abcdef01234567\n",
        )
        .unwrap();
        assert_eq!(get_build_sha(dir.path(), 8), Some("01234567".to_string()));
        assert_eq!(
            get_build_sha(dir.path(), 0),
            Some("0123456789abcdef0123456789abcdef01234567".to_string())
        );

        std::fs::write(dir.path().join(".ulnclaw_build_sha"), "  \n").unwrap();
        assert_eq!(get_build_sha(dir.path(), 8), None);
    }

    #[test]
    fn short_sha_falls_back_to_build_sha_then_unknown() {
        let dir = tempfile::tempdir().unwrap();
        // No .git, no marker -> unknown.
        assert_eq!(git_short_sha(dir.path()), "(unknown)");
        assert_eq!(git_commit_date(dir.path()), "");

        std::fs::write(dir.path().join(".ulnclaw_build_sha"), "abcdef0123456789").unwrap();
        assert_eq!(git_short_sha(dir.path()), "abcdef01");
    }

    #[test]
    fn version_string_includes_crate_version() {
        let dir = tempfile::tempdir().unwrap();
        let vs = version_string(dir.path());
        assert!(vs.starts_with(VERSION), "got {vs}");
        assert!(vs.contains("[(unknown)]"), "got {vs}");
    }

    #[test]
    fn dotenv_key_names_parses_env_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".env"),
            "\
# comment
OPENAI_API_KEY=sk-abc123
export TAVILY_API_KEY=\"tvly-1\"
EMPTY_KEY=
   # indented comment
QUOTED='x'
",
        )
        .unwrap();
        let names = dotenv_key_names(dir.path());
        assert!(names.contains("OPENAI_API_KEY"));
        assert!(names.contains("TAVILY_API_KEY"));
        assert!(names.contains("QUOTED"));
        assert!(!names.contains("EMPTY_KEY"));
    }

    #[test]
    fn dump_covers_sections_without_ansi() {
        let dir = tempfile::tempdir().unwrap();
        with_home(dir.path(), || {
            let mut config = UlncLawConfig::default();
            config.model.provider = "ollama".to_string();
            config.model.model = "qwen2.5:14b".to_string();
            let out = build_dump(&config, None, false);
            for needle in [
                "--- ulnclaw dump ---",
                "version:",
                "os:",
                "profile:          (default)",
                "ulnclaw_home:",
                "model:            qwen2.5:14b",
                "provider:         ollama",
                "terminal:",
                "api_keys:",
                "  OpenRouter           not set",
                "features:",
                "  mcp_servers:        0",
                "  cron_jobs:          ",
                "  skills:             0",
                "  checkpoints:        disabled",
                "--- end dump ---",
            ] {
                assert!(out.contains(needle), "missing: {needle}\n{out}");
            }
            assert!(
                !out.contains('\u{1b}'),
                "dump must not contain ANSI escapes"
            );
        });
    }

    #[test]
    fn dump_flags_shell_only_keys_and_redacts_with_show_keys() {
        let dir = tempfile::tempdir().unwrap();
        with_home(dir.path(), || {
            std::env::set_var("OPENROUTER_API_KEY", "sk-or-v1-1234567890abcdef");
            // .env has no OPENROUTER_API_KEY -> shell-only warning.
            let config = UlncLawConfig::default();
            let plain = build_dump(&config, Some("work"), false);
            assert!(plain.contains("profile:          work"), "{plain}");
            assert!(
                plain.contains("shell only — not in .env"),
                "expected shell-only flag\n{plain}"
            );
            assert!(
                plain.contains("  OpenRouter           set"),
                "expected set marker\n{plain}"
            );

            let shown = build_dump(&config, None, true);
            assert!(
                shown.contains("sk-…cdef"),
                "expected redacted value\n{shown}"
            );
            assert!(!shown.contains("1234567890"), "raw key leaked\n{shown}");
            std::env::remove_var("OPENROUTER_API_KEY");
        });
    }

    #[test]
    fn dump_reports_env_file_keys_as_set_without_warning() {
        let dir = tempfile::tempdir().unwrap();
        with_home(dir.path(), || {
            std::fs::write(
                dir.path().join(".env"),
                "TAVILY_API_KEY=tvly-abcdefgh1234\n",
            )
            .unwrap();
            let config = UlncLawConfig::default();
            let out = build_dump(&config, None, false);
            assert!(out.contains("  Tavily               set"), "{out}");
            let tavily_line = out
                .lines()
                .find(|l| l.trim_start().starts_with("Tavily"))
                .unwrap_or("");
            assert!(!tavily_line.contains("shell only"), "{tavily_line}");
        });
    }

    #[test]
    fn dump_config_overrides_and_terminal_env() {
        let dir = tempfile::tempdir().unwrap();
        with_home(dir.path(), || {
            std::env::set_var("TERMINAL_ENV", "docker");
            let mut config = UlncLawConfig::default();
            config.agent.max_iterations = 42;
            config.checkpoints.enabled = true;
            config.model.fallbacks = vec!["openai:gpt-5".into()];
            let out = build_dump(&config, None, false);
            assert!(out.contains("config_overrides:"), "{out}");
            assert!(out.contains("agent.max_iterations: 42"), "{out}");
            assert!(out.contains("checkpoints.enabled: true"), "{out}");
            assert!(out.contains("model.fallbacks: openai:gpt-5"), "{out}");
            assert!(
                out.contains("docker  (TERMINAL_ENV overrides config terminal.backend=local)"),
                "{out}"
            );
            std::env::remove_var("TERMINAL_ENV");
        });
    }

    #[test]
    fn version_report_handles_missing_root() {
        let out = build_version_report(None, false);
        assert!(out.contains("ulnclaw"), "{out}");
        assert!(out.contains("(unknown)"), "{out}");
        assert!(!out.contains("Update status"), "{out}");
    }

    #[test]
    fn version_report_includes_install_dir() {
        let dir = tempfile::tempdir().unwrap();
        let out = build_version_report(Some(dir.path()), false);
        assert!(out.starts_with("ulnclaw "), "{out}");
        assert!(out.contains("Install directory:"), "{out}");
        assert!(out.contains("Install method: prebuilt"), "{out}");
        assert!(!out.contains("Update status"), "check disabled\n{out}");
    }
}

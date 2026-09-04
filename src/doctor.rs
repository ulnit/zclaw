//! Doctor — diagnose configuration and dependencies.
//!
//! Port of hermes `hermes_cli/doctor.py` (v2026.8.3), adapted for a Rust
//! binary: boxed report banner, ✓/⚠/✗/ℹ checks grouped in sections, an
//! issues summary with a `--fix` fast path, plus `--online` provider
//! connectivity probes and `--json` output.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::UlncLawConfig;

/// Check severity (hermes check_ok/check_warn/check_fail/check_info).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Ok,
    Warn,
    Fail,
    Info,
}

/// One check line.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Check {
    pub level: Level,
    pub text: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

/// A titled group of checks.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Section {
    pub title: String,
    pub checks: Vec<Check>,
}

/// Full doctor report.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct DoctorReport {
    pub sections: Vec<Section>,
    pub issues: Vec<String>,
    pub fixed: usize,
}

impl DoctorReport {
    fn section(&mut self, title: &str) -> &mut Vec<Check> {
        self.sections.push(Section {
            title: title.to_string(),
            checks: Vec::new(),
        });
        &mut self.sections.last_mut().unwrap().checks
    }
}

fn ok(checks: &mut Vec<Check>, text: impl Into<String>) {
    checks.push(Check {
        level: Level::Ok,
        text: text.into(),
        detail: String::new(),
    });
}

fn ok_detail(checks: &mut Vec<Check>, text: impl Into<String>, detail: impl Into<String>) {
    checks.push(Check {
        level: Level::Ok,
        text: text.into(),
        detail: detail.into(),
    });
}

fn warn(checks: &mut Vec<Check>, text: impl Into<String>) {
    checks.push(Check {
        level: Level::Warn,
        text: text.into(),
        detail: String::new(),
    });
}

fn warn_detail(checks: &mut Vec<Check>, text: impl Into<String>, detail: impl Into<String>) {
    checks.push(Check {
        level: Level::Warn,
        text: text.into(),
        detail: detail.into(),
    });
}

fn fail(checks: &mut Vec<Check>, text: impl Into<String>) {
    checks.push(Check {
        level: Level::Fail,
        text: text.into(),
        detail: String::new(),
    });
}

fn info(checks: &mut Vec<Check>, text: impl Into<String>) {
    checks.push(Check {
        level: Level::Info,
        text: text.into(),
        detail: String::new(),
    });
}

fn merge(report: &mut DoctorReport, issues: Vec<String>, fixed: usize) {
    report.issues.extend(issues);
    report.fixed += fixed;
}

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

/// Doctor options (hermes `doctor --fix`; `--online`/`--json` are ulnclaw
/// extensions for the Rust port).
#[derive(Debug, Clone, Default)]
pub struct DoctorOptions {
    pub fix: bool,
    pub online: bool,
    pub json: bool,
}

const GREEN: &str = "#2ECC40";
const YELLOW: &str = "#FFC107";
const RED: &str = "#DC3545";
const CYAN: &str = "#00BCD4";
const DIM: &str = "#8B8682";

/// Run all diagnostic checks (hermes `run_doctor`).
pub fn run_doctor(config: &UlncLawConfig, opts: &DoctorOptions) -> DoctorReport {
    let mut report = DoctorReport::default();
    let home = crate::config::ulnclaw_home();

    check_version(&mut report);
    check_config_files(&mut report, config, &home, opts);
    check_directory_structure(&mut report, &home, opts);
    check_auth_providers(&mut report, config, &home);
    check_external_tools(&mut report);
    check_toolsets(&mut report, config);
    check_skills(&mut report, &home);
    check_profiles(&mut report, config, &home);
    if opts.online {
        check_api_connectivity(&mut report, config);
    }
    report
}

// =========================================================================
// Version & Updates
// =========================================================================

fn check_version(report: &mut DoctorReport) {
    let mut issues: Vec<String> = Vec::new();
    let checks = report.section("Version & Updates");
    ok(checks, format!("ulnclaw v{}", crate::VERSION));
    if let Some(state) = crate::banner::get_git_banner_state() {
        let mut text = format!("git checkout: upstream {}", state.upstream);
        if state.ahead > 0 {
            text.push_str(&format!(
                " · local {} (+{} carried {})",
                state.local,
                state.ahead,
                if state.ahead == 1 {
                    "commit"
                } else {
                    "commits"
                }
            ));
        }
        info(checks, text);
    }
    match crate::banner::check_for_updates() {
        Some(0) => ok(checks, "up to date with upstream"),
        Some(n) if n > 0 => {
            let word = if n == 1 { "commit" } else { "commits" };
            warn_detail(
                checks,
                format!("{n} {word} behind upstream"),
                format!(
                    "run {} to update",
                    crate::banner::recommended_update_command()
                ),
            );
            issues.push(format!(
                "ulnclaw is {n} {word} behind upstream — run {}",
                crate::banner::recommended_update_command()
            ));
        }
        Some(_) => {
            warn(checks, "update available (commit count unknown)");
            issues.push("an ulnclaw update is available".to_string());
        }
        None => info(
            checks,
            "update check not applicable (no git checkout found)",
        ),
    }
    merge(report, issues, 0);
}

// =========================================================================
// Configuration Files
// =========================================================================

fn check_config_files(
    report: &mut DoctorReport,
    config: &UlncLawConfig,
    home: &Path,
    opts: &DoctorOptions,
) {
    let mut issues: Vec<String> = Vec::new();
    let mut fixed = 0usize;
    let checks = report.section("Configuration Files");
    let home_label = home.display().to_string();
    let config_path = home.join("config.toml");

    if config_path.exists() {
        ok(checks, format!("{home_label}/config.toml exists"));
        match std::fs::read_to_string(&config_path) {
            Ok(content) => match content.parse::<toml::Value>() {
                Ok(_) => ok(checks, "config.toml parses as valid TOML"),
                Err(e) => {
                    fail(checks, format!("config.toml has TOML errors: {e}"));
                    issues.push("fix the TOML syntax errors in config.toml".to_string());
                }
            },
            Err(e) => {
                fail(checks, format!("config.toml unreadable: {e}"));
                issues.push("config.toml is unreadable".to_string());
            }
        }
    } else if opts.fix {
        match crate::config::UlncLawConfig::write_default_if_missing() {
            Ok(path) => {
                ok(checks, format!("created default {}", path.display()));
                fixed += 1;
            }
            Err(e) => {
                fail(checks, format!("could not create default config: {e}"));
                issues.push("create config.toml (run 'ulnclaw init')".to_string());
            }
        }
    } else {
        fail(checks, format!("{home_label}/config.toml missing"));
        info(checks, "run 'ulnclaw init' to create a default config");
        issues.push("run 'ulnclaw init' to create config.toml".to_string());
    }

    if config.model.model.trim().is_empty() {
        fail(checks, "no model configured");
        info(
            checks,
            "set model.model in config.toml (or run 'ulnclaw init')",
        );
        issues.push("configure model.model in config.toml".to_string());
    } else {
        ok(
            checks,
            format!("model: {} ({})", config.model.model, config.model.provider),
        );
    }

    // .env file (optional env source).
    let env_path = home.join(".env");
    if env_path.exists() {
        match std::fs::read_to_string(&env_path) {
            Ok(content) => {
                let has_key = content.lines().any(|line| {
                    let trimmed = line.trim();
                    !trimmed.starts_with('#')
                        && trimmed.contains('=')
                        && (trimmed.contains("API_KEY") || trimmed.contains("_TOKEN"))
                });
                if has_key {
                    ok(
                        checks,
                        format!("{home_label}/.env exists (API key present)"),
                    );
                } else {
                    warn(
                        checks,
                        format!("{home_label}/.env exists but holds no API key"),
                    );
                }
            }
            Err(_) => warn(
                checks,
                format!("{home_label}/.env exists but is unreadable"),
            ),
        }
    } else {
        info(
            checks,
            "no .env file (environment variables may be set directly)",
        );
    }
    merge(report, issues, fixed);
}

// =========================================================================
// Directory Structure
// =========================================================================

fn check_directory_structure(report: &mut DoctorReport, home: &Path, opts: &DoctorOptions) {
    let mut issues: Vec<String> = Vec::new();
    let mut fixed = 0usize;
    let checks = report.section("Directory Structure");
    if home.is_dir() {
        ok(checks, format!("home directory: {}", home.display()));
    } else if opts.fix {
        match std::fs::create_dir_all(home) {
            Ok(()) => {
                ok(checks, format!("created home directory {}", home.display()));
                fixed += 1;
            }
            Err(e) => {
                fail(checks, format!("cannot create home directory: {e}"));
                issues.push(format!("create the home directory {}", home.display()));
            }
        }
    } else {
        fail(
            checks,
            format!("home directory missing: {}", home.display()),
        );
        issues.push(format!(
            "home directory {} missing (run 'ulnclaw doctor --fix')",
            home.display()
        ));
    }

    for subdir in [
        "sessions",
        "skills",
        "memory",
        "cron",
        "checkpoints",
        "logs",
    ] {
        let path = home.join(subdir);
        if path.is_dir() {
            ok(checks, format!("{subdir}/ present"));
        } else if opts.fix {
            match std::fs::create_dir_all(&path) {
                Ok(()) => {
                    ok(checks, format!("created {subdir}/"));
                    fixed += 1;
                }
                Err(e) => warn(checks, format!("could not create {subdir}/: {e}")),
            }
        } else {
            info(
                checks,
                format!("{subdir}/ not created yet (created on first use)"),
            );
        }
    }

    // state.db sanity: present → readable header check skipped (sqlite is
    // bundled); only report existence.
    let state_db = home.join("state.db");
    if state_db.exists() {
        ok(checks, "state.db present (session store)");
    } else {
        info(
            checks,
            "state.db not created yet (created on first session)",
        );
    }
    merge(report, issues, fixed);
}

// =========================================================================
// Auth Providers
// =========================================================================

fn check_auth_providers(report: &mut DoctorReport, config: &UlncLawConfig, home: &Path) {
    let mut issues: Vec<String> = Vec::new();
    let checks = report.section("Auth Providers");
    let provider = config.model.provider.as_str();

    if crate::provider::auxiliary::is_keyless(provider) {
        info(
            checks,
            format!("provider '{provider}' runs locally — no API key needed"),
        );
        merge(report, issues, 0);
        return;
    }

    if let Some(source) = resolve_key_source(config, home) {
        ok(checks, format!("API key configured ({source})"));
    } else {
        warn(checks, "no API key found");
        info(checks, "set ULNCLAW_API_KEY / OPENAI_API_KEY / ANTHROPIC_API_KEY, or model.api_key in config.toml");
        issues.push("configure an API key (ULNCLAW_API_KEY / OPENAI_API_KEY / ANTHROPIC_API_KEY or config.toml)".to_string());
    }

    let base_url = config.resolve_base_url();
    if config.model.base_url.is_some() {
        info(checks, format!("custom base_url: {base_url}"));
    } else {
        info(checks, format!("base_url: {base_url} (provider default)"));
    }
    merge(report, issues, 0);
}

/// Where the API key came from, mirroring `UlncLawConfig::resolve_api_key`.
fn resolve_key_source(config: &UlncLawConfig, home: &Path) -> Option<String> {
    if config
        .model
        .api_key
        .as_deref()
        .map(str::trim)
        .map(|k| !k.is_empty())
        .unwrap_or(false)
    {
        return Some("config.toml model.api_key".to_string());
    }
    for var in ["ULNCLAW_API_KEY", "OPENAI_API_KEY", "ANTHROPIC_API_KEY"] {
        if crate::config::get_env_value(var)
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
        {
            return Some(format!("env {var}"));
        }
    }
    // .env in home is read through get_env_value on load; double-check the
    // file directly for diagnostics.
    let env_path = home.join(".env");
    if let Ok(content) = std::fs::read_to_string(env_path) {
        for var in ["ULNCLAW_API_KEY", "OPENAI_API_KEY", "ANTHROPIC_API_KEY"] {
            if content.lines().any(|line| {
                line.trim().starts_with(var)
                    && line.contains('=')
                    && !line.trim_start().starts_with('#')
            }) {
                return Some(format!("{var} in .env"));
            }
        }
    }
    None
}

// =========================================================================
// External Tools
// =========================================================================

fn which(cmd: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(cmd);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn check_external_tools(report: &mut DoctorReport) {
    let mut issues: Vec<String> = Vec::new();
    let checks = report.section("External Tools");

    match which("git") {
        Some(path) => ok_detail(checks, "git found", path.display().to_string()),
        None => {
            warn(checks, "git not found on PATH");
            info(
                checks,
                "checkpoints, /gitdiff and the git update check need git",
            );
            issues.push("install git for checkpoint/diff/update support".to_string());
        }
    }

    let candidates = crate::browser::connect::get_chrome_debug_candidates(
        crate::browser::connect::host_system(),
    );
    match candidates.first() {
        Some(path) => ok_detail(
            checks,
            "Chromium-family browser found",
            path.display().to_string(),
        ),
        None => {
            warn(checks, "no Chromium-family browser binary found");
            info(
                checks,
                "browser auto mode cannot launch; set ULNCLAW_BROWSER_PATH or ULNCLAW_BROWSER_CDP",
            );
        }
    }

    ok(
        checks,
        "SQLite bundled (rusqlite) — no system library required",
    );
    merge(report, issues, 0);
}

// =========================================================================
// Toolsets
// =========================================================================

fn check_toolsets(report: &mut DoctorReport, config: &UlncLawConfig) {
    let mut issues: Vec<String> = Vec::new();
    let checks = report.section("Toolsets");
    let enabled: Vec<String> = if config.enabled_toolsets.is_empty() {
        vec!["coding".to_string()]
    } else {
        config.enabled_toolsets.clone()
    };
    ok(checks, format!("enabled: {}", enabled.join(", ")));
    if !config.disabled_toolsets.is_empty() {
        info(
            checks,
            format!("disabled: {}", config.disabled_toolsets.join(", ")),
        );
    }
    // Resolve to concrete tool names so misnamed toolsets surface.
    let mut unknown = Vec::new();
    for name in &enabled {
        if crate::toolsets::resolve_toolset(name).is_empty()
            && !config.disabled_toolsets.contains(name)
        {
            unknown.push(name.clone());
        }
    }
    for name in unknown {
        warn(
            checks,
            format!("toolset '{name}' resolved to no tools (unknown name?)"),
        );
        issues.push(format!("toolset '{name}' is enabled but unknown"));
    }
    merge(report, issues, 0);
}

// =========================================================================
// Skills
// =========================================================================

fn check_skills(report: &mut DoctorReport, home: &Path) {
    let checks = report.section("Skills");
    let skills_dir = home.join("skills");
    if !skills_dir.is_dir() {
        info(
            checks,
            "no skills directory yet (skills/ is created on first install)",
        );
        return;
    }
    let skills = crate::skills::list_skills(&skills_dir);
    if skills.is_empty() {
        info(checks, "skills/ exists but no skills installed");
        return;
    }
    ok(checks, format!("{} skill(s) installed", skills.len()));
    // Skills whose frontmatter had no name (list_skills keeps directory
    // names as fallback — flag empties explicitly).
    for skill in &skills {
        if skill.name.trim().is_empty() {
            warn(
                checks,
                format!(
                    "skill directory {} has no name in SKILL.md frontmatter",
                    skill.path.display()
                ),
            );
        }
    }
}

// =========================================================================
// Profiles
// =========================================================================

fn check_profiles(report: &mut DoctorReport, config: &UlncLawConfig, home: &Path) {
    if config.profiles.is_empty() {
        return;
    }
    let checks = report.section("Profiles");
    ok(
        checks,
        format!("{} profile(s) configured", config.profiles.len()),
    );
    let mut names: Vec<&String> = config.profiles.keys().collect();
    names.sort();
    for name in names {
        let profile = &config.profiles[name];
        let profile_home = home.join("profiles").join(name);
        let mut parts: Vec<String> = Vec::new();
        if let Some(model) = &profile.model {
            parts.push(model.model.clone());
        }
        if profile.enabled_toolsets.is_some() || profile.disabled_toolsets.is_some() {
            parts.push("toolset overrides".to_string());
        }
        if !profile_home.is_dir() {
            parts.push("profile home not created yet".to_string());
        }
        let status = if parts.is_empty() {
            "configured".to_string()
        } else {
            parts.join(", ")
        };
        ok(checks, format!("  {name}: {status}"));
    }
}

// =========================================================================
// API Connectivity (--online)
// =========================================================================

fn check_api_connectivity(report: &mut DoctorReport, config: &UlncLawConfig) {
    let checks = report.section("API Connectivity");
    let provider = config.model.provider.as_str();
    if crate::provider::auxiliary::is_keyless(provider) {
        // Ollama-style local server: probe /api/tags.
        let base = config.resolve_base_url();
        let url = format!("{}/api/tags", base.trim_end_matches('/'));
        probe_url(
            checks,
            &url,
            None,
            &format!("provider '{provider}' at {base}"),
        );
        return;
    }
    let base = config.resolve_base_url();
    let url = format!("{}/v1/models", base.trim_end_matches('/'));
    let key = config.resolve_api_key();
    probe_url(
        checks,
        &url,
        key.as_deref(),
        &format!("provider '{}' at {base}", config.model.provider),
    );
}

fn probe_url(checks: &mut Vec<Check>, url: &str, bearer: Option<&str>, label: &str) {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .connect_timeout(Duration::from_secs(5))
        .build();
    let Ok(client) = client else {
        warn(checks, "could not build HTTP client for connectivity probe");
        return;
    };
    let mut request = client.get(url);
    if let Some(key) = bearer {
        request = request.bearer_auth(key);
    }
    match request.send() {
        Ok(resp) if resp.status().is_success() => {
            ok(
                checks,
                format!("{label} reachable (HTTP {})", resp.status()),
            );
        }
        Ok(resp) => {
            warn(
                checks,
                format!(
                    "{label} answered HTTP {} — check credentials/base_url",
                    resp.status()
                ),
            );
        }
        Err(e) => {
            warn(checks, format!("{label} unreachable: {e}"));
        }
    }
}

// =========================================================================
// Rendering
// =========================================================================

impl DoctorReport {
    /// Plain-text/ANSI rendering (hermes boxed banner + sections + summary).
    pub fn render(&self) -> String {
        let enabled = color_enabled();
        let mut out = String::new();
        out.push('\n');
        let border = "┌─────────────────────────────────────────────────────────┐";
        let title = "│                 🩺 ulnclaw Doctor                        │";
        let bottom = "└─────────────────────────────────────────────────────────┘";
        out.push_str(&paint(CYAN, false, border, enabled));
        out.push('\n');
        out.push_str(&paint(CYAN, false, title, enabled));
        out.push('\n');
        out.push_str(&paint(CYAN, false, bottom, enabled));
        out.push('\n');

        for section in &self.sections {
            out.push('\n');
            out.push_str(&paint(CYAN, true, &section.title, enabled));
            out.push('\n');
            for check in &section.checks {
                let (glyph, color) = match check.level {
                    Level::Ok => ("✓", GREEN),
                    Level::Warn => ("⚠", YELLOW),
                    Level::Fail => ("✗", RED),
                    Level::Info => ("ℹ", DIM),
                };
                out.push_str(&format!(
                    "  {} {}\n",
                    paint(color, false, glyph, enabled),
                    check.text
                ));
                if !check.detail.is_empty() {
                    out.push_str(&format!(
                        "      {}\n",
                        paint(DIM, false, &check.detail, enabled)
                    ));
                }
            }
        }

        out.push('\n');
        let rule = "─".repeat(60);
        if !self.issues.is_empty() {
            out.push_str(&paint(YELLOW, false, &rule, enabled));
            out.push('\n');
            out.push_str(&paint(
                YELLOW,
                true,
                &format!("  Found {} issue(s) to address:", self.issues.len()),
                enabled,
            ));
            out.push_str("\n\n");
            for (i, issue) in self.issues.iter().enumerate() {
                out.push_str(&format!("  {}. {}\n", i + 1, issue));
            }
            out.push('\n');
            if self.fixed > 0 {
                out.push_str(&paint(
                    GREEN,
                    true,
                    &format!("  Fixed {} issue(s).", self.fixed),
                    enabled,
                ));
                out.push('\n');
            }
            out.push_str(&paint(
                DIM,
                false,
                "  Tip: run 'ulnclaw doctor --fix' to auto-fix what's possible.",
                enabled,
            ));
            out.push('\n');
        } else {
            out.push_str(&paint(GREEN, false, &rule, enabled));
            out.push('\n');
            if self.fixed > 0 {
                out.push_str(&paint(
                    GREEN,
                    true,
                    &format!("  Fixed {} issue(s).", self.fixed),
                    enabled,
                ));
                out.push('\n');
            }
            out.push_str(&paint(GREEN, true, "  All checks passed! 🎉", enabled));
            out.push('\n');
        }
        out.push('\n');
        out
    }

    /// True when any check failed (JSON consumers; the CLI always exits 0,
    /// matching hermes).
    pub fn has_failures(&self) -> bool {
        self.sections
            .iter()
            .flat_map(|s| &s.checks)
            .any(|c| c.level == Level::Fail)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_config() -> UlncLawConfig {
        let mut config = UlncLawConfig::default();
        config.model.provider = "ollama".to_string();
        config.model.model = "qwen2.5:14b".to_string();
        config
    }

    #[test]
    fn report_renders_all_levels() {
        let mut report = DoctorReport::default();
        let checks = report.section("Test");
        ok(checks, "fine");
        warn(checks, "careful");
        fail(checks, "broken");
        info(checks, "note");
        let text = report.render();
        assert!(text.contains("ulnclaw Doctor"));
        assert!(text.contains("fine"));
        assert!(text.contains("broken"));
        assert!(report.has_failures());
    }

    #[test]
    fn summary_lists_issues_or_celebrates() {
        let mut report = DoctorReport::default();
        report.section("Empty");
        let text = report.render();
        assert!(text.contains("All checks passed"));

        report.issues.push("do the thing".to_string());
        let text = report.render();
        assert!(text.contains("Found 1 issue(s)"));
        assert!(text.contains("1. do the thing"));
        assert!(text.contains("--fix"));
    }

    #[test]
    fn keyless_provider_skips_key_check() {
        let _guard = crate::models_dev::test_env_lock();
        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var("ULNCLAW_HOME").ok();
        std::env::set_var("ULNCLAW_HOME", dir.path());
        let config = minimal_config();
        let report = run_doctor(&config, &DoctorOptions::default());
        let auth = report
            .sections
            .iter()
            .find(|s| s.title == "Auth Providers")
            .unwrap();
        assert!(auth
            .checks
            .iter()
            .any(|c| c.text.contains("no API key needed")));
        match prev {
            Some(v) => std::env::set_var("ULNCLAW_HOME", v),
            None => std::env::remove_var("ULNCLAW_HOME"),
        }
    }

    #[test]
    fn missing_model_flagged() {
        let _guard = crate::models_dev::test_env_lock();
        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var("ULNCLAW_HOME").ok();
        std::env::set_var("ULNCLAW_HOME", dir.path());
        let mut config = minimal_config();
        config.model.model = String::new();
        let report = run_doctor(&config, &DoctorOptions::default());
        assert!(report.issues.iter().any(|i| i.contains("model.model")));
        assert!(report.has_failures());
        match prev {
            Some(v) => std::env::set_var("ULNCLAW_HOME", v),
            None => std::env::remove_var("ULNCLAW_HOME"),
        }
    }

    #[test]
    fn fix_creates_directories_and_config() {
        let _guard = crate::models_dev::test_env_lock();
        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var("ULNCLAW_HOME").ok();
        std::env::set_var("ULNCLAW_HOME", dir.path());
        let config = minimal_config();
        let opts = DoctorOptions {
            fix: true,
            ..Default::default()
        };
        let report = run_doctor(&config, &opts);
        assert!(dir.path().join("config.toml").exists());
        assert!(dir.path().join("skills").is_dir());
        assert!(dir.path().join("sessions").is_dir());
        assert!(report.fixed > 0);
        match prev {
            Some(v) => std::env::set_var("ULNCLAW_HOME", v),
            None => std::env::remove_var("ULNCLAW_HOME"),
        }
    }

    #[test]
    fn unknown_toolset_flagged() {
        let _guard = crate::models_dev::test_env_lock();
        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var("ULNCLAW_HOME").ok();
        std::env::set_var("ULNCLAW_HOME", dir.path());
        let mut config = minimal_config();
        config.enabled_toolsets = vec!["definitely_not_a_toolset".to_string()];
        let report = run_doctor(&config, &DoctorOptions::default());
        assert!(report
            .issues
            .iter()
            .any(|i| i.contains("definitely_not_a_toolset")));
        match prev {
            Some(v) => std::env::set_var("ULNCLAW_HOME", v),
            None => std::env::remove_var("ULNCLAW_HOME"),
        }
    }

    #[test]
    fn json_output_round_trips() {
        let mut report = DoctorReport::default();
        let checks = report.section("S");
        ok_detail(checks, "text", "detail");
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["sections"][0]["title"], "S");
        assert_eq!(json["sections"][0]["checks"][0]["level"], "ok");
        assert_eq!(json["sections"][0]["checks"][0]["detail"], "detail");
    }
}

//! Gateway runtime-metadata footer — port of hermes
//! `gateway/runtime_footer.py`.
//!
//! Renders a compact footer showing runtime state (model, context %,
//! cwd, latency) and the messaging dispatcher appends it to the FINAL
//! reply of an agent turn when enabled. Off by default to keep
//! replies minimal.
//!
//! Config (`config.toml`):
//!
//! ```toml
//! [display.runtime_footer]
//! enabled = true                        # off by default
//! fields = ["model", "context_pct", "cwd"]   # order shown; drop any to hide
//! ```
//!
//! Available fields:
//! - `model`       — bare model id, vendor prefix dropped (`gpt-5.4`)
//! - `context_pct` — last-turn context occupancy as a percent (`5%`)
//! - `latency`     — wall-clock duration of the turn (`22s`, `1m05s`)
//! - `cwd`         — home-relative working dir (`~`)
//!
//! `latency` is opt-in: it is NOT in the default field set, so a
//! footer whose `fields` are unset renders exactly as before.
//!
//! Per-platform overrides live under
//! `[display.platforms.<platform>.runtime_footer]`. Users toggle the
//! global setting with `/footer on|off` from any gateway platform;
//! the toggle persists to `config.toml` and latches the running
//! process so it applies immediately.

use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

/// Default field order (hermes `_DEFAULT_FIELDS`).
pub const DEFAULT_FIELDS: &[&str] = &["model", "context_pct", "cwd"];

const SEP: &str = " \u{b7} ";

/// `[display.runtime_footer]` (hermes `display.runtime_footer`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeFooterConfig {
    pub enabled: bool,
    /// Display order; empty = [`DEFAULT_FIELDS`].
    pub fields: Vec<String>,
}

impl Default for RuntimeFooterConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            fields: Vec::new(),
        }
    }
}

/// `[display.platforms.<platform>]` — per-platform partial overrides
/// (hermes `display.platforms.<key>.runtime_footer` merge semantics:
/// only keys present override the global values).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeFooterOverride {
    pub enabled: Option<bool>,
    pub fields: Vec<String>,
}

/// Resolved effective footer settings.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedFooterConfig {
    pub enabled: bool,
    pub fields: Vec<String>,
}

/// Drop `vendor/` prefix for readability (`openai/gpt-5.4` → `gpt-5.4`).
pub fn model_short(model: &str) -> String {
    model.rsplit('/').next().unwrap_or("").to_string()
}

/// Return *cwd* with `$HOME` collapsed to `~`; empty string if unset.
pub fn home_relative_cwd(cwd: &str) -> String {
    if cwd.is_empty() {
        return String::new();
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_default();
    if !home.is_empty() {
        if cwd == home {
            return "~".to_string();
        }
        if let Some(rest) = cwd.strip_prefix(&format!("{home}/")) {
            return format!("~/{rest}");
        }
    }
    cwd.to_string()
}

/// Humanize a turn duration: `<1s`, `22s`, `1m05s`.
pub fn format_latency(seconds: f64) -> String {
    if seconds < 1.0 {
        return "<1s".to_string();
    }
    let total = seconds.round() as i64;
    if total < 60 {
        return format!("{total}s");
    }
    format!("{}m{:02}s", total / 60, total % 60)
}

/// Resolve effective runtime-footer config for *platform_key*.
///
/// Merge order (later wins):
/// 1. Built-in defaults (enabled=false, default fields)
/// 2. `[display.runtime_footer]`
/// 3. `[display.platforms.<platform_key>.runtime_footer]`
/// 4. The in-process `/footer` latch (set by the toggle command so a
///    config write applies immediately without a gateway restart).
pub fn resolve_footer_config(
    display: &crate::config::DisplayConfig,
    platform_key: Option<&str>,
) -> ResolvedFooterConfig {
    let mut enabled = display.runtime_footer.enabled;
    let mut fields = if display.runtime_footer.fields.is_empty() {
        DEFAULT_FIELDS.iter().map(|s| s.to_string()).collect()
    } else {
        display.runtime_footer.fields.clone()
    };
    if let Some(key) = platform_key {
        if let Some(override_cfg) = display.platforms.get(key) {
            if let Some(flag) = override_cfg.runtime_footer.enabled {
                enabled = flag;
            }
            if !override_cfg.runtime_footer.fields.is_empty() {
                fields = override_cfg.runtime_footer.fields.clone();
            }
        }
    }
    if let Some(latch) = enabled_latch() {
        enabled = latch;
    }
    ResolvedFooterConfig { enabled, fields }
}

/// Render the footer line, or return "" if no fields have data.
///
/// Fields are skipped silently when their underlying data is missing —
/// a partially-populated footer is better than a line with `?%` or
/// empty slots. Unknown field names are silently ignored.
pub fn format_runtime_footer(
    model: Option<&str>,
    context_tokens: u64,
    context_length: Option<u64>,
    cwd: Option<&str>,
    turn_seconds: Option<f64>,
    fields: &[String],
) -> String {
    let mut parts: Vec<String> = Vec::new();
    for field in fields {
        match field.as_str() {
            "model" => {
                let m = model.map(model_short).unwrap_or_default();
                if !m.is_empty() {
                    parts.push(m);
                }
            }
            "context_pct" => {
                if let Some(length) = context_length {
                    if length > 0 {
                        let pct = ((context_tokens as f64 / length as f64) * 100.0)
                            .round()
                            .clamp(0.0, 100.0) as u64;
                        parts.push(format!("{pct}%"));
                    }
                }
            }
            "latency" => {
                if let Some(seconds) = turn_seconds {
                    if seconds >= 0.0 {
                        parts.push(format_latency(seconds));
                    }
                }
            }
            "cwd" => {
                let raw = cwd
                    .map(str::to_string)
                    .filter(|s| !s.is_empty())
                    .or_else(|| std::env::var("TERMINAL_CWD").ok())
                    .unwrap_or_default();
                let rel = home_relative_cwd(&raw);
                if !rel.is_empty() {
                    parts.push(rel);
                }
            }
            _ => {}
        }
    }
    if parts.is_empty() {
        return String::new();
    }
    parts.join(SEP)
}

/// Top-level entry point used by the messaging dispatcher (hermes
/// `build_footer_line`). Returns the footer text (empty when disabled
/// or no data); callers append it to the final reply themselves,
/// preserving a single blank line of separation.
pub fn build_footer_line(
    display: &crate::config::DisplayConfig,
    platform_key: Option<&str>,
    model: Option<&str>,
    context_tokens: u64,
    context_length: Option<u64>,
    cwd: Option<&str>,
    turn_seconds: Option<f64>,
) -> String {
    let resolved = resolve_footer_config(display, platform_key);
    if !resolved.enabled {
        return String::new();
    }
    format_runtime_footer(
        model,
        context_tokens,
        context_length,
        cwd,
        turn_seconds,
        &resolved.fields,
    )
}

fn latch() -> &'static Mutex<Option<bool>> {
    static LATCH: OnceLock<Mutex<Option<bool>>> = OnceLock::new();
    LATCH.get_or_init(|| Mutex::new(None))
}

/// In-process `/footer` latch (applies before restart).
pub fn enabled_latch() -> Option<bool> {
    *latch().lock().unwrap()
}

/// Set the in-process latch (hermes `/footer` immediate effect).
pub fn set_enabled_latch(enabled: bool) {
    *latch().lock().unwrap() = Some(enabled);
}

/// Test-only latch clear.
#[doc(hidden)]
pub fn clear_enabled_latch_for_tests() {
    *latch().lock().unwrap() = None;
}

/// Persist the global flag to `config.toml` (hermes `/footer` writes
/// `display.runtime_footer.enabled`).
pub fn persist_enabled(enabled: bool) -> std::result::Result<(), String> {
    let path = crate::config_cmd::config_path();
    let mut toml = crate::config_cmd::load_toml(&path)?;
    crate::config_cmd::set_nested(
        &mut toml,
        "display.runtime_footer.enabled",
        toml::Value::Boolean(enabled),
    )?;
    crate::config_cmd::save_toml(&path, &toml)
}

/// `/footer` command handler (hermes `_handle_footer_command`).
///
/// Usage: `/footer` toggles, `/footer on|off` sets, `/footer status`
/// shows the effective state + fields. The global flag persists to
/// `config.toml`; per-platform overrides are respected but not
/// modified here.
pub fn handle_footer_command(
    arg: &str,
    display: &crate::config::DisplayConfig,
    platform_key: Option<&str>,
    model: Option<&str>,
) -> String {
    let arg = arg.trim().to_lowercase();
    let effective = resolve_footer_config(display, platform_key);

    if arg == "status" || arg == "?" {
        let state = if effective.enabled { "on" } else { "off" };
        return format!(
            "runtime footer: {state} · fields: {} · platform: {}",
            effective.fields.join(", "),
            platform_key.unwrap_or("(global)"),
        );
    }

    let new_state = if matches!(arg.as_str(), "on" | "enable" | "true" | "1") {
        true
    } else if matches!(arg.as_str(), "off" | "disable" | "false" | "0") {
        false
    } else if arg.is_empty() {
        !effective.enabled
    } else {
        return "usage: /footer [on|off|status]".to_string();
    };

    if let Err(e) = persist_enabled(new_state) {
        return format!("runtime footer: could not save config ({e})");
    }
    set_enabled_latch(new_state);

    if new_state {
        // Preview with the current model so the user sees the shape.
        let preview = format_runtime_footer(
            model,
            0,
            None,
            Some(
                &std::env::current_dir()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default(),
            ),
            None,
            &effective.fields,
        );
        let preview_note = if preview.is_empty() {
            String::new()
        } else {
            format!(" Example: {preview}")
        };
        format!(
            "runtime footer enabled (fields: {}).{preview_note}",
            effective.fields.join(", ")
        )
    } else {
        "runtime footer disabled.".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DisplayConfig;

    fn display_with(enabled: bool, fields: Vec<&str>) -> DisplayConfig {
        let mut display = DisplayConfig::default();
        display.runtime_footer.enabled = enabled;
        display.runtime_footer.fields = fields.into_iter().map(str::to_string).collect();
        display
    }

    #[test]
    fn model_short_drops_vendor_prefix() {
        assert_eq!(model_short("openai/gpt-5.4"), "gpt-5.4");
        assert_eq!(model_short("gpt-5.4"), "gpt-5.4");
        assert_eq!(model_short(""), "");
    }

    #[test]
    fn home_relative_cwd_collapses_home() {
        std::env::set_var("HOME", "/home/tester");
        assert_eq!(home_relative_cwd("/home/tester"), "~");
        assert_eq!(home_relative_cwd("/home/tester/work"), "~/work");
        assert_eq!(home_relative_cwd("/srv/app"), "/srv/app");
        assert_eq!(home_relative_cwd(""), "");
    }

    #[test]
    fn latency_humanizes() {
        assert_eq!(format_latency(0.4), "<1s");
        assert_eq!(format_latency(22.0), "22s");
        assert_eq!(format_latency(65.0), "1m05s");
        assert_eq!(format_latency(3661.0), "61m01s");
    }

    #[test]
    fn format_skips_fields_without_data() {
        let fields: Vec<String> = vec![
            "model".into(),
            "context_pct".into(),
            "latency".into(),
            "cwd".into(),
            "bogus".into(),
        ];
        // Full data.
        let line = format_runtime_footer(
            Some("openai/gpt-5.4"),
            10_000,
            Some(200_000),
            Some("/srv/app"),
            Some(22.0),
            &fields,
        );
        assert_eq!(line, "gpt-5.4 \u{b7} 5% \u{b7} 22s \u{b7} /srv/app");
        // Missing context window + latency → silently skipped.
        let line = format_runtime_footer(Some("gpt-5.4"), 0, None, Some("/srv"), None, &fields);
        assert_eq!(line, "gpt-5.4 \u{b7} /srv");
        // Nothing at all → empty.
        let line = format_runtime_footer(None, 0, None, Some(""), None, &fields);
        assert_eq!(line, "");
        // Percent clamps at 100.
        let fields = vec!["context_pct".to_string()];
        assert_eq!(
            format_runtime_footer(None, 999_999, Some(1_000), None, None, &fields),
            "100%"
        );
    }

    #[test]
    fn resolve_merges_global_platform_and_latch() {
        let _guard = crate::models_dev::test_env_lock();
        clear_enabled_latch_for_tests();
        let mut display = display_with(false, vec!["model"]);
        let override_cfg = RuntimeFooterOverride {
            enabled: Some(true),
            fields: vec!["latency".into()],
        };
        display.platforms.insert(
            "whatsapp".into(),
            crate::config::PlatformDisplayOverride {
                runtime_footer: override_cfg,
                ..Default::default()
            },
        );

        // Global only: disabled, default fields replaced by the global list.
        let resolved = resolve_footer_config(&display, None);
        assert!(!resolved.enabled);
        assert_eq!(resolved.fields, vec!["model"]);
        // Platform override wins on both axes.
        let resolved = resolve_footer_config(&display, Some("whatsapp"));
        assert!(resolved.enabled);
        assert_eq!(resolved.fields, vec!["latency"]);
        // Unknown platform falls back to global.
        let resolved = resolve_footer_config(&display, Some("telegram"));
        assert!(!resolved.enabled);
        // The /footer latch trumps file config.
        set_enabled_latch(false);
        let resolved = resolve_footer_config(&display, Some("whatsapp"));
        assert!(!resolved.enabled);
        clear_enabled_latch_for_tests();
    }

    #[test]
    fn empty_fields_fall_back_to_defaults() {
        let display = display_with(true, vec![]);
        let resolved = resolve_footer_config(&display, None);
        assert_eq!(resolved.fields, vec!["model", "context_pct", "cwd"]);
    }

    #[test]
    fn build_footer_line_respects_enabled_gate() {
        let _guard = crate::models_dev::test_env_lock();
        clear_enabled_latch_for_tests();
        let display = display_with(false, vec!["model"]);
        assert_eq!(
            build_footer_line(&display, None, Some("x/y"), 0, None, None, None),
            ""
        );
        set_enabled_latch(true);
        assert_eq!(
            build_footer_line(&display, None, Some("x/y"), 0, None, None, None),
            "y"
        );
        clear_enabled_latch_for_tests();
    }

    #[test]
    fn footer_command_reports_status_and_usage() {
        let _guard = crate::models_dev::test_env_lock();
        clear_enabled_latch_for_tests();
        let display = display_with(true, vec!["model", "cwd"]);
        let status = handle_footer_command("status", &display, Some("telegram"), Some("p/m"));
        assert!(status.contains("runtime footer: on"), "{status}");
        assert!(status.contains("model, cwd"), "{status}");
        assert!(status.contains("telegram"), "{status}");
        let usage = handle_footer_command("sideways", &display, None, None);
        assert_eq!(usage, "usage: /footer [on|off|status]");
        clear_enabled_latch_for_tests();
    }

    #[test]
    fn footer_command_persists_and_latches() {
        let _guard = crate::models_dev::test_env_lock();
        clear_enabled_latch_for_tests();
        let temp = tempfile::tempdir().expect("tempdir");
        std::env::set_var("ULNCLAW_HOME", temp.path());
        let display = DisplayConfig::default();
        let reply = handle_footer_command("on", &display, None, Some("openai/gpt-5.4"));
        assert!(reply.starts_with("runtime footer enabled"), "{reply}");
        assert_eq!(enabled_latch(), Some(true));
        // The flag landed in config.toml.
        let toml = crate::config_cmd::load_toml(&crate::config_cmd::config_path()).unwrap();
        let stored = crate::config_cmd::get_nested(&toml, "display.runtime_footer.enabled");
        assert_eq!(stored, Some(&toml::Value::Boolean(true)));
        // Toggle back off.
        let reply = handle_footer_command("", &display, None, None);
        assert_eq!(reply, "runtime footer disabled.");
        assert_eq!(enabled_latch(), Some(false));
        std::env::remove_var("ULNCLAW_HOME");
        clear_enabled_latch_for_tests();
    }
}

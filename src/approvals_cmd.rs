//! `ulnclaw approvals [manual|smart|off]` — inspect or persist the terminal
//! approval mode (hermes `hermes_cli/approval_mode.py` parity).
//!
//! Approval mode is persistent configuration, not conversation state: it is
//! read from `config.toml` when a run starts, so changes take effect in new
//! sessions (hermes reloads config per check; ulnclaw agents snapshot it).

use crate::config::UlncLawConfig;
use crate::tools::approval::{parse_approval_mode, ApprovalMode};

/// Modes accepted by the command (hermes `VALID_APPROVAL_MODES`).
pub const VALID_MODES: &[&str] = &["manual", "smart", "off"];

/// Command outcome (hermes `ApprovalModeResult`).
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalModeResult {
    pub ok: bool,
    pub mode: String,
    pub changed: bool,
    pub message: String,
}

/// The mode the terminal approval guard would enforce for `config`.
pub fn effective_mode(config: &UlncLawConfig) -> &'static str {
    match parse_approval_mode(&config.approvals.mode) {
        ApprovalMode::Smart => "smart",
        ApprovalMode::Off => "off",
        ApprovalMode::Manual => "manual",
    }
}

/// Inspect or persist `approvals.mode` through the canonical config API
/// (hermes `run_approval_mode_command`).
pub fn run_approval_mode_command(requested: Option<&str>) -> ApprovalModeResult {
    let current =
        effective_mode(&UlncLawConfig::load(None).unwrap_or_else(|_| UlncLawConfig::default()))
            .to_string();
    let requested = requested.unwrap_or("").trim().to_ascii_lowercase();

    if requested.is_empty() {
        let message = format!("Approval mode: {current} (persistent setting in config.toml).");
        return ApprovalModeResult {
            ok: true,
            mode: current,
            changed: false,
            message,
        };
    }
    if !VALID_MODES.contains(&requested.as_str()) {
        let message = "Usage: ulnclaw approvals [manual|smart|off]".to_string();
        return ApprovalModeResult {
            ok: false,
            mode: current,
            changed: false,
            message,
        };
    }

    if let Err(err) = crate::config_cmd::set_config_value("approvals.mode", &requested, false) {
        let message = format!("Failed to save approval mode: {err}");
        return ApprovalModeResult {
            ok: false,
            mode: current,
            changed: false,
            message,
        };
    }

    let effective =
        effective_mode(&UlncLawConfig::load(None).unwrap_or_else(|_| UlncLawConfig::default()))
            .to_string();
    if effective != requested {
        let message = format!(
            "Approval mode remains {effective}; the requested value did not become effective."
        );
        return ApprovalModeResult {
            ok: false,
            mode: effective,
            changed: false,
            message,
        };
    }
    let changed = effective != current;
    let message = format!(
        "Approval mode: {effective} (persistent setting in config.toml; takes effect in new sessions)."
    );
    ApprovalModeResult {
        ok: true,
        mode: effective,
        changed,
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_home<F: FnOnce()>(dir: &std::path::Path, f: F) {
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
    fn effective_mode_maps_config_values() {
        let mut config = UlncLawConfig::default();
        config.approvals.mode = "smart".to_string();
        assert_eq!(effective_mode(&config), "smart");
        config.approvals.mode = "off".to_string();
        assert_eq!(effective_mode(&config), "off");
        config.approvals.mode = "manual".to_string();
        assert_eq!(effective_mode(&config), "manual");
        config.approvals.mode = "bogus".to_string();
        assert_eq!(effective_mode(&config), "manual");
    }

    #[test]
    fn no_argument_reports_current_mode() {
        let dir = tempfile::tempdir().unwrap();
        with_home(dir.path(), || {
            std::fs::write(
                dir.path().join("config.toml"),
                "[approvals]\nmode = \"smart\"\n",
            )
            .unwrap();
            let result = run_approval_mode_command(None);
            assert!(result.ok);
            assert_eq!(result.mode, "smart");
            assert!(!result.changed);
            assert!(result.message.contains("smart"), "{}", result.message);
        });
    }

    #[test]
    fn invalid_mode_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        with_home(dir.path(), || {
            let result = run_approval_mode_command(Some("yolo"));
            assert!(!result.ok);
            assert!(result.message.starts_with("Usage:"), "{}", result.message);
        });
    }

    #[test]
    fn set_mode_persists_to_config() {
        let dir = tempfile::tempdir().unwrap();
        with_home(dir.path(), || {
            let result = run_approval_mode_command(Some("off"));
            assert!(result.ok, "{}", result.message);
            assert_eq!(result.mode, "off");
            assert!(result.changed);
            let text = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
            assert!(
                text.contains("mode = \"off\"") || text.contains("mode=\"off\""),
                "{text}"
            );

            // Setting the same value again reports no change.
            let again = run_approval_mode_command(Some("off"));
            assert!(again.ok);
            assert!(!again.changed);
        });
    }

    #[test]
    fn set_mode_defaults_to_manual_when_unset() {
        let dir = tempfile::tempdir().unwrap();
        with_home(dir.path(), || {
            let result = run_approval_mode_command(None);
            assert!(result.ok);
            assert_eq!(result.mode, "manual");
        });
    }
}

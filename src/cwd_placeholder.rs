//! Resolve gateway `terminal.cwd` placeholder values — port of hermes
//! `gateway/cwd_placeholder.py`.
//!
//! When `terminal.cwd` is unset or a placeholder (`.`, `auto`, `cwd`),
//! the gateway must not blindly map the host home directory into
//! container backends. Docker with workspace mounting still needs an
//! explicit host path signal (`MESSAGING_CWD` or an absolute config
//! path) for the terminal tool to map `/host/project` → `/workspace`.

use std::path::PathBuf;

/// Placeholder values for `terminal.cwd` (hermes `CWD_PLACEHOLDERS`).
pub const CWD_PLACEHOLDERS: &[&str] = &[".", "auto", "cwd"];

/// True when `value` is unset or a placeholder.
pub fn is_placeholder(value: &str) -> bool {
    value.is_empty() || CWD_PLACEHOLDERS.contains(&value.trim())
}

fn truthy_env(value: Option<&str>) -> bool {
    matches!(
        value.map(|v| v.trim().to_lowercase()).as_deref(),
        Some("true") | Some("1") | Some("yes")
    )
}

/// Return the effective terminal cwd, or `None` to leave it unset
/// (hermes `resolve_placeholder_terminal_cwd`).
///
/// Cases:
/// - **local** + placeholder → `messaging_cwd` or `home_fallback`
/// - **docker** + placeholder + mount on + host `messaging_cwd` → host
///   path (for the terminal tool's `/workspace` mapping)
/// - **docker** + placeholder + mount off → `None` (sandbox default)
/// - other non-local backends + placeholder → `None`
pub fn resolve_placeholder_terminal_cwd(
    configured_cwd: &str,
    terminal_backend: &str,
    messaging_cwd: Option<&str>,
    docker_mount_cwd_to_workspace: bool,
    home_fallback: &str,
) -> Option<String> {
    let configured = configured_cwd.trim();
    if !configured.is_empty() && !CWD_PLACEHOLDERS.contains(&configured) {
        return Some(configured.to_string());
    }
    let backend = terminal_backend.trim().to_lowercase();
    let backend = if backend.is_empty() {
        "local"
    } else {
        &backend
    };
    let messaging = messaging_cwd.map(str::trim).unwrap_or("");
    match backend {
        "local" => {
            if !messaging.is_empty() {
                Some(messaging.to_string())
            } else {
                Some(home_fallback.to_string())
            }
        }
        "docker" if docker_mount_cwd_to_workspace => {
            if !messaging.is_empty() && !CWD_PLACEHOLDERS.contains(&messaging) {
                Some(messaging.to_string())
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Resolve the messaging gateway's terminal cwd from config + env
/// (ulnclaw integration over [`resolve_placeholder_terminal_cwd`],
/// mirroring the hermes gateway bootstrap).
///
/// Sources:
/// - configured cwd: `TERMINAL_CWD`/`ULNCLAW_TERMINAL_CWD` env override,
///   else `[terminal] cwd`
/// - backend: `[terminal] backend`, `TERMINAL_ENV` env override wins
/// - messaging cwd: `ULNCLAW_MESSAGING_CWD`/`MESSAGING_CWD`
/// - docker mount flag: `[terminal] docker_mount_cwd_to_workspace`,
///   `TERMINAL_DOCKER_MOUNT_CWD_TO_WORKSPACE` env override wins
/// - home fallback: `$HOME`
pub fn resolve_messaging_cwd(config: &crate::config::UlncLawConfig) -> Option<PathBuf> {
    let configured = std::env::var("ULNCLAW_TERMINAL_CWD")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            std::env::var("TERMINAL_CWD")
                .ok()
                .filter(|v| !v.trim().is_empty())
        })
        .or_else(|| config.terminal.cwd.clone());
    let backend = std::env::var("TERMINAL_ENV")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| config.terminal.backend.clone())
        .unwrap_or_else(|| "local".to_string());
    let messaging_cwd = std::env::var("ULNCLAW_MESSAGING_CWD")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            std::env::var("MESSAGING_CWD")
                .ok()
                .filter(|v| !v.trim().is_empty())
        });
    let mount = std::env::var("TERMINAL_DOCKER_MOUNT_CWD_TO_WORKSPACE")
        .ok()
        .map(|v| truthy_env(Some(&v)))
        .unwrap_or(config.terminal.docker_mount_cwd_to_workspace);
    let home_fallback = dirs::home_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| ".".to_string());
    resolve_placeholder_terminal_cwd(
        configured.as_deref().unwrap_or(""),
        &backend,
        messaging_cwd.as_deref(),
        mount,
        &home_fallback,
    )
    .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_cwd_passes_through() {
        assert_eq!(
            resolve_placeholder_terminal_cwd("/srv/project", "docker", None, false, "/home/u"),
            Some("/srv/project".into())
        );
        assert!(!is_placeholder("/srv/project"));
        assert!(is_placeholder("."));
        assert!(is_placeholder("auto"));
        assert!(is_placeholder("cwd"));
        assert!(is_placeholder(""));
    }

    #[test]
    fn local_placeholder_falls_back() {
        // MESSAGING_CWD wins when set…
        assert_eq!(
            resolve_placeholder_terminal_cwd("", "local", Some("/chat/dir"), false, "/home/u"),
            Some("/chat/dir".into())
        );
        // …else the home fallback.
        assert_eq!(
            resolve_placeholder_terminal_cwd("auto", "local", None, false, "/home/u"),
            Some("/home/u".into())
        );
        assert_eq!(
            resolve_placeholder_terminal_cwd(".", "LOCAL", Some("  "), false, "/home/u"),
            Some("/home/u".into())
        );
    }

    #[test]
    fn docker_mount_on_uses_host_messaging_cwd() {
        assert_eq!(
            resolve_placeholder_terminal_cwd(".", "docker", Some("/host/project"), true, "/home/u"),
            Some("/host/project".into())
        );
        // Placeholder messaging cwd is not a host-path signal.
        assert_eq!(
            resolve_placeholder_terminal_cwd(".", "docker", Some("auto"), true, "/home/u"),
            None
        );
        assert_eq!(
            resolve_placeholder_terminal_cwd(".", "docker", None, true, "/home/u"),
            None
        );
    }

    #[test]
    fn docker_mount_off_and_other_backends_unset() {
        assert_eq!(
            resolve_placeholder_terminal_cwd(
                ".",
                "docker",
                Some("/host/project"),
                false,
                "/home/u"
            ),
            None
        );
        assert_eq!(
            resolve_placeholder_terminal_cwd(".", "ssh", Some("/host/project"), true, "/home/u"),
            None
        );
        // Explicit cwd still wins on any backend.
        assert_eq!(
            resolve_placeholder_terminal_cwd("/w", "ssh", None, false, "/home/u"),
            Some("/w".into())
        );
    }

    #[test]
    fn resolve_messaging_cwd_env_overrides() {
        let _lock = crate::models_dev::test_env_lock();
        let mut config = crate::config::UlncLawConfig::default();
        config.terminal.backend = Some("docker".into());
        config.terminal.docker_mount_cwd_to_workspace = true;

        // Clean slate.
        for key in [
            "ULNCLAW_TERMINAL_CWD",
            "TERMINAL_CWD",
            "TERMINAL_ENV",
            "ULNCLAW_MESSAGING_CWD",
            "MESSAGING_CWD",
            "TERMINAL_DOCKER_MOUNT_CWD_TO_WORKSPACE",
        ] {
            std::env::remove_var(key);
        }

        // docker + mount on but no messaging cwd → unset.
        assert_eq!(resolve_messaging_cwd(&config), None);

        std::env::set_var("MESSAGING_CWD", "/host/project");
        assert_eq!(
            resolve_messaging_cwd(&config),
            Some(PathBuf::from("/host/project"))
        );

        // TERMINAL_ENV overrides the backend back to local → home
        // fallback (messaging cwd wins over home).
        std::env::set_var("TERMINAL_ENV", "local");
        assert_eq!(
            resolve_messaging_cwd(&config),
            Some(PathBuf::from("/host/project"))
        );

        // ULNCLAW_TERMINAL_CWD is the canonical explicit cwd.
        std::env::set_var("ULNCLAW_TERMINAL_CWD", "/explicit");
        assert_eq!(
            resolve_messaging_cwd(&config),
            Some(PathBuf::from("/explicit"))
        );

        for key in ["ULNCLAW_TERMINAL_CWD", "TERMINAL_ENV", "MESSAGING_CWD"] {
            std::env::remove_var(key);
        }
    }
}

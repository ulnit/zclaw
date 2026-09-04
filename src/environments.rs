//! Terminal execution environments — Rust-native port of hermes'
//! environments concept: run terminal commands locally (default), inside a
//! Docker container, or on a remote host over SSH.
//!
//! Configuration (`[terminal]` in config.toml):
//! ```toml
//! [terminal]
//! backend = "docker"          # "local" (default) | "docker" | "ssh"
//! container = "devbox"        # docker: existing container name
//! image = "debian:stable"     # docker: auto-create a container from this image
//! ssh_host = "build.example.com"
//! ssh_user = "ci"
//! ssh_port = 22
//! ssh_identity = "~/.ssh/id_ed25519"
//! ```

use crate::config::TerminalConfig;
use crate::error::{AgentError, Result};

/// Resolved terminal execution backend.
#[derive(Debug, Clone, PartialEq)]
pub enum TerminalBackend {
    /// Execute on this machine (default).
    Local,
    /// Execute inside a Docker container via `docker exec`.
    Docker { container: String },
    /// Execute on a remote host via the `ssh` client.
    Ssh {
        host: String,
        user: Option<String>,
        port: Option<u16>,
        identity: Option<String>,
    },
}

/// Resolve the backend from config, validating required fields.
pub fn resolve(config: &TerminalConfig) -> Result<TerminalBackend> {
    match config.backend.as_deref().unwrap_or("local") {
        "local" | "" => Ok(TerminalBackend::Local),
        "docker" => {
            let container = match config.container.clone() {
                Some(container) if !container.trim().is_empty() => container.trim().to_string(),
                _ => {
                    return Err(AgentError::config(
                        "[terminal] backend=\"docker\" requires container = \"...\" (or image = \"...\" to auto-create)",
                    ))
                }
            };
            ensure_docker_container(&container, config.image.as_deref())
                .map(|container| TerminalBackend::Docker { container })
        }
        "ssh" => {
            let Some(host) = config.ssh_host.clone().filter(|h| !h.trim().is_empty()) else {
                return Err(AgentError::config(
                    "[terminal] backend=\"ssh\" requires ssh_host = \"...\"",
                ));
            };
            Ok(TerminalBackend::Ssh {
                host: host.trim().to_string(),
                user: config.ssh_user.clone(),
                port: config.ssh_port,
                identity: config.ssh_identity.clone(),
            })
        }
        other => Err(AgentError::config(format!(
            "[terminal] backend must be local, docker, or ssh (got: {})",
            other
        ))),
    }
}

/// Check `docker inspect`; when the container is missing and an image is
/// configured, create a long-lived container from it.
fn ensure_docker_container(container: &str, image: Option<&str>) -> Result<String> {
    let probe = std::process::Command::new("docker")
        .args(["inspect", "--format", "{{.Id}}", container])
        .output();
    match probe {
        Ok(output) if output.status.success() => Ok(container.to_string()),
        Ok(_) => {
            let Some(image) = image.filter(|i| !i.trim().is_empty()) else {
                return Err(AgentError::config(format!(
                    "docker container '{}' not found (set [terminal] image to auto-create it)",
                    container
                )));
            };
            let created = std::process::Command::new("docker")
                .args([
                    "run", "--detach", "--name", container, image, "sleep", "infinity",
                ])
                .output()
                .map_err(|e| AgentError::config(format!("docker run failed: {}", e)))?;
            if !created.status.success() {
                let stderr = String::from_utf8_lossy(&created.stderr);
                return Err(AgentError::config(format!(
                    "docker run {} failed: {}",
                    image,
                    stderr.trim()
                )));
            }
            Ok(container.to_string())
        }
        Err(e) => Err(AgentError::config(format!(
            "docker not available: {} (install docker or use backend=\"local\")",
            e
        ))),
    }
}

/// Shell-quote a string for embedding inside `bash -c '...'`.
pub fn shell_quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('\'');
    for ch in text.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Rewrite `command` so it executes inside the backend environment.
/// `cwd` is applied remotely (docker `-w`, ssh `cd` prefix).
pub fn wrap_command(backend: &TerminalBackend, command: &str, cwd: Option<&str>) -> String {
    match backend {
        TerminalBackend::Local => command.to_string(),
        TerminalBackend::Docker { container } => {
            let mut parts = vec!["docker".to_string(), "exec".to_string()];
            if let Some(cwd) = cwd.filter(|c| !c.is_empty()) {
                parts.push("--workdir".to_string());
                parts.push(cwd.to_string());
            }
            parts.push(container.clone());
            parts.push("bash".to_string());
            parts.push("-c".to_string());
            parts.push(shell_quote(command));
            parts.join(" ")
        }
        TerminalBackend::Ssh {
            host,
            user,
            port,
            identity,
        } => {
            let mut parts = vec!["ssh".to_string()];
            parts.push("-o".to_string());
            parts.push("BatchMode=yes".to_string());
            if let Some(port) = port {
                parts.push("-p".to_string());
                parts.push(port.to_string());
            }
            if let Some(identity) = identity.as_ref().filter(|i| !i.is_empty()) {
                let identity = shellexpand_tilde(identity);
                parts.push("-i".to_string());
                parts.push(shell_quote(&identity));
            }
            let target = match user {
                Some(user) if !user.trim().is_empty() => format!("{}@{}", user.trim(), host),
                _ => host.clone(),
            };
            let remote = match cwd.filter(|c| !c.is_empty()) {
                Some(cwd) => format!("cd {} && {}", shell_quote(cwd), command),
                None => command.to_string(),
            };
            parts.push(target);
            parts.push(shell_quote(&remote));
            parts.join(" ")
        }
    }
}

fn shellexpand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = crate::config::get_env_value("HOME") {
            return format!("{}/{}", home.trim_end_matches('/'), rest);
        }
    }
    path.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shell_quote() {
        assert_eq!(shell_quote("simple"), "'simple'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn test_wrap_local_is_identity() {
        let backend = TerminalBackend::Local;
        assert_eq!(wrap_command(&backend, "ls -la", Some("/tmp")), "ls -la");
    }

    #[test]
    fn test_wrap_docker() {
        let backend = TerminalBackend::Docker {
            container: "devbox".to_string(),
        };
        let wrapped = wrap_command(&backend, "cargo test", Some("/src"));
        assert_eq!(
            wrapped,
            "docker exec --workdir /src devbox bash -c 'cargo test'"
        );
        let no_cwd = wrap_command(&backend, "echo hi", None);
        assert_eq!(no_cwd, "docker exec devbox bash -c 'echo hi'");
    }

    #[test]
    fn test_wrap_ssh() {
        let backend = TerminalBackend::Ssh {
            host: "build.lan".to_string(),
            user: Some("ci".to_string()),
            port: Some(2222),
            identity: None,
        };
        let wrapped = wrap_command(&backend, "make all", Some("/opt/proj"));
        assert_eq!(
            wrapped,
            "ssh -o BatchMode=yes -p 2222 ci@build.lan 'cd '\\''/opt/proj'\\'' && make all'"
        );
    }

    #[test]
    fn test_resolve_validation() {
        let mut config = TerminalConfig::default();
        config.backend = Some("docker".to_string());
        assert!(resolve(&config).is_err());
        config.backend = Some("bogus".to_string());
        assert!(resolve(&config).is_err());
        config.backend = Some("local".to_string());
        assert_eq!(resolve(&config).unwrap(), TerminalBackend::Local);
    }
}

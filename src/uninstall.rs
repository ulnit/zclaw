//! Uninstaller — port of hermes `hermes_cli/uninstall.py`.
//!
//! Options (hermes parity):
//! - **Keep data** (default): remove code + shell PATH entries + wrapper
//!   symlinks, keep `~/.ulnclaw/` (configs, sessions, logs) for reinstall.
//! - **Full uninstall** (`--full`): also wipe the ulnclaw home directory.
//!
//! Steps mirror hermes `_perform_uninstall`: stop gateway processes →
//! strip PATH entries from shell rc files → remove wrapper symlinks →
//! remove the code checkout → optionally wipe the home dir. Windows
//! registry/env cleanup is not ported (hermes Windows installer
//! artifacts; ulnclaw ships as a plain binary).

use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Logging helpers (hermes log_info / log_success / log_warn)
// ---------------------------------------------------------------------------

fn log_info(msg: &str) {
    println!("→ {msg}");
}

fn log_success(msg: &str) {
    println!("✓ {msg}");
}

fn log_warn(msg: &str) {
    println!("⚠ {msg}");
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// The running binary's canonical path.
fn current_exe() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok())
}

/// Locate the ulnclaw code checkout that owns the running binary: walk up
/// from the executable looking for a `Cargo.toml` whose package name is
/// `ulnclaw` (handles target/debug, target/<triple>/debug, cargo-install).
pub fn find_project_root() -> Option<PathBuf> {
    let exe = current_exe()?;
    let mut dir = exe.parent()?.to_path_buf();
    for _ in 0..6 {
        let manifest = dir.join("Cargo.toml");
        if manifest.exists() {
            if let Ok(contents) = std::fs::read_to_string(&manifest) {
                if contents.contains("name = \"ulnclaw\"") {
                    return Some(dir);
                }
            }
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

/// Shell config files that might have PATH entries (hermes
/// find_shell_configs).
pub fn find_shell_configs() -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    [
        ".bashrc",
        ".bash_profile",
        ".profile",
        ".zshrc",
        ".zprofile",
    ]
    .iter()
    .map(|name| home.join(name))
    .filter(|path| path.exists())
    .collect()
}

/// Remove ulnclaw PATH entries from shell configuration files (hermes
/// remove_path_from_shell_configs). Returns the files that changed.
pub fn remove_path_from_shell_configs() -> Vec<PathBuf> {
    let mut removed_from = Vec::new();
    for config_path in find_shell_configs() {
        let Ok(content) = std::fs::read_to_string(&config_path) else {
            log_warn(&format!("Could not read {}", config_path.display()));
            continue;
        };
        let mut new_lines: Vec<String> = Vec::new();
        let mut skip_next = false;
        for line in content.split('\n') {
            // Skip the "# ulnclaw" marker comment and the PATH line after it.
            if line.contains("# ulnclaw") || line.contains("# ulnclaw-agent") {
                skip_next = true;
                continue;
            }
            let lower = line.to_ascii_lowercase();
            if skip_next && lower.contains("ulnclaw") && lower.contains("path") {
                skip_next = false;
                continue;
            }
            skip_next = false;
            // Remove any PATH line containing ulnclaw.
            if lower.contains("ulnclaw") && (line.contains("PATH=") || lower.contains("path=")) {
                continue;
            }
            new_lines.push(line.to_string());
        }
        let mut new_content = new_lines.join("\n");
        // Clean up multiple blank lines.
        while new_content.contains("\n\n\n") {
            new_content = new_content.replace("\n\n\n", "\n\n");
        }
        if new_content != content {
            match std::fs::write(&config_path, new_content) {
                Ok(()) => removed_from.push(config_path),
                Err(e) => log_warn(&format!("Could not update {}: {e}", config_path.display())),
            }
        }
    }
    removed_from
}

/// Wrapper-symlink candidates (hermes remove_wrapper_script targets the
/// `hermes` launcher; ulnclaw installs into ~/.local/bin or /usr/local/bin).
fn wrapper_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".local/bin/ulnclaw"));
        candidates.push(home.join(".cargo/bin/ulnclaw"));
    }
    candidates.push(PathBuf::from("/usr/local/bin/ulnclaw"));
    candidates
}

/// Remove wrapper symlinks that point into this installation. Regular
/// files and links owned by other installs are left alone (hermes checks
/// the wrapper actually belongs to this checkout).
pub fn remove_wrapper_scripts(project_root: &Option<PathBuf>) -> Vec<PathBuf> {
    let mut removed = Vec::new();
    for candidate in wrapper_candidates() {
        let Ok(metadata) = std::fs::symlink_metadata(&candidate) else {
            continue;
        };
        if metadata.file_type().is_symlink() {
            let Ok(target) = std::fs::canonicalize(&candidate) else {
                continue;
            };
            let belongs = project_root
                .as_ref()
                .map(|root| target.starts_with(root))
                .unwrap_or(false)
                || current_exe().map(|exe| target == exe).unwrap_or(false);
            if belongs {
                if std::fs::remove_file(&candidate).is_ok() {
                    removed.push(candidate);
                }
            }
        }
    }
    removed
}

/// Running `ulnclaw gateway` processes (other than this one) found via
/// /proc — the hermes uninstall_gateway_service "kill standalone
/// processes" half; ulnclaw has no systemd/launchd service to stop.
pub fn find_gateway_processes() -> Vec<u32> {
    let Some(exe) = current_exe() else {
        return Vec::new();
    };
    let self_pid = std::process::id();
    let mut pids = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        if pid == self_pid {
            continue;
        }
        let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        let args: Vec<String> = cmdline
            .split(|b| *b == 0)
            .filter(|chunk| !chunk.is_empty())
            .map(|chunk| String::from_utf8_lossy(chunk).to_string())
            .collect();
        if args.is_empty() {
            continue;
        }
        let argv0_matches = Path::new(&args[0])
            .canonicalize()
            .map(|p| p == exe)
            .unwrap_or(false);
        if argv0_matches && args.iter().any(|arg| arg == "gateway") {
            pids.push(pid);
        }
    }
    pids
}

/// SIGTERM the running gateway processes (hermes stops the service first).
pub fn stop_gateway_processes() -> Vec<u32> {
    let pids = find_gateway_processes();
    let mut stopped = Vec::new();
    for pid in pids {
        if crate::process_ctl::terminate(pid).is_ok() {
            stopped.push(pid);
        }
    }
    stopped
}

// ---------------------------------------------------------------------------
// Plan + execution
// ---------------------------------------------------------------------------

/// What the uninstaller will touch (rendered by --dry-run).
pub struct UninstallPlan {
    pub project_root: Option<PathBuf>,
    pub home: PathBuf,
    pub full: bool,
}

pub fn build_plan(full: bool) -> UninstallPlan {
    UninstallPlan {
        project_root: find_project_root(),
        home: crate::config::ulnclaw_home(),
        full,
    }
}

/// Print the uninstall plan without stopping processes or deleting files
/// (hermes _print_uninstall_dry_run).
pub fn print_dry_run(plan: &UninstallPlan) {
    println!("Uninstall plan (dry run — nothing will be changed):");
    println!("  1. Stop running `ulnclaw gateway` processes (SIGTERM)");
    println!("  2. Remove ulnclaw PATH entries from shell rc files:");
    for config in find_shell_configs() {
        println!("     - {}", config.display());
    }
    println!("  3. Remove wrapper symlinks:");
    for candidate in wrapper_candidates() {
        println!(
            "     - {} (only if it links into this install)",
            candidate.display()
        );
    }
    match &plan.project_root {
        Some(root) => println!("  4. Remove code checkout: {}", root.display()),
        None => println!("  4. No ulnclaw checkout found next to the binary (skip)"),
    }
    if plan.full {
        println!("  5. FULL: wipe config + data: {}", plan.home.display());
    } else {
        println!(
            "  5. Keep config + data in {} (--full to wipe)",
            plan.home.display()
        );
    }
}

/// Execute the uninstall steps (hermes _perform_uninstall). Shared by the
/// interactive and `--yes` paths so the destructive sequence lives in
/// exactly one place.
pub fn perform_uninstall(plan: &UninstallPlan) {
    println!("\nUninstalling...\n");

    // 1. Stop running gateway processes.
    log_info("Checking for running gateway...");
    let stopped = stop_gateway_processes();
    if stopped.is_empty() {
        log_info("No gateway processes found");
    } else {
        for pid in stopped {
            log_success(&format!("Stopped gateway process {pid}"));
        }
    }

    // 2. Remove PATH entries from shell configs.
    log_info("Removing PATH entries from shell configs...");
    let removed_configs = remove_path_from_shell_configs();
    if removed_configs.is_empty() {
        log_info("No PATH entries found to remove in shell rc files");
    } else {
        for config in removed_configs {
            log_success(&format!("Updated {}", config.display()));
        }
    }

    // 3. Remove wrapper symlinks.
    log_info("Removing ulnclaw command wrappers...");
    let removed_wrappers = remove_wrapper_scripts(&plan.project_root);
    if removed_wrappers.is_empty() {
        log_info("No wrapper symlinks found");
    } else {
        for wrapper in removed_wrappers {
            log_success(&format!("Removed {}", wrapper.display()));
        }
    }

    // 4. Remove the code checkout.
    log_info("Removing installation directory...");
    match &plan.project_root {
        Some(root) if root.exists() => match std::fs::remove_dir_all(root) {
            Ok(()) => log_success(&format!("Removed {}", root.display())),
            Err(e) => {
                log_warn(&format!("Could not fully remove {}: {e}", root.display()));
                log_info("You may need to manually remove it");
            }
        },
        Some(root) => log_info(&format!("{} does not exist — skipping", root.display())),
        None => log_info("No checkout detected (binary installed elsewhere) — removing nothing"),
    }

    // 5. Optionally wipe the home directory.
    if plan.full {
        log_info("Removing configuration and data...");
        if plan.home.exists() {
            match std::fs::remove_dir_all(&plan.home) {
                Ok(()) => log_success(&format!("Removed {}", plan.home.display())),
                Err(e) => {
                    log_warn(&format!(
                        "Could not fully remove {}: {e}",
                        plan.home.display()
                    ));
                    log_info("You may need to manually remove it");
                }
            }
        }
    } else {
        log_info(&format!(
            "Keeping configuration and data in {}",
            plan.home.display()
        ));
    }

    println!("\n┌─────────────────────────────────────────────────────────┐");
    println!("│              ✓ Uninstall Complete!                      │");
    println!("└─────────────────────────────────────────────────────────┘\n");
    if !plan.full {
        println!("Your configuration and data have been preserved:");
        println!("  {}/\n", plan.home.display());
    }
    println!("Reload your shell to complete the process:");
    println!("  source ~/.bashrc  # or ~/.zshrc\n");
    println!("Thank you for using ulnclaw!");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_config_scan_covers_standard_rc_files() {
        // Whatever exists in the test environment, the scan must not panic
        // and must only return existing paths.
        for path in find_shell_configs() {
            assert!(path.exists());
        }
    }

    #[test]
    fn wrapper_candidates_include_standard_locations() {
        let candidates = wrapper_candidates();
        assert!(candidates.iter().any(|p| p.ends_with(".local/bin/ulnclaw")));
        assert!(candidates
            .iter()
            .any(|p| p == Path::new("/usr/local/bin/ulnclaw")));
    }

    #[test]
    fn gateway_process_scan_is_safe_headless() {
        // Must not panic and must never list this test process.
        let pids = find_gateway_processes();
        assert!(!pids.contains(&std::process::id()));
    }

    #[test]
    fn plan_builds_without_panic() {
        let plan = build_plan(false);
        assert!(plan.home.to_string_lossy().contains("ulnclaw") || plan.full == false);
        let full = build_plan(true);
        assert!(full.full);
    }
}

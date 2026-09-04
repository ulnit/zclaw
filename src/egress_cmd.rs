//! `ulnclaw egress` handlers — port of hermes `hermes_cli/proxy_cli.py`
//! @ v2026.8.3 (top-level command `hermes egress`; core module
//! `src/iron_proxy.rs`). The inbound OAuth reverse-proxy command
//! (`ulnclaw proxy`) lives in `src/proxy_cmd.rs` — different direction,
//! different purpose.

use crate::iron_proxy as ip;

fn load_config_toml() -> Result<toml::Value, String> {
    crate::config_cmd::load_toml(&crate::config_cmd::config_path())
}

fn save_config_toml(value: &toml::Value) -> Result<(), String> {
    crate::config_cmd::save_toml(&crate::config_cmd::config_path(), value)
}

fn proxy_table(value: &toml::Value) -> toml::map::Map<String, toml::Value> {
    value
        .get("proxy")
        .and_then(|v| v.as_table())
        .cloned()
        .unwrap_or_default()
}

fn proxy_table_mut(value: &mut toml::Value) -> &mut toml::map::Map<String, toml::Value> {
    value
        .as_table_mut()
        .expect("config root is a table")
        .entry("proxy")
        .or_insert_with(|| toml::Value::Table(Default::default()))
        .as_table_mut()
        .expect("proxy is a table")
}

fn yn(flag: bool) -> &'static str {
    if flag {
        "yes"
    } else {
        "no"
    }
}

/// hermes `_redact_token`.
fn redact_token(token: &str) -> String {
    if token.len() < 16 {
        return token.to_string();
    }
    format!("{}…{}", &token[..12], &token[token.len() - 4..])
}

/// `ulnclaw egress install` (hermes `cmd_install`).
pub fn handle_install(force: bool) -> Result<(), String> {
    let binary = ip::install_iron_proxy(force).map_err(|e| {
        format!("{e}\n  Manual install: https://github.com/ironsh/iron-proxy/releases")
    })?;
    let version = ip::iron_proxy_version(&binary);
    let version = if version.is_empty() {
        "(version unknown)".to_string()
    } else {
        version
    };
    println!("✓ installed {}  {version}", binary.display());
    Ok(())
}

/// Setup wizard options (hermes `cmd_setup` args).
pub struct SetupOptions {
    pub tunnel_port: Option<u16>,
    pub from_bitwarden: bool,
    pub no_bitwarden: bool,
    pub rotate_tokens: bool,
    /// None = ask on a tty (restart only when a daemon was running).
    pub restart: Option<bool>,
}

/// `ulnclaw egress setup` — install + CA + mint tokens + write config
/// (hermes `cmd_setup`).
pub fn handle_setup(opts: SetupOptions) -> Result<(), String> {
    println!("┌─────────────────────────────────────────────────────────┐");
    println!("│ iron-proxy setup                                        │");
    println!("│                                                         │");
    println!("│ Routes outbound sandbox traffic through a local         │");
    println!("│ TLS-intercepting proxy so prompt-injected agents never  │");
    println!("│ see real provider API keys.                             │");
    println!("│ Project: https://github.com/ironsh/iron-proxy (Apache-2.0) │");
    println!("└─────────────────────────────────────────────────────────┘");

    // ------------------------------------------------------------- binary
    println!();
    println!("Step 1  Install the iron-proxy binary");
    let binary = match ip::find_iron_proxy(false) {
        Some(path) => path,
        None => {
            println!("  No iron-proxy on PATH — downloading…");
            ip::install_iron_proxy(false)?
        }
    };
    let version = ip::iron_proxy_version(&binary);
    let version = if version.is_empty() {
        "(version unknown)".to_string()
    } else {
        version
    };
    println!("  ✓ {}  {version}", binary.display());

    // ------------------------------------------------------------- CA
    println!();
    println!("Step 2  Generate a CA cert");
    let (ca_crt, ca_key) = ip::ensure_ca_cert(false)?;
    println!("  ✓ {}", ca_crt.display());

    // ------------------------------------------------------------- mint
    println!();
    println!("Step 3  Mint proxy tokens for known providers");

    let config = crate::config::UlncLawConfig::load(None).unwrap_or_default();
    let mut available_env_names: Vec<String> = Vec::new();
    if opts.from_bitwarden {
        let bw_cfg = &config.secrets.bitwarden;
        if !bw_cfg.enabled {
            return Err(
                "--from-bitwarden requested but secrets.bitwarden.enabled is false.\n  \
                 Run `ulnclaw secrets bitwarden setup` first, or omit --from-bitwarden."
                    .into(),
            );
        }
        let access_token = std::env::var(&bw_cfg.access_token_env).unwrap_or_default();
        if access_token.trim().is_empty() {
            return Err(format!(
                "--from-bitwarden requested but {} is not set in the environment.",
                bw_cfg.access_token_env
            ));
        }
        let result = crate::secrets::fetch_bitwarden_source(bw_cfg, &crate::config::ulnclaw_home());
        if !result.ok {
            return Err(format!(
                "Could not enumerate Bitwarden secrets: {}\n  Either fix the Bitwarden \
                 config and retry, or rerun setup without --from-bitwarden (the proxy \
                 will read secrets from the host process env at start time).",
                result.error.unwrap_or_else(|| "unknown error".into())
            ));
        }
        available_env_names = result.secrets.keys().cloned().collect();
        if available_env_names.is_empty() {
            return Err(
                "Bitwarden returned an empty secrets list.\n  Check the project_id in \
                 secrets.bitwarden and the BWS access-token's project scope."
                    .into(),
            );
        }
        println!(
            "  Pulled {} env names from Bitwarden.",
            available_env_names.len()
        );
    } else {
        // Operators commonly keep provider keys only in <home>/.env
        // (loaded at agent runtime, not exported into an interactive
        // shell) — backfill so discovery finds the same keys the agent
        // would (hermes `_load_env_file_into_environ`).
        let env_file = crate::config_cmd::env_path();
        let file_env = crate::config::load_env_file(&env_file);
        let mut added = 0usize;
        let mut known: Vec<&str> = ip::BEARER_PROVIDERS.iter().map(|(n, _)| *n).collect();
        known.extend(ip::NON_BEARER_PROVIDERS.iter().copied());
        known.extend(ip::HEADER_AUTH_PROVIDERS.iter().map(|s| s.env_name));
        for name in known {
            if std::env::var(name)
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false)
            {
                continue;
            }
            if let Some(value) = file_env.get(name).filter(|v| !v.trim().is_empty()) {
                std::env::set_var(name, value);
                added += 1;
            }
        }
        if added > 0 {
            println!(
                "  Loaded {added} provider key name(s) from {} for discovery.",
                env_file.display()
            );
        }
    }

    let discovered = ip::discover_provider_mappings(if available_env_names.is_empty() {
        None
    } else {
        Some(&available_env_names)
    });

    // Preserve tokens for providers we already had unless the operator
    // explicitly requested rotation (avoids 401-ing running sandboxes).
    let existing = ip::load_mappings();
    let rotate = opts.rotate_tokens;
    if rotate && !existing.is_empty() {
        if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
            println!(
                "⚠  --rotate-tokens will invalidate proxy tokens in every running \
                 sandbox.  They will start 401-ing against upstreams until restarted."
            );
            print!("Type 'rotate' to confirm: ");
            use std::io::Write;
            let _ = std::io::stdout().flush();
            let mut answer = String::new();
            let _ = std::io::stdin().read_line(&mut answer);
            if answer.trim().to_lowercase() != "rotate" {
                return Err("Cancelled.".into());
            }
        }
        // Backup the existing mappings before overwriting.
        let state_dir = crate::config::ulnclaw_home().join("proxy");
        let mappings_src = state_dir.join("mappings.json");
        if mappings_src.exists() {
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let backup = state_dir.join(format!("mappings.json.rotated-{ts}"));
            if let Err(e) = std::fs::copy(&mappings_src, &backup) {
                println!("  ⚠ Could not back up mappings before rotation: {e}");
            } else {
                println!("  backup: {}", backup.display());
            }
        }
    } else if rotate && existing.is_empty() {
        println!(
            "Note: --rotate-tokens is a no-op on first-time setup (no existing tokens \
             to rotate)."
        );
    }

    let mappings = ip::merge_mappings(&existing, discovered, rotate);
    if mappings.is_empty() {
        let mut names: Vec<&str> = ip::BEARER_PROVIDERS.iter().map(|(n, _)| *n).collect();
        names.sort();
        let list = names
            .iter()
            .map(|n| format!("    - {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        return Err(format!(
            "No known provider API keys found in env/Bitwarden.\n  Set at least one of \
             these and rerun setup:\n{list}"
        ));
    }

    // Providers we recognize but can't proxy (SigV4 / SDK-minted OAuth).
    let uncovered = ip::discover_uncovered_providers(if available_env_names.is_empty() {
        None
    } else {
        Some(&available_env_names)
    });
    if !uncovered.is_empty() {
        println!();
        println!("  ⚠  Detected provider env vars that the proxy does not yet cover:");
        for name in &uncovered {
            println!("    - {name}");
        }
        println!(
            "  These providers use request signing or SDK-minted OAuth (SigV4, \
             service-account files) and will hold real credentials inside the sandbox.  \
             Egress isolation is INCOMPLETE for these."
        );
    }

    println!();
    println!(
        "  {:<24} {:<44} {}",
        "Provider env", "Upstream hosts", "Proxy token"
    );
    for m in &mappings {
        println!(
            "  {:<24} {:<44} {}",
            m.real_env_name,
            m.upstream_hosts.join(", "),
            redact_token(&m.proxy_token)
        );
    }

    // ------------------------------------------------------------- write
    println!();
    println!("Step 4  Write config and persist mappings");

    let mut cfg = load_config_toml()?;
    let proxy_cfg = proxy_table(&cfg);
    let tunnel_port = match opts.tunnel_port {
        Some(port) => {
            if port < 1 || port > 65534 {
                return Err(
                    "--tunnel-port must be between 1 and 65534 (the plain-HTTP listener \
                     uses port+1)."
                        .into(),
                );
            }
            port
        }
        None => proxy_cfg
            .get("tunnel_port")
            .and_then(|v| v.as_integer())
            .map(|v| v as u16)
            .unwrap_or(ip::DEFAULT_TUNNEL_PORT),
    };

    let extra_hosts: Vec<String> = proxy_cfg
        .get("extra_allowed_hosts")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let mut allowed: Vec<String> = ip::DEFAULT_ALLOWED_HOSTS
        .iter()
        .map(|s| s.to_string())
        .collect();
    for host in extra_hosts {
        if !allowed.contains(&host) {
            allowed.push(host);
        }
    }

    let audit_log_path = crate::config::ulnclaw_home()
        .join("proxy")
        .join("audit.log");
    // Pre-create the audit log 0600. On v0.39 the daemon does NOT write
    // it (reserved for v0.40+), so failure is a WARNING, not an abort.
    let mut audit_log_ok = true;
    if let Err(e) = ip::ensure_audit_log(&audit_log_path) {
        audit_log_ok = false;
        println!("  ⚠ {e}");
    }

    let deny_cidrs: Option<Vec<String>> = proxy_cfg
        .get("upstream_deny_cidrs")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        });
    let iron_cfg = ip::build_proxy_config(
        &mappings,
        &ca_crt,
        &ca_key,
        tunnel_port,
        Some(&audit_log_path),
        Some(allowed),
        deny_cidrs,
        None,
    );
    let cfg_path = ip::write_proxy_config(&iron_cfg)?;
    let mappings_path = ip::write_mappings(&mappings)?;
    // The generated config enables the loopback management listener; the
    // daemon requires the key env var non-empty at startup.
    ip::ensure_management_token(false)?;
    println!("  ✓ config:   {}", cfg_path.display());
    println!("  ✓ mappings: {}", mappings_path.display());
    if audit_log_ok {
        println!(
            "  ✓ audit log: {} (reserved — not written by iron-proxy v0.39; per-request \
             records land in iron-proxy.log)",
            audit_log_path.display()
        );
    }

    // ------------------------------------------------------------- enable
    {
        let table = proxy_table_mut(&mut cfg);
        table.insert("enabled".into(), toml::Value::Boolean(true));
        table.insert(
            "tunnel_port".into(),
            toml::Value::Integer(tunnel_port as i64),
        );
        table
            .entry("auto_install")
            .or_insert(toml::Value::Boolean(true));
        table
            .entry("enforce_on_docker")
            .or_insert(toml::Value::Boolean(true));
        // CRITICAL: do NOT silently downgrade credential_source on
        // re-run — require an explicit --no-bitwarden to switch back.
        let existing_source = table
            .get("credential_source")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if opts.from_bitwarden {
            table.insert(
                "credential_source".into(),
                toml::Value::String("bitwarden".into()),
            );
        } else if opts.no_bitwarden {
            table.insert(
                "credential_source".into(),
                toml::Value::String("env".into()),
            );
            if existing_source == "bitwarden" {
                println!("Switched credential_source from bitwarden to env.");
            }
        } else if existing_source == "bitwarden" {
            println!(
                "Keeping credential_source=bitwarden from existing config.  Pass \
                 --no-bitwarden to switch to env-based credentials."
            );
        } else {
            table.insert(
                "credential_source".into(),
                toml::Value::String("env".into()),
            );
        }
    }
    save_config_toml(&cfg)?;

    let live_status = ip::get_status();
    let was_running = live_status.pid.is_some();
    if was_running {
        ip::stop_proxy();
    }

    // Restart decision (hermes):
    //   --restart      → always (re)start, even if nothing was running
    //   --no-restart   → never; print the manual hint
    //   neither + tty  → ask (only when a daemon was running)
    //   neither + !tty → restart when a daemon was running
    let do_restart = match opts.restart {
        Some(pref) => pref,
        None if was_running => {
            if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                print!("  Restart the running proxy now with the new config? [Y/n] ");
                use std::io::Write;
                let _ = std::io::stdout().flush();
                let mut answer = String::new();
                let _ = std::io::stdin().read_line(&mut answer);
                matches!(answer.trim().to_lowercase().as_str(), "" | "y" | "yes")
            } else {
                true
            }
        }
        None => false,
    };

    if do_restart {
        let auto_install = cfg
            .get("proxy")
            .and_then(|v| v.get("auto_install"))
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        match ip::start_proxy(&ip::StartOptions {
            binary: None,
            config_path: None,
            extra_env: None,
            install_if_missing: auto_install,
            refresh_secrets_from_bitwarden: false,
            bitwarden_config: None,
            allow_env_fallback: false,
        }) {
            Ok(new_status) => {
                let listening = if new_status.listening {
                    "listening"
                } else {
                    "not yet listening"
                };
                let verb = if was_running { "restarted" } else { "started" };
                println!(
                    "  ✓ {verb} iron-proxy with the new config (pid={:?}, port={}, \
                     {listening})",
                    new_status.pid, new_status.tunnel_port
                );
            }
            Err(e) => {
                println!("  ⚠ could not start iron-proxy with the new config: {e}");
                println!("  Run `ulnclaw egress start` manually before launching new sandboxes.");
            }
        }
    } else if was_running {
        println!(
            "  ⚠ stopped the running iron-proxy; config or tokens changed.  Run \
             `ulnclaw egress restart` (or `start`) before launching new sandboxes."
        );
    }

    println!();
    println!("✓ iron-proxy is configured.  Sandboxes will route outbound traffic through it.");
    println!("  Start:   ulnclaw egress start");
    println!("  Restart: ulnclaw egress restart  (after any re-setup)");
    println!("  Reload:  ulnclaw egress reload   (apply ruleset edits in-place, no restart)");
    println!("  Status:  ulnclaw egress status");
    println!("  Stop:    ulnclaw egress stop");
    println!("  Disable: ulnclaw egress disable");
    Ok(())
}

fn bitwarden_start_config(
) -> Result<(bool, Option<crate::secrets::BitwardenSourceConfig>, bool), String> {
    let cfg = load_config_toml()?;
    let proxy_cfg = proxy_table(&cfg);
    if !proxy_cfg
        .get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return Err("proxy.enabled is false — run `ulnclaw egress setup` first.".into());
    }
    let credential_source = proxy_cfg
        .get("credential_source")
        .and_then(|v| v.as_str())
        .unwrap_or("env")
        .to_string();
    let allow_env_fallback = proxy_cfg
        .get("allow_env_fallback")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let config = crate::config::UlncLawConfig::load(None).unwrap_or_default();
    let bw_enabled = config.secrets.bitwarden.enabled;
    let refresh_bw = credential_source == "bitwarden" && bw_enabled;

    // Silent-degrade guard: the operator explicitly chose bitwarden, but
    // secrets.bitwarden has since been disabled or removed — refuse
    // unless the documented escape hatch is set.
    if credential_source == "bitwarden" && !refresh_bw {
        if allow_env_fallback {
            println!(
                "⚠ credential_source=bitwarden but secrets.bitwarden is disabled or \
                 missing — falling back to host-env secrets (allow_env_fallback=true).  \
                 Rotated Bitwarden keys will NOT propagate."
            );
        } else {
            return Err(
                "Refusing to start: proxy.credential_source is 'bitwarden' but \
                 secrets.bitwarden is disabled or missing.\n  Re-enable it \
                 (secrets.bitwarden.enabled = true), switch back to env credentials \
                 with `ulnclaw egress setup --no-bitwarden`, or set \
                 `proxy.allow_env_fallback: true` to opt into the host-env fallback."
                    .into(),
            );
        }
    }

    // Fail loud BEFORE start_proxy degrades: the operator picked BWS for
    // the rotation guarantee.
    if refresh_bw {
        let bw_cfg = &config.secrets.bitwarden;
        if std::env::var(&bw_cfg.access_token_env)
            .unwrap_or_default()
            .trim()
            .is_empty()
        {
            return Err(format!(
                "Refusing to start: credential_source=bitwarden but {} is not set in \
                 the environment.\n  Either export the access token, or run `ulnclaw \
                 egress setup --no-bitwarden` to switch back to env-based credentials.",
                bw_cfg.access_token_env
            ));
        }
        if bw_cfg.project_id.trim().is_empty() {
            return Err("Refusing to start: credential_source=bitwarden but \
                 secrets.bitwarden.project_id is empty.\n  Run `ulnclaw secrets \
                 bitwarden setup` to configure the project, or switch back via \
                 `ulnclaw egress setup --no-bitwarden`."
                .into());
        }
    }

    Ok((
        refresh_bw,
        refresh_bw.then(|| config.secrets.bitwarden.clone()),
        allow_env_fallback,
    ))
}

/// `ulnclaw egress start` (hermes `cmd_start`).
pub fn handle_start() -> Result<(), String> {
    let cfg = load_config_toml()?;
    let auto_install = proxy_table(&cfg)
        .get("auto_install")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let (refresh_bw, bw_cfg, allow_env_fallback) = bitwarden_start_config()?;

    let status = ip::start_proxy(&ip::StartOptions {
        binary: None,
        config_path: None,
        extra_env: None,
        install_if_missing: auto_install,
        refresh_secrets_from_bitwarden: refresh_bw,
        bitwarden_config: bw_cfg,
        allow_env_fallback,
    })?;
    match status.pid {
        Some(pid) => {
            let listening = if status.listening {
                "listening"
            } else {
                "not yet listening"
            };
            println!(
                "✓ iron-proxy started (pid={pid}, port={}, {listening})",
                status.tunnel_port
            );
            println!(
                "  Configure sandboxes with HTTPS_PROXY=http://host.docker.internal:{}",
                status.tunnel_port
            );
        }
        None => println!("iron-proxy did not report a pid — check `ulnclaw egress status`."),
    }
    Ok(())
}

/// `ulnclaw egress stop` (hermes `cmd_stop`).
pub fn handle_stop() -> Result<(), String> {
    if ip::stop_proxy() {
        println!("✓ iron-proxy stopped");
    } else {
        println!("iron-proxy was not running");
    }
    Ok(())
}

/// `ulnclaw egress restart` (hermes `cmd_restart`): stop (if running)
/// then start — the one-command way to apply config changes.
pub fn handle_restart() -> Result<(), String> {
    if ip::stop_proxy() {
        println!("stopped the running iron-proxy");
    }
    handle_start()
}

/// `ulnclaw egress reload` (hermes `cmd_reload`): hot-apply ruleset
/// changes via the management API — no restart, no dropped connections.
pub fn handle_reload() -> Result<(), String> {
    ip::reload_proxy()?;
    println!("✓ iron-proxy ruleset reloaded in-place (no restart, connections preserved)");
    println!(
        "Note: new upstream secrets (rotated keys, new providers) still need `ulnclaw \
         egress restart` — the daemon reads real credentials from its environment at \
         spawn time."
    );
    Ok(())
}

/// `ulnclaw egress disable` (hermes `cmd_disable`): flip proxy.enabled
/// to false (does not stop a running proxy).
pub fn handle_disable() -> Result<(), String> {
    let mut cfg = load_config_toml()?;
    let already_off = !proxy_table(&cfg)
        .get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if already_off {
        println!("proxy.enabled was already false.");
        return Ok(());
    }
    proxy_table_mut(&mut cfg).insert("enabled".into(), toml::Value::Boolean(false));
    save_config_toml(&cfg)?;
    println!("✓ proxy.enabled set to false");
    if ip::get_status().pid.is_some() {
        println!(
            "  iron-proxy is still running — stop it with `ulnclaw egress stop` if you \
             want it down too."
        );
    }
    Ok(())
}

/// `ulnclaw egress config` (hermes `cmd_config`).
pub fn handle_config() -> Result<(), String> {
    let status = ip::get_status();
    match status.config_path {
        Some(path) => {
            println!("{}", path.display());
            Ok(())
        }
        None => Err("(no config generated — run `ulnclaw egress setup`)".into()),
    }
}

/// `ulnclaw egress status` (hermes `cmd_status`).
pub fn handle_status(show_tokens: bool) -> Result<(), String> {
    println!("{}", format_status_text(show_tokens));
    if show_tokens {
        println!(
            "⚠  proxy tokens just printed in full — they may persist in your shell \
             history.  Consider clearing it after this command."
        );
    }
    Ok(())
}

/// Plain-text egress status for the `/egress` slash command, Dashboard,
/// and Desktop (hermes `format_status_text`).
pub fn format_status_text(show_tokens: bool) -> String {
    let cfg = load_config_toml().unwrap_or_else(|_| toml::Value::Table(Default::default()));
    let proxy_cfg = proxy_table(&cfg);
    let status = ip::get_status();
    let enabled = proxy_cfg
        .get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mut lines: Vec<String> = vec![
        "Egress proxy status".into(),
        String::new(),
        format!("Enabled: {}", yn(enabled)),
        format!(
            "Binary: {}",
            status
                .binary_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(missing)".into())
        ),
        format!(
            "Binary version: {}",
            status
                .binary_version
                .as_deref()
                .unwrap_or("(unknown)")
                .to_string()
        ),
        format!(
            "Config: {}",
            status
                .config_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(not generated)".into())
        ),
        format!(
            "CA cert: {}",
            status
                .ca_cert_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "(not generated)".into())
        ),
        format!("Tunnel port: {}", status.tunnel_port),
        match status.pid {
            Some(pid) => format!("Process: pid {pid}"),
            None => "Process: (stopped)".into(),
        },
        format!("Listening: {}", yn(status.listening)),
        format!(
            "Credential src: {}",
            proxy_cfg
                .get("credential_source")
                .and_then(|v| v.as_str())
                .unwrap_or("env")
        ),
        format!(
            "Docker enforce: {}",
            yn(proxy_cfg
                .get("enforce_on_docker")
                .and_then(|v| v.as_bool())
                .unwrap_or(true))
        ),
        "Scope: Docker backend only in this release".into(),
    ];

    let mappings = ip::load_mappings();
    if !mappings.is_empty() {
        lines.push(String::new());
        lines.push("Token mappings:".into());
        for m in &mappings {
            let token = if show_tokens {
                m.proxy_token.clone()
            } else {
                redact_token(&m.proxy_token)
            };
            lines.push(format!(
                "  - {}: {token} ({})",
                m.real_env_name,
                m.upstream_hosts.join(", ")
            ));
        }
    }

    let uncovered = ip::discover_uncovered_providers(None);
    if !uncovered.is_empty() {
        lines.push(String::new());
        lines.push(
            "Uncovered providers (real credentials still visible inside the sandbox):".into(),
        );
        for name in uncovered {
            lines.push(format!("  - {name}"));
        }
    }

    if enabled && !status.configured() {
        lines.push(String::new());
        lines.push("Next: run `ulnclaw egress setup` to mint tokens and write proxy.yaml.".into());
    } else if enabled && !(status.pid.is_some() && status.listening) {
        lines.push(String::new());
        lines.push("Next: run `ulnclaw egress start` before launching Docker sandboxes.".into());
    }

    lines.join("\n")
}

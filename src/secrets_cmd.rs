//! Secrets backend CLI — port of hermes `hermes_cli/secrets_cli.py`
//! (Bitwarden) and `hermes_cli/onepassword_secrets_cli.py` (1Password):
//! interactive setup wizards, pinned-binary install, per-backend status,
//! mapping management, and disable.

use std::io::{IsTerminal, Write};
use std::path::Path;

const OP_DOCS_URL: &str = "https://developer.1password.com/docs/cli/get-started/";

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn prompt(label: &str) -> Result<String, String> {
    print!("{label}");
    std::io::stdout().flush().map_err(|e| e.to_string())?;
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    Ok(line.trim().to_string())
}

/// Masked secret prompt (hermes `masked_secret_prompt`): terminal echo
/// is disabled while the line is read. Falls back to the plain prompt
/// when stdin is not a terminal or termios is unavailable.
fn prompt_masked(label: &str) -> Result<String, String> {
    if !std::io::stdin().is_terminal() {
        return prompt(label);
    }
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let fd = std::io::stdin().as_raw_fd();
        let mut original: libc::termios = unsafe { std::mem::zeroed() };
        let have_original = unsafe { libc::tcgetattr(fd, &mut original) } == 0;
        if have_original {
            let mut noecho = original;
            noecho.c_lflag &= !libc::ECHO;
            unsafe { libc::tcsetattr(fd, libc::TCSANOW, &noecho) };
        }
        print!("{label}");
        std::io::stdout().flush().ok();
        let mut line = String::new();
        let read = std::io::stdin().read_line(&mut line);
        if have_original {
            unsafe { libc::tcsetattr(fd, libc::TCSANOW, &original) };
        }
        println!();
        read.map_err(|e| e.to_string())?;
        return Ok(line.trim().to_string());
    }
    #[cfg(not(unix))]
    prompt(label)
}

fn binary_version(binary: &Path, flag: &str) -> String {
    std::process::Command::new(binary)
        .arg(flag)
        .stdin(std::process::Stdio::null())
        .output()
        .ok()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown version".to_string())
}

fn set_config(key: &str, value: &str) -> Result<(), String> {
    crate::config_cmd::set_config_value(key, value, true).map(|_| ())
}

// ---------------------------------------------------------------------------
// Bitwarden (hermes secrets_cli.py)
// ---------------------------------------------------------------------------

/// `ulnclaw secrets bitwarden …` actions.
pub enum BitwardenCmd {
    Setup {
        access_token: Option<String>,
        server_url: Option<String>,
        project_id: Option<String>,
    },
    Install {
        force: bool,
    },
    Status,
    /// Rotate the access token: validate a new one and store it in .env
    /// (hermes `secrets bitwarden token`).
    Token {
        access_token: Option<String>,
        no_verify: bool,
    },
    Disable,
}

pub fn bitwarden_cmd(cmd: BitwardenCmd) -> Result<(), String> {
    let home = crate::config::ulnclaw_home();
    match cmd {
        BitwardenCmd::Setup {
            access_token,
            server_url,
            project_id,
        } => bitwarden_setup(&home, access_token, server_url, project_id),
        BitwardenCmd::Install { force } => {
            let path = crate::secrets::install_bws(&home, force)?;
            println!(
                "✓ {}  ({})",
                path.display(),
                binary_version(&path, "--version")
            );
            Ok(())
        }
        BitwardenCmd::Status => bitwarden_status(&home),
        BitwardenCmd::Token {
            access_token,
            no_verify,
        } => bitwarden_token(&home, access_token, no_verify),
        BitwardenCmd::Disable => {
            set_config("secrets.bitwarden.enabled", "false")?;
            println!("✓ Bitwarden integration disabled.");
            Ok(())
        }
    }
}

fn bitwarden_setup(
    home: &Path,
    access_token: Option<String>,
    server_url: Option<String>,
    project_id: Option<String>,
) -> Result<(), String> {
    println!("┌─────────────────────────────────────────────────────────┐");
    println!("│        Bitwarden Secrets Manager setup                  │");
    println!("└─────────────────────────────────────────────────────────┘");
    println!();
    println!("Need an access token? In the Bitwarden web app:");
    println!("  Secrets Manager → Machine accounts → [your account] →");
    println!("  Access tokens → Create access token");
    println!();
    println!("Copy the token (starts with 0.…) — it cannot be retrieved later.");

    // ------------------------------------------------------------- binary
    println!();
    println!("Step 1  Install the bws CLI");
    let binary = match crate::secrets::find_bws(home) {
        Some(path) => path,
        None => {
            println!("  No bws on PATH — downloading…");
            crate::secrets::install_bws(home, false).map_err(|e| {
                format!(
                    "Could not install bws: {e}\n  Manual install: https://github.com/bitwarden/sdk-sm/releases"
                )
            })?
        }
    };
    println!(
        "  ✓ {}  ({})",
        binary.display(),
        binary_version(&binary, "--version")
    );

    // ------------------------------------------------- non-interactive guard
    let interactive = std::io::stdin().is_terminal();
    if !interactive {
        let mut missing = Vec::new();
        if access_token
            .as_deref()
            .map(str::trim)
            .map(str::is_empty)
            .unwrap_or(true)
        {
            missing.push("--access-token");
        }
        let env_url = std::env::var("BWS_SERVER_URL").unwrap_or_default();
        if server_url
            .as_deref()
            .map(str::trim)
            .map(str::is_empty)
            .unwrap_or(true)
            && env_url.trim().is_empty()
        {
            missing.push("--server-url");
        }
        if project_id
            .as_deref()
            .map(str::trim)
            .map(str::is_empty)
            .unwrap_or(true)
        {
            missing.push("--project-id");
        }
        if !missing.is_empty() {
            return Err(format!(
                "Non-interactive mode (no TTY) requires all setup flags.\n  Missing: {}\n\n  Usage:\n    ulnclaw secrets bitwarden setup \\\n      --access-token '0.xxx' \\\n      --server-url 'https://vault.bitwarden.com' \\\n      --project-id 'xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx'",
                missing.join(", ")
            ));
        }
    }

    // -------------------------------------------------------------- token
    println!();
    println!("Step 2  Save the access token");
    let token_env = "BWS_ACCESS_TOKEN";
    let token = match access_token.filter(|t| !t.trim().is_empty()) {
        Some(token) => token.trim().to_string(),
        None => {
            let token = prompt(&format!("  Paste access token ({token_env}): "))?;
            if token.is_empty() {
                return Err("Empty token, aborting.".into());
            }
            token
        }
    };
    if !token.starts_with("0.") {
        println!(
            "  Warning: token doesn't start with '0.' — usually that means you pasted\n  something other than a BSM access token. Continuing anyway."
        );
    }
    crate::config_cmd::set_env_value(token_env, &token)
        .map_err(|e| format!("cannot store token: {e}"))?;
    // SAFETY: single-threaded CLI path — let the test fetch below see it.
    unsafe { std::env::set_var(token_env, &token) };
    println!(
        "  ✓ stored in {} as {token_env}",
        crate::config_cmd::env_path().display()
    );

    // ------------------------------------------------------------- region
    println!();
    println!("Step 3  Pick a Bitwarden region");
    let resolved_server_url = match server_url.filter(|s| !s.trim().is_empty()) {
        Some(url) => url.trim().to_string(),
        None => {
            let env_url = std::env::var("BWS_SERVER_URL").unwrap_or_default();
            if !env_url.trim().is_empty() {
                env_url.trim().to_string()
            } else if interactive {
                println!("  1) US Cloud   (https://vault.bitwarden.com — bws default)");
                println!("  2) EU Cloud   (https://vault.bitwarden.eu)");
                println!("  3) Self-hosted (enter URL)");
                loop {
                    let choice = prompt("  Select region [1-3, Enter = US default]: ")?;
                    match choice.as_str() {
                        "" | "1" => break String::new(),
                        "2" => break "https://vault.bitwarden.eu".to_string(),
                        "3" => {
                            let url = prompt("  Server URL: ")?;
                            if url.trim().is_empty() {
                                println!("  Enter a URL, or pick 1/2.");
                                continue;
                            }
                            break url.trim().to_string();
                        }
                        _ => println!("  Enter 1, 2 or 3."),
                    }
                }
            } else {
                String::new()
            }
        }
    };
    if resolved_server_url.is_empty() {
        println!("  ✓ using bws default (US Cloud, https://vault.bitwarden.com)");
    } else {
        println!("  ✓ using {resolved_server_url}");
    }

    // ------------------------------------------------------------ project
    let resolved_project = match project_id.filter(|p| !p.trim().is_empty()) {
        Some(id) => id.trim().to_string(),
        None => {
            println!();
            println!("Step 4  Pick a project");
            let projects = list_bws_projects(&binary, &resolved_server_url)?;
            if projects.is_empty() {
                return Err(
                    "No projects visible to this machine account.\n  In the Bitwarden web app, open the machine account → Projects tab and grant it access to at least one project."
                        .into(),
                );
            }
            println!("  {:>3}  {:<32}  ID", "#", "Name");
            for (idx, (name, id)) in projects.iter().enumerate() {
                println!("  {:>3}  {:<32}  {}", idx + 1, name, id);
            }
            loop {
                let choice = prompt(&format!("  Select project [1-{}]: ", projects.len()))?;
                match choice.parse::<usize>() {
                    Ok(n) if n >= 1 && n <= projects.len() => {
                        break projects[n - 1].1.clone();
                    }
                    _ => println!("  Out of range — pick 1-{}.", projects.len()),
                }
            }
        }
    };

    // --------------------------------------------------------------- test
    println!();
    println!("Step 5  Test fetch");
    let test_cfg = crate::secrets::BitwardenSourceConfig {
        enabled: true,
        access_token_env: token_env.to_string(),
        project_id: resolved_project.clone(),
        server_url: resolved_server_url.clone(),
        override_existing: true,
        cache_ttl_seconds: 0, // bypass cache for the validation fetch
        auto_install: false,
    };
    let result = crate::secrets::fetch_bitwarden_source(&test_cfg, home);
    if !result.ok {
        return Err(format!(
            "Fetch failed: {}",
            result.error.unwrap_or_else(|| "unknown error".into())
        ));
    }
    if result.secrets.is_empty() {
        println!("  Fetch succeeded but the project has no secrets.");
    } else {
        println!("  {:<32}  Status", "Name");
        let mut names: Vec<&String> = result.secrets.keys().collect();
        names.sort();
        for name in names {
            let status = if name == token_env {
                "bootstrap token — never overrides itself"
            } else if std::env::var(name).is_ok() {
                "already set in env (will be overwritten)"
            } else {
                "new"
            };
            println!("  {name:<32}  {status}");
        }
    }
    for warn in &result.warnings {
        println!("  warning: {warn}");
    }

    // --------------------------------------------------------------- save
    set_config("secrets.bitwarden.enabled", "true")?;
    set_config("secrets.bitwarden.project_id", &resolved_project)?;
    if !resolved_server_url.is_empty() {
        set_config("secrets.bitwarden.server_url", &resolved_server_url)?;
    }
    set_config("secrets.bitwarden.access_token_env", token_env)?;
    set_config("secrets.bitwarden.cache_ttl_seconds", "300")?;
    set_config("secrets.bitwarden.override_existing", "true")?;
    set_config("secrets.bitwarden.auto_install", "true")?;

    println!();
    println!("✓ Bitwarden Secrets Manager is enabled. Secrets will be pulled at the");
    println!("  start of every ulnclaw process.");
    println!("  Status:  ulnclaw secrets bitwarden status");
    println!("  Refresh: ulnclaw secrets sync");
    println!("  Disable: ulnclaw secrets bitwarden disable");
    Ok(())
}

/// `bws project list --output json` → (name, id) pairs. The bootstrap
/// token reaches bws through the inherited process environment (the
/// wizard exported it before this runs).
fn list_bws_projects(binary: &Path, server_url: &str) -> Result<Vec<(String, String)>, String> {
    list_bws_projects_with(binary, server_url, None)
}

/// Same as `list_bws_projects`, with an explicit `(env_name, token)`
/// override so token rotation can probe a NEW credential before the
/// working one is replaced (hermes `_list_projects`).
fn list_bws_projects_with(
    binary: &Path,
    server_url: &str,
    token: Option<(&str, &str)>,
) -> Result<Vec<(String, String)>, String> {
    let mut cmd = std::process::Command::new(binary);
    cmd.arg("project")
        .arg("list")
        .arg("--output")
        .arg("json")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    if let Some((env_name, value)) = token {
        cmd.env(env_name, value);
    }
    if !server_url.is_empty() {
        cmd.env("BWS_SERVER_URL", server_url);
    }
    let output = cmd.output().map_err(|e| format!("bws spawn failed: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let first = stderr.lines().next().unwrap_or("").trim();
        return Err(format!(
            "bws project list exited with {}{}",
            output.status,
            if first.is_empty() {
                String::new()
            } else {
                format!(": {first}")
            }
        ));
    }
    let items: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("unparseable bws project list: {e}"))?;
    Ok(items
        .iter()
        .map(|item| {
            (
                item.get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?")
                    .to_string(),
                item.get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?")
                    .to_string(),
            )
        })
        .collect())
}

/// `ulnclaw secrets bitwarden token` — rotate the BSM access token
/// without re-running the whole setup wizard (hermes `cmd_token`):
/// prompt/accept a new machine-account token, probe Bitwarden with it
/// (unless `--no-verify`), and only then persist it to .env — a bad
/// paste never bricks the working token.
fn bitwarden_token(
    home: &Path,
    access_token: Option<String>,
    no_verify: bool,
) -> Result<(), String> {
    let config = crate::config::UlncLawConfig::load(None).unwrap_or_default();
    let bw_cfg = &config.secrets.bitwarden;
    let token_env = if bw_cfg.access_token_env.trim().is_empty() {
        "BWS_ACCESS_TOKEN".to_string()
    } else {
        bw_cfg.access_token_env.trim().to_string()
    };
    let server_url = bw_cfg.server_url.trim().to_string();

    let mut token = access_token.unwrap_or_default().trim().to_string();
    if token.is_empty() {
        if !std::io::stdin().is_terminal() {
            return Err("No TTY — pass the token with --access-token.".into());
        }
        println!("Create a new token in the Bitwarden web app:");
        println!("  Secrets Manager → Machine accounts → [your account] →");
        println!("  Access tokens → Create access token");
        println!();
        token = prompt_masked(&format!("Paste new access token ({}): ", token_env))?;
    }
    if token.is_empty() {
        return Err("Empty token, aborting.".into());
    }
    if !token.starts_with("0.") {
        println!(
            "Warning: token doesn't start with '0.' — usually that means you pasted \
             something other than a BSM access token."
        );
    }

    if !no_verify {
        let Some(binary) = crate::secrets::find_bws_maybe_install(home, bw_cfg.auto_install) else {
            return Err(
                "bws binary not available — cannot verify. Re-run with --no-verify to \
                 store anyway."
                    .into(),
            );
        };
        println!("Verifying against Bitwarden…");
        let projects = list_bws_projects_with(&binary, &server_url, Some((&token_env, &token)));
        let projects = match projects {
            Ok(projects) => projects,
            Err(_) => {
                return Err("✗ New token was rejected — nothing was changed.".into());
            }
        };
        println!(
            "✓ Token accepted ({} project{} visible).",
            projects.len(),
            if projects.len() == 1 { "" } else { "s" }
        );
        let project_id = bw_cfg.project_id.trim();
        if !project_id.is_empty()
            && !projects.is_empty()
            && !projects.iter().any(|(_, id)| id == project_id)
        {
            println!(
                "Warning: configured project {} is not visible to this machine account. \
                 Grant it access in the Bitwarden web app or re-run \
                 `ulnclaw secrets bitwarden setup` to pick a different project.",
                project_id
            );
        }
    }

    crate::config_cmd::set_env_value(&token_env, &token)?;
    std::env::set_var(&token_env, &token);
    // Old cached pulls are keyed on the previous token's fingerprint;
    // drop them so the next startup fetches fresh (hermes clear_caches).
    crate::secrets_cache::clear_bws_caches(home);
    println!(
        "✓ Stored in {} as {}. Takes effect on the next ulnclaw invocation.",
        crate::config::ulnclaw_home().join(".env").display(),
        token_env
    );
    if !bw_cfg.enabled {
        println!(
            "Note: the Bitwarden integration is currently disabled — run \
             `ulnclaw secrets bitwarden setup` (or set secrets.bitwarden.enabled = true) \
             to turn it on."
        );
    }
    Ok(())
}

fn bitwarden_status(home: &Path) -> Result<(), String> {
    let config = crate::config::UlncLawConfig::load(None).unwrap_or_default();
    let cfg = &config.secrets.bitwarden;
    println!("Bitwarden Secrets Manager:");
    println!("  enabled:          {}", cfg.enabled);
    match crate::secrets::find_bws(home) {
        Some(path) => println!(
            "  bws binary:       {}  ({})",
            path.display(),
            binary_version(&path, "--version")
        ),
        None => println!("  bws binary:       NOT FOUND (secrets bitwarden install)"),
    }
    println!("  auto_install:     {}", cfg.auto_install);
    let token_present = crate::config::get_env_value(&cfg.access_token_env)
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false);
    println!(
        "  token ({}): {}",
        cfg.access_token_env,
        if token_present { "present" } else { "MISSING" }
    );
    println!(
        "  project_id:       {}",
        if cfg.project_id.is_empty() {
            "(unset)"
        } else {
            &cfg.project_id
        }
    );
    println!(
        "  server_url:       {}",
        if cfg.server_url.is_empty() {
            "(bws default — US Cloud)"
        } else {
            &cfg.server_url
        }
    );
    println!("  cache_ttl:        {}s", cfg.cache_ttl_seconds);
    let cache_path = home
        .join("cache")
        .join(crate::secrets_cache::BWS_ENCRYPTED_CACHE_BASENAME);
    println!(
        "  encrypted cache:  {}",
        if cache_path.exists() {
            "present"
        } else {
            "empty"
        }
    );
    if !cfg.enabled || cfg.project_id.is_empty() {
        println!();
        println!("Run `ulnclaw secrets bitwarden setup` to configure.");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 1Password (hermes onepassword_secrets_cli.py)
// ---------------------------------------------------------------------------

/// `ulnclaw secrets onepassword …` actions.
pub enum OnePasswordCmd {
    Setup {
        binary_path: Option<String>,
        account: Option<String>,
        token: Option<String>,
    },
    Status,
    Set {
        name: String,
        reference: String,
    },
    Remove {
        name: String,
    },
    Disable,
}

pub fn onepassword_cmd(cmd: OnePasswordCmd) -> Result<(), String> {
    let home = crate::config::ulnclaw_home();
    match cmd {
        OnePasswordCmd::Setup {
            binary_path,
            account,
            token,
        } => onepassword_setup(&home, binary_path, account, token),
        OnePasswordCmd::Status => onepassword_status(),
        OnePasswordCmd::Set { name, reference } => {
            if !crate::secrets::is_valid_env_name(&name) {
                return Err(format!("{name:?} is not a valid env-var name"));
            }
            if !reference.trim().starts_with("op://") {
                return Err(format!("{reference:?} is not an op:// secret reference"));
            }
            set_config(&format!("secrets.onepassword.env.{name}"), reference.trim())?;
            println!("✓ Mapped {name} → {}", reference.trim());
            println!("  Preview: ulnclaw secrets sync");
            Ok(())
        }
        OnePasswordCmd::Remove { name } => {
            crate::config_cmd::unset_config_value(&format!("secrets.onepassword.env.{name}"))
                .map(|msg| println!("{msg}"))
        }
        OnePasswordCmd::Disable => {
            set_config("secrets.onepassword.enabled", "false")?;
            println!("✓ 1Password integration disabled.");
            Ok(())
        }
    }
}

fn onepassword_setup(
    home: &Path,
    binary_path: Option<String>,
    account: Option<String>,
    token: Option<String>,
) -> Result<(), String> {
    let _ = home;
    println!("┌─────────────────────────────────────────────────────────┐");
    println!("│        1Password secret source setup                    │");
    println!("└─────────────────────────────────────────────────────────┘");
    println!();
    println!("ulnclaw resolves op://vault/item/field references through your");
    println!("already-installed, already-authenticated 1Password CLI (`op`).");
    println!();
    println!("Don't have it yet? Install + sign in: {OP_DOCS_URL}");

    // ------------------------------------------------------------- binary
    println!();
    println!("Step 1  Locate the op CLI");
    let configured_path = binary_path.unwrap_or_default();
    let binary = match crate::secrets::find_op(&configured_path) {
        Some(path) => path,
        None => {
            if configured_path.is_empty() {
                return Err(format!(
                    "op not found on PATH.\n  Install the 1Password CLI: {OP_DOCS_URL}"
                ));
            }
            return Err(format!(
                "{configured_path} is not an executable op binary.\n  Install the 1Password CLI: {OP_DOCS_URL}"
            ));
        }
    };
    println!(
        "  ✓ {}  ({})",
        binary.display(),
        binary_version(&binary, "--version")
    );
    if !configured_path.is_empty() {
        set_config("secrets.onepassword.binary_path", &configured_path)?;
    }
    if let Some(account) = account.filter(|a| !a.trim().is_empty()) {
        set_config("secrets.onepassword.account", account.trim())?;
        println!("  Account: {}", account.trim());
    }

    // -------------------------------------------------------------- token
    println!();
    println!("Step 2  Authentication");
    let token_env = "OP_SERVICE_ACCOUNT_TOKEN";
    match token.filter(|t| !t.trim().is_empty()) {
        Some(token) => {
            crate::config_cmd::set_env_value(token_env, token.trim())
                .map_err(|e| format!("cannot store token: {e}"))?;
            println!(
                "  ✓ service-account token stored in {} as {token_env}",
                crate::config_cmd::env_path().display()
            );
        }
        None => {
            if crate::config::get_env_value(token_env)
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false)
            {
                println!("  ✓ using service-account token from {token_env}");
            } else {
                let who = op_whoami(&binary);
                match who {
                    Some(who) => println!("  ✓ using existing op session ({who})"),
                    None => println!(
                        "  No service-account token and no active op session detected.\n  Either run `op signin` (desktop/interactive) or set a service-account\n  token in {token_env}, then re-run status."
                    ),
                }
            }
        }
    }

    // ------------------------------------------------------------- enable
    set_config("secrets.onepassword.enabled", "true")?;
    set_config("secrets.onepassword.cache_ttl_seconds", "300")?;
    set_config("secrets.onepassword.override_existing", "true")?;

    println!();
    println!("✓ 1Password secret source is enabled.");
    println!("  Map credentials:  ulnclaw secrets onepassword set OPENAI_API_KEY \"op://Private/OpenAI/api key\"");
    println!("  Preview:          ulnclaw secrets sync");
    println!("  Status:           ulnclaw secrets onepassword status");
    Ok(())
}

fn op_whoami(binary: &Path) -> Option<String> {
    let output = std::process::Command::new(binary)
        .arg("whoami")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn onepassword_status() -> Result<(), String> {
    let config = crate::config::UlncLawConfig::load(None).unwrap_or_default();
    let cfg = &config.secrets.onepassword;
    println!("1Password secret source:");
    println!("  enabled:     {}", cfg.enabled);
    match crate::secrets::find_op(&cfg.binary_path) {
        Some(path) => println!(
            "  op binary:   {}  ({})",
            path.display(),
            binary_version(&path, "--version")
        ),
        None => println!("  op binary:   NOT FOUND ({OP_DOCS_URL})"),
    }
    println!(
        "  account:     {}",
        if cfg.account.is_empty() {
            "(default)"
        } else {
            &cfg.account
        }
    );
    let token_present = crate::config::get_env_value(&cfg.service_account_token_env)
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false);
    println!(
        "  token ({}): {}",
        cfg.service_account_token_env,
        if token_present {
            "present"
        } else {
            "absent (interactive op auth may still work)"
        }
    );
    println!("  cache_ttl:   {}s", cfg.cache_ttl_seconds);
    if cfg.env.is_empty() {
        println!("  bindings:    none — map with `ulnclaw secrets onepassword set NAME op://vault/item/field`");
    } else {
        println!("  bindings:    {}", cfg.env.len());
        let mut names: Vec<&String> = cfg.env.keys().collect();
        names.sort();
        for name in names {
            println!("    {name} = {}", &cfg.env[name]);
        }
    }
    if !cfg.enabled {
        println!();
        println!("Run `ulnclaw secrets onepassword setup` to configure.");
    }
    Ok(())
}

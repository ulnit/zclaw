//! `ulnclaw setup` — onboarding wizard (lean port of hermes `setup.py`).
//!
//! Hermes ships a 3.6k-line interactive wizard (model/tts/terminal/gateway/
//! tools/telemetry/agent sections, quick + blank-slate modes, config backup,
//! non-interactive guidance). This port keeps the structure and the flows
//! that map onto ulnclaw surfaces:
//!
//! ```text
//! ulnclaw setup                — full wizard (first-time or reconfigure)
//! ulnclaw setup model|terminal|gateway|tools|agent   — one section
//! ulnclaw setup --quick        — existing installs: fill only missing items
//! ulnclaw setup --reset        — reset config.toml to defaults first
//! ulnclaw setup --non-interactive — print guidance and exit (also auto on no TTY)
//! ```
//!
//! Dropped versus hermes (documented as differences): the Nous Portal
//! quick-setup (Nous-specific OAuth), the OpenClaw migration offer
//! (`hermes claw`), the curses space-toggle checklist (replaced by a
//! comma-separated numeric multi-select), the TTS/telemetry sections
//! (ulnclaw configures TTS via tool env keys and monitoring via
//! `[monitoring]`), and container-resource prompts for cloud sandboxes
//! (ulnclaw terminal backends are local/docker/ssh).

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::config::UlncLawConfig;
use crate::config_cmd;

/// Wizard sections in run order (hermes `SETUP_SECTIONS`, minus tts and
/// telemetry which have no ulnclaw counterpart section).
pub const SECTIONS: &[(&str, &str)] = &[
    ("model", "Model & Provider"),
    ("terminal", "Terminal Backend"),
    ("gateway", "Messaging Platforms (Gateway)"),
    ("tools", "Tools"),
    ("agent", "Agent Settings"),
];

pub fn section_label(key: &str) -> Option<&'static str> {
    SECTIONS.iter().find(|(k, _)| *k == key).map(|(_, l)| *l)
}

// ---------------------------------------------------------------------------
// Platform credential table (gateway section)
// ---------------------------------------------------------------------------

/// One configurable messaging platform: config key, credential env vars to
/// prompt for, and optional follow-up guidance for platforms that have a
/// dedicated wizard or non-token setup.
pub struct PlatformSpec {
    pub name: &'static str,
    pub label: &'static str,
    /// Env vars written to `.env` (empty = no token prompt).
    pub envs: &'static [&'static str],
    /// Post-enable guidance (dedicated wizard / manual config).
    pub guidance: Option<&'static str>,
}

pub fn platform_specs() -> &'static [PlatformSpec] {
    &[
        PlatformSpec { name: "telegram", label: "Telegram", envs: &["TELEGRAM_BOT_TOKEN"], guidance: None },
        PlatformSpec { name: "discord", label: "Discord", envs: &["DISCORD_BOT_TOKEN"], guidance: None },
        PlatformSpec { name: "slack", label: "Slack", envs: &["SLACK_BOT_TOKEN", "SLACK_APP_TOKEN"], guidance: Some("Socket mode needs both tokens; `ulnclaw slack manifest` generates the app manifest.") },
        PlatformSpec { name: "signal", label: "Signal", envs: &[], guidance: Some("Runs against a signal-cli HTTP daemon — set `[messaging.signal]` endpoint in config.toml.") },
        PlatformSpec { name: "weixin", label: "Weixin (WeChat)", envs: &[], guidance: Some("Scan-login with `ulnclaw weixin login` after enabling.") },
        PlatformSpec { name: "qq", label: "QQ Bot", envs: &[], guidance: Some("Run the dedicated wizard: `ulnclaw qq setup`.") },
        PlatformSpec { name: "yuanbao", label: "Yuanbao", envs: &[], guidance: Some("Adapter credentials live in `[messaging.yuanbao]`.") },
        PlatformSpec { name: "email", label: "Email (IMAP/SMTP)", envs: &[], guidance: Some("Configure `[messaging.email]` (IMAP poll + SMTP send) in config.toml.") },
        PlatformSpec { name: "mattermost", label: "Mattermost", envs: &[], guidance: Some("Configure `[messaging.mattermost]` (URL + token) in config.toml.") },
        PlatformSpec { name: "matrix", label: "Matrix", envs: &[], guidance: Some("Configure `[messaging.matrix]` (homeserver + token) in config.toml.") },
        PlatformSpec { name: "dingtalk", label: "DingTalk", envs: &[], guidance: Some("Configure `[messaging.dingtalk]` app credentials in config.toml.") },
        PlatformSpec { name: "wecom", label: "WeCom", envs: &[], guidance: Some("Configure `[messaging.wecom]` bot credentials in config.toml.") },
        PlatformSpec { name: "feishu", label: "Feishu/Lark", envs: &[], guidance: Some("Configure `[messaging.feishu]` app id/secret in config.toml.") },
        PlatformSpec { name: "homeassistant", label: "Home Assistant", envs: &[], guidance: Some("Configure `[messaging.homeassistant]` URL + token in config.toml.") },
        PlatformSpec { name: "sms", label: "SMS (Twilio)", envs: &[], guidance: Some("Configure Twilio credentials under `[messaging.sms]` in config.toml.") },
        PlatformSpec { name: "whatsapp", label: "WhatsApp (Baileys)", envs: &[], guidance: Some("Enables the built-in Baileys bridge; first start runs the QR pairing flow.") },
        PlatformSpec { name: "irc", label: "IRC", envs: &[], guidance: Some("Configure `[messaging.irc]` server/nick in config.toml.") },
        PlatformSpec { name: "ntfy", label: "ntfy", envs: &[], guidance: Some("Configure `[messaging.ntfy]` topic in config.toml.") },
        PlatformSpec { name: "simplex", label: "SimpleX", envs: &[], guidance: Some("Needs a running simplex-chat daemon; see `[messaging.simplex]`.") },
        PlatformSpec { name: "teams", label: "Microsoft Teams", envs: &[], guidance: Some("Configure Teams app credentials under `[messaging.teams]` in config.toml.") },
        PlatformSpec { name: "line", label: "LINE", envs: &[], guidance: Some("Configure `[messaging.line]` channel token/secret in config.toml.") },
        PlatformSpec { name: "google_chat", label: "Google Chat", envs: &[], guidance: Some("Configure service-account credentials under `[messaging.google_chat]`.") },
        PlatformSpec { name: "buzz", label: "Buzz (Nostr)", envs: &[], guidance: Some("Configure relays + key under `[messaging.buzz]` in config.toml.") },
        PlatformSpec { name: "photon", label: "Photon (iMessage)", envs: &[], guidance: Some("Needs the Photon Spectrum sidecar; see `[messaging.photon]`.") },
        PlatformSpec { name: "raft", label: "Raft", envs: &[], guidance: Some("Configure the bridge token under `[messaging.raft]` in config.toml.") },
        PlatformSpec { name: "a2a", label: "A2A (Agent2Agent)", envs: &[], guidance: Some("Serves the Agent2Agent endpoint; see `[messaging.a2a]`.") },
    ]
}

/// `.env` name holding a platform's primary credential, if token-promptable.
pub fn platform_env(name: &str) -> &'static [&'static str] {
    platform_specs()
        .iter()
        .find(|p| p.name == name)
        .map(|p| p.envs)
        .unwrap_or(&[])
}

/// Env var that carries the API key for a provider (hermes key-file
/// semantics; ulnclaw's `resolve_api_key` reads these names).
pub fn api_key_env_var(provider: &str) -> Option<&'static str> {
    match provider {
        "anthropic" => Some("ANTHROPIC_API_KEY"),
        "ollama" | "llamacpp" | "llama_cpp" | "local" => None,
        // openai / openrouter / dashscope / custom all resolve through
        // OPENAI_API_KEY in `resolve_api_key`.
        _ => Some("OPENAI_API_KEY"),
    }
}

/// Default base URL prompt answer for providers that need one beyond the
/// built-in defaults (`config::default_base_url`).
pub fn needs_base_url(provider: &str) -> bool {
    !matches!(
        provider,
        "openai"
            | "anthropic"
            | "ollama"
            | "llamacpp"
            | "llama_cpp"
            | "local"
            | "dashscope"
            | "moa"
    )
}

/// Provider picker entries (id, label).
pub fn provider_choices() -> &'static [(&'static str, &'static str)] {
    &[
        ("openai", "OpenAI"),
        ("anthropic", "Anthropic (Claude)"),
        ("openrouter", "OpenRouter (300+ models)"),
        ("dashscope", "DashScope (Qwen, OpenAI-compatible)"),
        ("ollama", "Ollama (local, keyless)"),
        ("llamacpp", "llama.cpp server (local, keyless)"),
        ("custom", "Custom OpenAI-compatible endpoint"),
    ]
}

/// Sensible model defaults offered per provider when the config has none.
pub fn default_model_for(provider: &str) -> &'static str {
    match provider {
        "anthropic" => "claude-sonnet-4-5",
        "openrouter" => "openrouter/auto",
        "dashscope" => "qwen-max",
        "ollama" => "llama3.1",
        "llamacpp" | "llama_cpp" => "local-model",
        _ => "gpt-4.1",
    }
}

// ---------------------------------------------------------------------------
// Pure helpers (tested)
// ---------------------------------------------------------------------------

/// Back up an existing config file next to itself with a timestamp suffix
/// (hermes #3522). Returns the backup path when a backup was made.
pub fn backup_config(config_path: &Path) -> Option<PathBuf> {
    if !config_path.exists() {
        return None;
    }
    let stamp = chrono_stamp();
    let backup = config_path.with_file_name(format!(
        "{}.bak.{}",
        config_path.file_name()?.to_str()?,
        stamp
    ));
    std::fs::copy(config_path, &backup).ok()?;
    Some(backup)
}

fn chrono_stamp() -> String {
    // Seconds-precision local time without pulling in chrono: the std
    // library gives us the Unix epoch directly.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{}s", secs)
}

/// Existing-install detection (hermes `get_active_provider` + env probe):
/// any resolvable API key or an explicitly chosen model marks an install.
pub fn is_existing_install(config: &UlncLawConfig, config_path: &Path) -> bool {
    if config
        .model
        .api_key
        .as_deref()
        .map(str::trim)
        .map_or(false, |k| !k.is_empty())
    {
        return true;
    }
    for var in ["ULNCLAW_API_KEY", "OPENAI_API_KEY", "ANTHROPIC_API_KEY"] {
        if crate::config::get_env_value(var)
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
        {
            return true;
        }
    }
    // Config file present with a non-default provider/model counts too.
    if config_path.exists() {
        let default = UlncLawConfig::default();
        if config.model.provider != default.model.provider
            || config.model.model != default.model.model
        {
            return true;
        }
        if !config.messaging.enabled_platform_names().is_empty() {
            return true;
        }
    }
    false
}

/// Parse a comma/space-separated "1,3 5" multi-select answer into sorted,
/// de-duplicated zero-based indices. Empty input → empty selection.
pub fn parse_multi_choice(raw: &str, len: usize) -> Result<Vec<usize>, String> {
    let mut out: Vec<usize> = Vec::new();
    for token in raw.split(|c: char| c == ',' || c.is_whitespace()) {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        let n: usize = token
            .parse()
            .map_err(|_| format!("'{token}' is not a number"))?;
        if n < 1 || n > len {
            return Err(format!("'{token}' is out of range (1-{len})"));
        }
        if !out.contains(&(n - 1)) {
            out.push(n - 1);
        }
    }
    out.sort_unstable();
    Ok(out)
}

/// Apply model-section answers to the raw config document.
pub fn apply_model_answers(
    value: &mut toml::Value,
    provider: &str,
    model: &str,
    base_url: Option<&str>,
) -> Result<(), String> {
    config_cmd::set_nested(
        value,
        "model.provider",
        toml::Value::String(provider.to_string()),
    )?;
    config_cmd::set_nested(value, "model.model", toml::Value::String(model.to_string()))?;
    match base_url {
        Some(url) if !url.trim().is_empty() => config_cmd::set_nested(
            value,
            "model.base_url",
            toml::Value::String(url.trim().to_string()),
        )?,
        _ => {
            config_cmd::unset_nested(value, "model.base_url");
        }
    }
    Ok(())
}

/// Platforms with a credential token but no home channel (hermes
/// missing-home check at the end of `setup_gateway`).
pub fn missing_home_channels(has_value: &dyn Fn(&str) -> Option<String>) -> Vec<&'static str> {
    let mut missing = Vec::new();
    let pairs: [(&str, &str); 3] = [
        ("TELEGRAM_BOT_TOKEN", "Telegram"),
        ("DISCORD_BOT_TOKEN", "Discord"),
        ("SLACK_BOT_TOKEN", "Slack"),
    ];
    for (token_var, label) in pairs {
        let has_token = has_value(token_var)
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false);
        let home_var = token_var.replace("_BOT_TOKEN", "_HOME_CHANNEL");
        let has_home = has_value(&home_var)
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false);
        if has_token && !has_home {
            missing.push(label);
        }
    }
    missing
}

/// Render the end-of-wizard summary (hermes `_print_setup_summary`, lean).
pub fn render_summary(
    config_path: &Path,
    env_file: &Path,
    provider: &str,
    model: &str,
    has_key: bool,
    enabled_platforms: &[String],
    toolsets: &[String],
) -> String {
    let mut out = String::new();
    out.push_str("──────────────────────────────────────────────\n");
    out.push_str("  Setup complete!\n\n");
    out.push_str(&format!("  Provider:   {provider}\n"));
    out.push_str(&format!("  Model:      {model}\n"));
    out.push_str(&format!(
        "  API key:    {}\n",
        if has_key {
            "configured"
        } else {
            "MISSING — set one before chatting"
        }
    ));
    out.push_str(&format!(
        "  Platforms:  {}\n",
        if enabled_platforms.is_empty() {
            "none (run `ulnclaw setup gateway` later)".to_string()
        } else {
            enabled_platforms.join(", ")
        }
    ));
    let toolset_display = if toolsets.is_empty() {
        "coding (default)".to_string()
    } else {
        toolsets.join(", ")
    };
    out.push_str(&format!("  Toolsets:   {}\n", toolset_display));
    out.push_str(&format!("\n  Config:  {}\n", config_path.display()));
    out.push_str(&format!("  Secrets: {}\n", env_file.display()));
    out.push_str("\n  Next: run `ulnclaw` to start chatting, or `ulnclaw gateway` to\n");
    out.push_str("  serve the OpenAI-compatible API + messaging platforms.\n");
    out
}

/// Non-interactive guidance (hermes `print_noninteractive_setup_guidance`).
pub fn noninteractive_guidance(reason: Option<&str>) -> String {
    let mut out = String::new();
    out.push_str("ulnclaw setup — non-interactive configuration\n\n");
    if let Some(r) = reason {
        out.push_str(&format!("  {r}\n\n"));
    }
    out.push_str("  Configure without a TTY by setting config keys and env vars directly:\n\n");
    out.push_str("    ulnclaw config set model.provider openai\n");
    out.push_str("    ulnclaw config set model.model gpt-4.1\n");
    out.push_str("    ulnclaw config set OPENAI_API_KEY sk-...      # lands in .env\n");
    out.push_str("    ulnclaw config set messaging.telegram.enabled true\n");
    out.push_str("    ulnclaw config set TELEGRAM_BOT_TOKEN ...     # lands in .env\n\n");
    out.push_str("  Or export credentials in the environment: OPENAI_API_KEY,\n");
    out.push_str("  ANTHROPIC_API_KEY, ULNCLAW_API_KEY, TELEGRAM_BOT_TOKEN, ...\n\n");
    out.push_str(
        "  Sections available interactively: ulnclaw setup model|terminal|gateway|tools|agent\n",
    );
    out
}

// ---------------------------------------------------------------------------
// Interactive driver
// ---------------------------------------------------------------------------

/// Entry point for `ulnclaw setup [section] [--quick] [--reset]
/// [--non-interactive]`.
pub fn run_setup(
    section: Option<&str>,
    quick: bool,
    reset: bool,
    non_interactive_flag: bool,
) -> Result<(), String> {
    let home = crate::config::ulnclaw_home();
    std::fs::create_dir_all(&home).map_err(|e| e.to_string())?;
    UlncLawConfig::write_default_if_missing().map_err(|e| e.to_string())?;

    let config_path = config_cmd::config_path();
    if reset {
        let default = UlncLawConfig::default();
        let content = toml::to_string_pretty(&default).map_err(|e| e.to_string())?;
        std::fs::write(&config_path, content).map_err(|e| e.to_string())?;
        println!("✓ Configuration reset to defaults.");
    }

    // Back up existing config before the wizard modifies it (hermes #3522).
    let backup_path = backup_config(&config_path);

    let non_interactive = non_interactive_flag || !is_interactive_stdin();
    if non_interactive {
        print!(
            "{}",
            noninteractive_guidance(Some(if non_interactive_flag {
                "--non-interactive requested."
            } else {
                "Running in a non-interactive environment (no TTY detected)."
            },))
        );
        return Ok(());
    }

    if let Some(key) = section {
        if section_label(key).is_none() {
            return Err(format!(
                "Unknown setup section: {key}. Available: {}",
                SECTIONS
                    .iter()
                    .map(|(k, _)| *k)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        let mut doc = config_cmd::load_toml(&config_path)?;
        println!();
        print_header(&format!("ulnclaw Setup — {}", section_label(key).unwrap()));
        run_section(key, &mut doc)?;
        config_cmd::save_toml(&config_path, &doc)?;
        announce_backup(backup_path.as_deref(), &config_path);
        println!();
        println!("✓ {} configuration complete!", section_label(key).unwrap());
        return Ok(());
    }

    let config = UlncLawConfig::load(None).map_err(|e| e.to_string())?;
    let is_existing = is_existing_install(&config, &config_path);

    println!();
    println!("┌─────────────────────────────────────────────────────────┐");
    println!("│              ulnclaw Agent Setup Wizard                 │");
    println!("├─────────────────────────────────────────────────────────┤");
    println!("│  Let's configure your ulnclaw installation.             │");
    println!("│  Press Ctrl+C at any time to exit.                      │");
    println!("└─────────────────────────────────────────────────────────┘");

    let mut doc = config_cmd::load_toml(&config_path)?;

    if is_existing && quick {
        run_quick_setup(&mut doc)?;
        config_cmd::save_toml(&config_path, &doc)?;
        announce_backup(backup_path.as_deref(), &config_path);
        print_final_summary(&config_path)?;
        return Ok(());
    }

    if is_existing {
        println!();
        println!("  ─── Reconfigure ───");
        println!("  ✓ You already have ulnclaw configured.");
        println!("  ℹ Running the full wizard — each prompt shows your current value.");
        println!("  ℹ Press Enter to keep it, or type a new value to change it.");
        println!(
            "  ℹ Tip: jump to a section with `ulnclaw setup model|terminal|gateway|tools|agent`,"
        );
        println!("     or fill only missing items with --quick.");
    } else {
        println!();
        let mode = prompt_choice(
            "How would you like to set up ulnclaw?",
            &[
                "Full setup — configure provider, terminal, platforms & tools (recommended)",
                "Blank Slate — everything off except the bare minimum; opt in to each capability",
            ],
            0,
        )?;
        if mode == 1 {
            // Blank slate: defaults only, then configure the model.
            let default = UlncLawConfig::default();
            let content = toml::to_string_pretty(&default).map_err(|e| e.to_string())?;
            std::fs::write(&config_path, content).map_err(|e| e.to_string())?;
            doc = config_cmd::load_toml(&config_path)?;
            println!();
            println!("  ✓ Blank slate: configuration reset to minimal defaults.");
            section_model(&mut doc)?;
            config_cmd::save_toml(&config_path, &doc)?;
            announce_backup(backup_path.as_deref(), &config_path);
            print_final_summary(&config_path)?;
            return Ok(());
        }
    }

    println!();
    println!("  ─── Configuration Location ───");
    println!("  ℹ Config file:  {}", config_path.display());
    println!("  ℹ Secrets file: {}", config_cmd::env_path().display());
    println!("  ℹ Data folder:  {}", home.display());
    println!("  ℹ You can edit these files directly or use `ulnclaw config edit`.");

    // Section order mirrors hermes: model → terminal → agent defaults →
    // gateway → tools. Agent settings are applied silently on first
    // install (hermes `_apply_default_agent_settings`) and skipped after.
    section_model(&mut doc)?;
    section_terminal(&mut doc)?;
    if !is_existing {
        println!();
        println!("  ─── Agent Settings ───");
        println!("  ✓ Recommended agent defaults applied (tune later: `ulnclaw setup agent`).");
    }
    section_gateway(&mut doc)?;
    section_tools(&mut doc)?;

    config_cmd::save_toml(&config_path, &doc)?;
    announce_backup(backup_path.as_deref(), &config_path);
    print_final_summary(&config_path)?;
    Ok(())
}

/// `--quick` on an existing install: fill only what's missing.
fn run_quick_setup(doc: &mut toml::Value) -> Result<(), String> {
    println!();
    println!("  ─── Quick Setup (fill missing items) ───");
    let config = UlncLawConfig::load(None).map_err(|e| e.to_string())?;
    let has_key = config.resolve_api_key().is_some();
    if !has_key {
        println!("  ℹ No API key found — running the model section.");
        section_model(doc)?;
    } else {
        println!("  ✓ Model & API key already configured.");
    }
    if config.messaging.enabled_platform_names().is_empty() {
        if prompt_yes_no("No messaging platforms enabled. Configure one now?", true)? {
            section_gateway(doc)?;
        }
    } else {
        println!("  ✓ Messaging platforms already configured.");
    }
    Ok(())
}

fn run_section(key: &str, doc: &mut toml::Value) -> Result<(), String> {
    match key {
        "model" => section_model(doc),
        "terminal" => section_terminal(doc),
        "gateway" => section_gateway(doc),
        "tools" => section_tools(doc),
        "agent" => section_agent(doc),
        _ => Err(format!("unknown section {key}")),
    }
}

// ── Section: Model & Provider ──────────────────────────────────────────────

fn section_model(doc: &mut toml::Value) -> Result<(), String> {
    println!();
    println!("  ─── Model & Provider ───");
    let current_provider = nested_str(doc, "model.provider").unwrap_or_default();
    let current_model = nested_str(doc, "model.model").unwrap_or_default();

    let choices = provider_choices();
    let default_idx = choices
        .iter()
        .position(|(id, _)| *id == current_provider)
        .unwrap_or(0);
    let labels: Vec<String> = choices
        .iter()
        .map(|(id, label)| {
            if *id == current_provider {
                format!("{label} (current)")
            } else {
                label.to_string()
            }
        })
        .collect();
    let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    let pick = prompt_choice(
        "Which provider should ulnclaw use?",
        &label_refs,
        default_idx,
    )?;
    let provider = choices[pick].0;

    let model_default = if current_model.is_empty() {
        default_model_for(provider)
    } else {
        current_model.as_str()
    };
    let model = prompt_line("Model name", model_default)?;

    let mut base_url: Option<String> = None;
    if provider == "custom" || needs_base_url(provider) {
        let current_base = nested_str(doc, "model.base_url").unwrap_or_default();
        let answer = prompt_line("Base URL (OpenAI-compatible)", &current_base)?;
        if !answer.trim().is_empty() {
            base_url = Some(answer);
        }
    }

    apply_model_answers(doc, provider, &model, base_url.as_deref())?;

    // API key → .env (hermes prompts per-provider; ulnclaw resolves
    // OPENAI_API_KEY / ANTHROPIC_API_KEY).
    if let Some(var) = api_key_env_var(provider) {
        let existing = crate::config::get_env_value(var).unwrap_or_default();
        let masked = if existing.trim().is_empty() {
            "not set".to_string()
        } else {
            format!(
                "set ({}…)",
                &existing[..existing
                    .chars()
                    .take(4)
                    .map(char::len_utf8)
                    .sum::<usize>()
                    .min(existing.len())]
            )
        };
        println!("  ℹ {var}: {masked}");
        let key = prompt_hidden(&format!("{var} (Enter to keep current)"))?;
        if !key.trim().is_empty() {
            config_cmd::set_env_value(var, key.trim())?;
            println!("  ✓ Saved {var} to {}", config_cmd::env_path().display());
        }
    } else {
        println!("  ℹ {provider} is keyless — no API key needed.");
    }
    Ok(())
}

// ── Section: Terminal Backend ──────────────────────────────────────────────

fn section_terminal(doc: &mut toml::Value) -> Result<(), String> {
    println!();
    println!("  ─── Terminal Backend ───");
    let current = nested_str(doc, "terminal.backend").unwrap_or_default();
    let current = if current.is_empty() {
        "local"
    } else {
        current.as_str()
    };
    let idx = match current {
        "docker" => 1,
        "ssh" => 2,
        _ => 0,
    };
    let pick = prompt_choice(
        "Where should agent commands run?",
        &[
            "Local machine (default)",
            "Docker container",
            "Remote host over SSH",
        ],
        idx,
    )?;
    match pick {
        0 => {
            config_cmd::unset_nested(doc, "terminal.backend");
            config_cmd::unset_nested(doc, "terminal.container");
            config_cmd::unset_nested(doc, "terminal.image");
            config_cmd::unset_nested(doc, "terminal.ssh_host");
            config_cmd::unset_nested(doc, "terminal.ssh_user");
        }
        1 => {
            config_cmd::set_nested(
                doc,
                "terminal.backend",
                toml::Value::String("docker".into()),
            )?;
            let container = prompt_line(
                "Container name",
                &nested_str(doc, "terminal.container").unwrap_or_else(|| "ulnclaw".into()),
            )?;
            config_cmd::set_nested(doc, "terminal.container", toml::Value::String(container))?;
            let image = prompt_line(
                "Image (auto-created when missing)",
                &nested_str(doc, "terminal.image").unwrap_or_else(|| "ubuntu:24.04".into()),
            )?;
            config_cmd::set_nested(doc, "terminal.image", toml::Value::String(image))?;
        }
        _ => {
            config_cmd::set_nested(doc, "terminal.backend", toml::Value::String("ssh".into()))?;
            let host = prompt_line(
                "SSH host",
                &nested_str(doc, "terminal.ssh_host").unwrap_or_default(),
            )?;
            config_cmd::set_nested(doc, "terminal.ssh_host", toml::Value::String(host))?;
            let user = prompt_line(
                "SSH user",
                &nested_str(doc, "terminal.ssh_user").unwrap_or_default(),
            )?;
            if !user.trim().is_empty() {
                config_cmd::set_nested(doc, "terminal.ssh_user", toml::Value::String(user))?;
            }
        }
    }
    let timeout = prompt_line(
        "Command timeout (seconds)",
        &nested_str(doc, "terminal.timeout").unwrap_or_else(|| "180".into()),
    )?;
    if let Ok(secs) = timeout.trim().parse::<u64>() {
        config_cmd::set_nested(doc, "terminal.timeout", toml::Value::Integer(secs as i64))?;
    }
    Ok(())
}

// ── Section: Messaging Platforms ───────────────────────────────────────────

fn section_gateway(doc: &mut toml::Value) -> Result<(), String> {
    println!();
    println!("  ─── Messaging Platforms ───");
    println!("  ℹ Connect platforms to chat with ulnclaw from anywhere.");
    let specs = platform_specs();
    let config = UlncLawConfig::load(None).map_err(|e| e.to_string())?;
    let enabled_now: Vec<String> = config
        .messaging
        .enabled_platform_names()
        .iter()
        .map(|s| s.to_string())
        .collect();

    for (i, spec) in specs.iter().enumerate() {
        let status = if enabled_now.iter().any(|p| p == spec.name) {
            "configured"
        } else {
            "not configured"
        };
        println!("    {:>2}. {:<22} ({})", i + 1, spec.label, status);
    }
    let answer = prompt_line(
        "Platforms to configure (comma-separated numbers, blank = none)",
        "",
    )?;
    let selected = match parse_multi_choice(&answer, specs.len()) {
        Ok(v) => v,
        Err(e) => {
            println!("  ⚠ {e} — skipping platform configuration.");
            return Ok(());
        }
    };
    if selected.is_empty() {
        println!("  ℹ No platforms selected. Run `ulnclaw setup gateway` later to configure.");
        return Ok(());
    }

    for idx in selected {
        let spec = &specs[idx];
        println!();
        println!("  ── {} ──", spec.label);
        let key = format!("messaging.{}.enabled", spec.name);
        config_cmd::set_nested(doc, &key, toml::Value::Boolean(true))?;
        for var in spec.envs {
            let existing = crate::config::get_env_value(var).unwrap_or_default();
            if !existing.trim().is_empty() {
                println!("  ✓ {var} already set.");
                continue;
            }
            let value = prompt_hidden(&format!("{var}"))?;
            if value.trim().is_empty() {
                println!("  ⚠ Skipped {var} — set it later: ulnclaw config set {var} <value>");
            } else {
                config_cmd::set_env_value(var, value.trim())?;
                println!("  ✓ Saved {var} to {}", config_cmd::env_path().display());
            }
        }
        if let Some(guidance) = spec.guidance {
            println!("  ℹ {guidance}");
        }
    }

    let missing = missing_home_channels(&|var| crate::config::get_env_value(var));
    if !missing.is_empty() {
        println!();
        println!("  ℹ Home channel not set for: {}.", missing.join(", "));
        println!("    The home channel receives proactive notifications. Set e.g.:");
        println!("    ulnclaw config set TELEGRAM_HOME_CHANNEL <chat-id>");
    }
    Ok(())
}

// ── Section: Tools ─────────────────────────────────────────────────────────

fn section_tools(doc: &mut toml::Value) -> Result<(), String> {
    println!();
    println!("  ─── Tools ───");
    let registry = crate::toolsets::toolsets();
    let mut names: Vec<&str> = registry.keys().copied().collect();
    names.sort_unstable();
    let current = crate::config::UlncLawConfig::load(None)
        .map(|c| c.enabled_toolsets)
        .unwrap_or_default();

    for (i, name) in names.iter().enumerate() {
        let def = &registry[*name];
        let mark = if current.iter().any(|t| t == name) {
            "✓"
        } else {
            " "
        };
        println!(
            "    {:>2}. [{}] {:<14} {}",
            i + 1,
            mark,
            name,
            def.description
        );
    }
    println!("  ℹ Blank keeps the default \"coding\" set; mark additional toolsets to enable.");
    let answer = prompt_line("Toolsets to enable (comma-separated numbers)", "")?;
    if answer.trim().is_empty() {
        println!("  ℹ Keeping default toolset selection.");
        return Ok(());
    }
    match parse_multi_choice(&answer, names.len()) {
        Ok(picks) => {
            let selected: Vec<toml::Value> = picks
                .iter()
                .map(|i| toml::Value::String(names[*i].to_string()))
                .collect();
            config_cmd::set_nested(doc, "enabled_toolsets", toml::Value::Array(selected))?;
            println!("  ✓ Toolset selection saved.");
        }
        Err(e) => println!("  ⚠ {e} — keeping current toolset selection."),
    }
    Ok(())
}

// ── Section: Agent Settings ────────────────────────────────────────────────

fn section_agent(doc: &mut toml::Value) -> Result<(), String> {
    println!();
    println!("  ─── Agent Settings ───");
    let approval_default = nested_bool(doc, "agent.approval").unwrap_or(true);
    let approval = prompt_yes_no(
        "Ask for confirmation before dangerous commands?",
        approval_default,
    )?;
    config_cmd::set_nested(doc, "agent.approval", toml::Value::Boolean(approval))?;
    let iterations = prompt_line(
        "Max tool-call iterations per run",
        &nested_str(doc, "agent.max_iterations").unwrap_or_else(|| "90".into()),
    )?;
    if let Ok(n) = iterations.trim().parse::<i64>() {
        if n > 0 {
            config_cmd::set_nested(doc, "agent.max_iterations", toml::Value::Integer(n))?;
        }
    }
    let verbose_default = nested_bool(doc, "agent.verbose").unwrap_or(false);
    let verbose = prompt_yes_no("Verbose logging to stderr?", verbose_default)?;
    config_cmd::set_nested(doc, "agent.verbose", toml::Value::Boolean(verbose))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Prompt primitives (mirror the main.rs qq_* helpers)
// ---------------------------------------------------------------------------

pub(crate) fn is_interactive_stdin() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

fn print_header(title: &str) {
    println!("  ─── {} ───", title);
}

pub(crate) fn prompt_line(prompt: &str, default: &str) -> Result<String, String> {
    if default.is_empty() {
        print!("  {}: ", prompt);
    } else {
        print!("  {} [{}]: ", prompt, default);
    }
    std::io::stdout().flush().ok();
    let mut line = String::new();
    if std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| e.to_string())?
        == 0
    {
        return Ok(default.to_string());
    }
    let trimmed = line.trim();
    Ok(if trimmed.is_empty() {
        default.to_string()
    } else {
        trimmed.to_string()
    })
}

pub(crate) fn prompt_hidden(prompt: &str) -> Result<String, String> {
    use crossterm::event::{Event, KeyCode, KeyEventKind};
    use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
    print!("  {}: ", prompt);
    std::io::stdout().flush().ok();
    if enable_raw_mode().is_err() {
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .map_err(|e| e.to_string())?;
        println!();
        return Ok(line.trim().to_string());
    }
    let mut value = String::new();
    loop {
        match crossterm::event::read() {
            Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Enter => break,
                KeyCode::Backspace => {
                    value.pop();
                }
                KeyCode::Char(c) => value.push(c),
                KeyCode::Esc => {
                    disable_raw_mode().ok();
                    println!();
                    return Ok(String::new());
                }
                _ => {}
            },
            Err(e) => {
                disable_raw_mode().ok();
                return Err(e.to_string());
            }
            _ => {}
        }
    }
    disable_raw_mode().ok();
    println!();
    Ok(value)
}

pub(crate) fn prompt_yes_no(prompt: &str, default: bool) -> Result<bool, String> {
    let suffix = if default { "[Y/n]" } else { "[y/N]" };
    let answer = prompt_line(&format!("{} {}", prompt, suffix), "")?;
    Ok(match answer.trim().to_lowercase().as_str() {
        "y" | "yes" => true,
        "n" | "no" => false,
        _ => default,
    })
}

pub(crate) fn prompt_choice(
    prompt: &str,
    choices: &[&str],
    default_index: usize,
) -> Result<usize, String> {
    println!();
    println!("  {}", prompt);
    for (idx, choice) in choices.iter().enumerate() {
        println!("    {}. {}", idx + 1, choice);
    }
    loop {
        let answer = prompt_line(&format!("Choice [{}]", default_index + 1), "")?;
        if answer.trim().is_empty() {
            return Ok(default_index);
        }
        if let Ok(n) = answer.trim().parse::<usize>() {
            if n >= 1 && n <= choices.len() {
                return Ok(n - 1);
            }
        }
        println!("  Please enter a number between 1 and {}.", choices.len());
    }
}

// ---------------------------------------------------------------------------
// Small config-document helpers
// ---------------------------------------------------------------------------

fn nested_str(doc: &toml::Value, key: &str) -> Option<String> {
    config_cmd::get_nested(doc, key)
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

fn nested_bool(doc: &toml::Value, key: &str) -> Option<bool> {
    config_cmd::get_nested(doc, key).and_then(|v| v.as_bool())
}

fn announce_backup(backup: Option<&Path>, config_path: &Path) {
    if let Some(backup) = backup {
        println!();
        println!("  ℹ Previous config backed up to: {}", backup.display());
        println!("    If setup changed a value you customized, restore it with:");
        println!("    cp {} {}", backup.display(), config_path.display());
    }
}

fn print_final_summary(config_path: &Path) -> Result<(), String> {
    let config = UlncLawConfig::load(None).map_err(|e| e.to_string())?;
    let platforms: Vec<String> = config
        .messaging
        .enabled_platform_names()
        .iter()
        .map(|s| s.to_string())
        .collect();
    let has_key = config.resolve_api_key().is_some();
    print!(
        "{}",
        render_summary(
            config_path,
            &config_cmd::env_path(),
            &config.model.provider,
            &config.model.model,
            has_key,
            &platforms,
            &config.enabled_toolsets,
        )
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sections_registry_is_complete() {
        let keys: Vec<&str> = SECTIONS.iter().map(|(k, _)| *k).collect();
        assert_eq!(keys, vec!["model", "terminal", "gateway", "tools", "agent"]);
        assert_eq!(section_label("model"), Some("Model & Provider"));
        assert_eq!(section_label("nope"), None);
    }

    #[test]
    fn api_key_env_var_mapping() {
        assert_eq!(api_key_env_var("anthropic"), Some("ANTHROPIC_API_KEY"));
        assert_eq!(api_key_env_var("openai"), Some("OPENAI_API_KEY"));
        assert_eq!(api_key_env_var("openrouter"), Some("OPENAI_API_KEY"));
        assert_eq!(api_key_env_var("ollama"), None);
        assert_eq!(api_key_env_var("llamacpp"), None);
    }

    #[test]
    fn parse_multi_choice_handles_common_input() {
        assert_eq!(parse_multi_choice("", 5).unwrap(), Vec::<usize>::new());
        assert_eq!(parse_multi_choice("1,3", 5).unwrap(), vec![0, 2]);
        assert_eq!(parse_multi_choice("3 1 1", 5).unwrap(), vec![0, 2]);
        assert!(parse_multi_choice("0", 5).is_err());
        assert!(parse_multi_choice("6", 5).is_err());
        assert!(parse_multi_choice("x", 5).is_err());
    }

    #[test]
    fn apply_model_answers_sets_and_clears_base_url() {
        let mut doc: toml::Value = toml::from_str("").unwrap();
        apply_model_answers(
            &mut doc,
            "openrouter",
            "openrouter/auto",
            Some("https://openrouter.ai/api/v1"),
        )
        .unwrap();
        assert_eq!(
            nested_str(&doc, "model.provider").as_deref(),
            Some("openrouter")
        );
        assert_eq!(
            nested_str(&doc, "model.model").as_deref(),
            Some("openrouter/auto")
        );
        assert_eq!(
            nested_str(&doc, "model.base_url").as_deref(),
            Some("https://openrouter.ai/api/v1")
        );
        // Blank base URL clears it again.
        apply_model_answers(&mut doc, "openai", "gpt-4.1", None).unwrap();
        assert!(nested_str(&doc, "model.base_url").is_none());
    }

    #[test]
    fn platform_table_covers_token_platforms() {
        assert_eq!(platform_env("telegram"), &["TELEGRAM_BOT_TOKEN"]);
        assert_eq!(
            platform_env("slack"),
            &["SLACK_BOT_TOKEN", "SLACK_APP_TOKEN"]
        );
        assert!(platform_env("weixin").is_empty());
        let specs = platform_specs();
        assert!(specs.len() >= 25, "expected full platform roster");
        assert!(specs
            .iter()
            .all(|s| !s.name.is_empty() && !s.label.is_empty()));
    }

    #[test]
    fn missing_home_channels_flags_token_without_home() {
        let missing = missing_home_channels(&|var| match var {
            "TELEGRAM_BOT_TOKEN" => Some("token".into()),
            "DISCORD_BOT_TOKEN" => Some("token".into()),
            "DISCORD_HOME_CHANNEL" => Some("123".into()),
            _ => None,
        });
        assert_eq!(missing, vec!["Telegram"]);
    }

    #[test]
    fn render_summary_marks_missing_key() {
        let out = render_summary(
            Path::new("/tmp/config.toml"),
            Path::new("/tmp/.env"),
            "openai",
            "gpt-4.1",
            false,
            &[],
            &[],
        );
        assert!(out.contains("MISSING"));
        assert!(out.contains("coding (default)"));
        assert!(out.contains("none (run `ulnclaw setup gateway` later)"));
    }

    #[test]
    fn noninteractive_guidance_mentions_config_set() {
        let out = noninteractive_guidance(Some("no tty"));
        assert!(out.contains("ulnclaw config set model.provider"));
        assert!(out.contains("no tty"));
    }

    #[test]
    fn backup_config_roundtrip() {
        let dir = std::env::temp_dir().join(format!("ulnclaw-setup-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("config.toml");
        std::fs::write(&cfg, "x = 1").unwrap();
        let backup = backup_config(&cfg).expect("backup made");
        assert!(backup.exists());
        assert!(backup
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("config.toml.bak."));
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), "x = 1");
        // Missing file → no backup.
        assert!(backup_config(&dir.join("missing.toml")).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn provider_and_model_defaults_are_sane() {
        assert!(provider_choices().iter().any(|(id, _)| *id == "openai"));
        assert_eq!(default_model_for("anthropic"), "claude-sonnet-4-5");
        assert!(needs_base_url("custom"));
        assert!(!needs_base_url("openai"));
        assert!(!needs_base_url("moa"));
    }
}

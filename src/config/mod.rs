//! Configuration layer — port of hermes_cli/config.py
//!
//! Config lives at `~/.ulnclaw/config.toml`. Environment variables can be
//! placed in `~/.ulnclaw/.env` (KEY=VALUE lines). Values resolve with the
//! precedence: explicit override > environment > .env file > config.toml
//! default.

use crate::error::{AgentError, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Default model used when nothing is configured (mirrors hermes' default).
pub const DEFAULT_MODEL: &str = "gpt-5.2";
/// Default provider when nothing is configured.
pub const DEFAULT_PROVIDER: &str = "openai";

/// Resolve the ulnclaw home directory (`ULNCLAW_HOME` override or `~/.ulnclaw`).
pub fn ulnclaw_home() -> PathBuf {
    if let Ok(dir) = std::env::var("ULNCLAW_HOME") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    // Hermes compatibility: fall back to HERMES_HOME when set so existing
    // installs can share state during migration.
    if let Ok(dir) = std::env::var("HERMES_HOME") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".ulnclaw")
}

/// Ensure home + standard subdirectories exist.
pub fn ensure_home() -> Result<PathBuf> {
    let home = ulnclaw_home();
    for sub in [
        "",
        "memory",
        "skills",
        "cron",
        "sessions",
        "logs",
        "sandboxes",
    ] {
        let dir = if sub.is_empty() {
            home.clone()
        } else {
            home.join(sub)
        };
        std::fs::create_dir_all(&dir)
            .map_err(|e| AgentError::config(format!("cannot create {}: {}", dir.display(), e)))?;
    }
    Ok(home)
}

/// Load a KEY=VALUE env file (`.env`), ignoring comments/blank lines.
pub fn load_env_file(path: &Path) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if let Ok(content) = std::fs::read_to_string(path) {
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let line = line.strip_prefix("export ").unwrap_or(line);
            if let Some((key, value)) = line.split_once('=') {
                let value = value.trim().trim_matches(|c| c == '"' || c == '\'');
                map.insert(key.trim().to_string(), value.to_string());
            }
        }
    }
    map
}

/// Keys applied by the last `reload_env` — the removal set when a key
/// disappears from `.env` (never clobbers unrelated environment; the
/// hermes known-keys removal policy).
fn reload_applied_keys() -> &'static std::sync::Mutex<std::collections::BTreeSet<String>> {
    static KEYS: std::sync::OnceLock<std::sync::Mutex<std::collections::BTreeSet<String>>> =
        std::sync::OnceLock::new();
    KEYS.get_or_init(|| std::sync::Mutex::new(std::collections::BTreeSet::new()))
}

/// Re-read `<home>/.env` into the process environment (hermes
/// `reload_env` parity): adds/updates vars that changed and removes
/// vars a previous reload applied that are gone from the file now.
/// Returns the number of vars updated or removed.
pub fn reload_env() -> usize {
    let env_file = ulnclaw_home().join(".env");
    let vars = load_env_file(&env_file);
    let mut count = 0;
    for (key, value) in &vars {
        if std::env::var(key).ok().as_deref() != Some(value.as_str()) {
            std::env::set_var(key, value);
            count += 1;
        }
    }
    let mut applied = reload_applied_keys().lock().unwrap();
    for key in applied.iter() {
        if !vars.contains_key(key) && std::env::var(key).is_ok() {
            std::env::remove_var(key);
            count += 1;
        }
    }
    *applied = vars.keys().cloned().collect();
    count
}

/// Config-aware env lookup: process env first, then `~/.ulnclaw/.env`.
/// Port of hermes' `get_env_value`.
pub fn get_env_value(name: &str) -> Option<String> {
    if let Ok(val) = std::env::var(name) {
        if !val.is_empty() {
            return Some(val);
        }
    }
    let env_file = ulnclaw_home().join(".env");
    load_env_file(&env_file).remove(name)
}

/// Web search backend selection (port of `web.backend` config).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WebConfig {
    /// Search backend: "duckduckgo" (built-in), "tavily", "brave", "searxng", "auto".
    #[serde(default)]
    pub search_backend: Option<String>,
    /// Extract backend: "http" (built-in fetch+strip), "firecrawl", ...
    #[serde(default)]
    pub extract_backend: Option<String>,
}

/// `[browser]` — browser tool endpoint + cloud provider selection
/// (hermes `browser:` config block).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct BrowserConfig {
    /// Persistent CDP endpoint: `ws://`, `http://host:port`, or `auto`
    /// (hermes `browser.cdp_url`). Live override: `ULNCLAW_BROWSER_CDP`.
    pub cdp_url: Option<String>,
    /// Cloud browser backend (`browserbase` / `browser-use` / `firecrawl`),
    /// or `"local"` to disable cloud mode entirely (hermes
    /// `browser.cloud_provider`). Unset: hermes legacy availability walk
    /// (browser-use → browserbase; firecrawl is explicit-only).
    pub cloud_provider: Option<String>,
    /// Prefer the managed Nous tool gateway for browser backends even when
    /// a direct API key is set (hermes `tool_gateway.browser: gateway` /
    /// `browser.use_gateway`).
    pub use_gateway: Option<Truthiness>,
}

/// Model/provider settings (port of the model section of hermes config).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    #[serde(default = "default_provider")]
    pub provider: String,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Fallback chain of "provider:model" strings.
    #[serde(default)]
    pub fallbacks: Vec<String>,
    /// Retry transient provider errors (429/5xx/network) this many times
    /// with exponential backoff before giving up.
    #[serde(default = "default_max_retries")]
    pub max_retries: usize,
}

fn default_max_retries() -> usize {
    2
}

fn default_provider() -> String {
    DEFAULT_PROVIDER.to_string()
}
fn default_model() -> String {
    DEFAULT_MODEL.to_string()
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            provider: default_provider(),
            model: default_model(),
            base_url: None,
            api_key: None,
            temperature: None,
            max_tokens: None,
            fallbacks: Vec::new(),
            max_retries: default_max_retries(),
        }
    }
}

/// `[providers.<slug>]` — user-defined provider entry (port of hermes'
/// v12+ keyed `providers:` config form). Rows surface in the
/// `/api/model/options` picker inventory; the runtime keeps using the
/// `[model]` section for the active provider.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CustomProviderConfig {
    /// Endpoint base URL (OpenAI-compatible `/v1` root or Anthropic root).
    #[serde(default)]
    pub base_url: Option<String>,
    /// Literal API key (takes priority over `key_env`).
    #[serde(default)]
    pub api_key: Option<String>,
    /// Environment variable holding the API key.
    #[serde(default)]
    pub key_env: Option<String>,
    /// Default model for this provider in pickers.
    #[serde(default)]
    pub model: Option<String>,
    /// Wire dialect: `openai` (default, OpenAI-compatible) or `anthropic`.
    #[serde(default)]
    pub mode: Option<String>,
}

impl CustomProviderConfig {
    fn nonblank(value: Option<&String>) -> Option<String> {
        value
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    }

    pub fn base_url(&self) -> Option<String> {
        Self::nonblank(self.base_url.as_ref())
    }

    pub fn api_key(&self) -> Option<String> {
        Self::nonblank(self.api_key.as_ref())
    }

    pub fn model(&self) -> Option<String> {
        Self::nonblank(self.model.as_ref())
    }

    /// Wire dialect, normalized (`anthropic` or `openai`).
    pub fn mode(&self) -> String {
        match Self::nonblank(self.mode.as_ref())
            .map(|v| v.to_lowercase())
            .as_deref()
        {
            Some("anthropic") => "anthropic".to_string(),
            _ => "openai".to_string(),
        }
    }

    /// Resolve the API key: literal `api_key`, else `key_env` lookup.
    pub fn resolved_api_key(&self) -> Option<String> {
        if let Some(key) = self.api_key() {
            return Some(key);
        }
        Self::nonblank(self.key_env.as_ref()).and_then(|name| get_env_value(&name))
    }
}

/// `[model_catalog]` — picker catalog knobs (hermes `model_catalog:`).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelCatalogConfig {
    /// Provider slugs hidden from the model-options picker inventory.
    #[serde(default)]
    pub excluded_providers: Vec<String>,
}

/// Agent behavior settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSettings {
    /// Max tool-call iterations per run (hermes: iteration budget).
    #[serde(default = "default_max_iterations")]
    pub max_iterations: usize,
    /// Whether to ask for confirmation before dangerous commands.
    #[serde(default = "default_true")]
    pub approval: bool,
    /// Execute independent tool calls concurrently.
    #[serde(default)]
    pub concurrent_tool_execution: bool,
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent_tools: usize,
    /// Context window budget (tokens) before compression kicks in.
    #[serde(default = "default_context_budget")]
    pub context_budget_tokens: usize,
    /// Verbose logging to stderr.
    #[serde(default)]
    pub verbose: bool,
    /// Probe the local Python toolchain for the system prompt
    /// (hermes `agent.environment_probe`, default true).
    #[serde(default = "default_true")]
    pub environment_probe: bool,
    /// Reasoning effort for model calls (hermes `agent.reasoning_effort`):
    /// none | minimal | low | medium | high | xhigh | max | ultra.
    /// Empty string = the provider endpoint's own default. The CLI
    /// `--reasoning` flag overrides this for one invocation; kanban
    /// workers pin it per task via the `reasoning_effort` column.
    #[serde(default)]
    pub reasoning_effort: String,
    /// Priority Processing pin (hermes `agent.service_tier`): "fast"
    /// sends `service_tier=priority` on every OpenAI-compatible request;
    /// "normal" or empty keeps the endpoint default. The gateway `/fast`
    /// slash command and `PUT /api/fast` write this key.
    #[serde(default)]
    pub service_tier: String,
    /// System-prompt override (hermes `agent.system_prompt`): replaces
    /// the built-in system prompt when non-empty. The `/personality`
    /// slash command writes the active personality's resolved prompt
    /// here.
    #[serde(default)]
    pub system_prompt: String,
    /// Name of the active personality (ulnclaw bookkeeping over hermes
    /// `agent.system_prompt` writes): empty = the default persona.
    #[serde(default)]
    pub personality: String,
    /// Named personalities for `/personality` (hermes
    /// `agent.personalities`): bare prompt strings or tables with
    /// system_prompt/tone/style/description.
    #[serde(default)]
    pub personalities: HashMap<String, PersonalityDef>,
}

/// One `/personality` entry (hermes `agent.personalities.<name>`):
/// either a bare prompt string or a table with system_prompt/tone/
/// style/description keys.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PersonalityDef {
    Prompt(String),
    Detailed {
        #[serde(default)]
        system_prompt: String,
        #[serde(default)]
        tone: String,
        #[serde(default)]
        style: String,
        #[serde(default)]
        description: String,
    },
}

impl PersonalityDef {
    /// The full prompt injected into `agent.system_prompt` when this
    /// personality is activated (hermes `_resolve_prompt`).
    pub fn resolve_prompt(&self) -> String {
        match self {
            PersonalityDef::Prompt(prompt) => prompt.clone(),
            PersonalityDef::Detailed {
                system_prompt,
                tone,
                style,
                ..
            } => {
                let mut parts = vec![system_prompt.clone()];
                if !tone.trim().is_empty() {
                    parts.push(format!("Tone: {}", tone.trim()));
                }
                if !style.trim().is_empty() {
                    parts.push(format!("Style: {}", style.trim()));
                }
                parts
                    .into_iter()
                    .filter(|part| !part.is_empty())
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }
    }

    /// Short listing preview (hermes personality list): the description
    /// when present, else the clipped prompt head.
    pub fn preview(&self) -> String {
        let raw = match self {
            PersonalityDef::Prompt(prompt) => prompt.clone(),
            PersonalityDef::Detailed {
                description,
                system_prompt,
                ..
            } => {
                if !description.trim().is_empty() {
                    description.clone()
                } else {
                    system_prompt.clone()
                }
            }
        };
        let first_line = raw.lines().next().unwrap_or("").trim();
        let clipped: String = first_line.chars().take(50).collect();
        if clipped.is_empty() {
            "(empty prompt)".to_string()
        } else {
            clipped
        }
    }
}

fn default_max_iterations() -> usize {
    90
}
fn default_max_concurrent() -> usize {
    5
}
fn default_context_budget() -> usize {
    120_000
}
fn default_true() -> bool {
    true
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            max_iterations: default_max_iterations(),
            approval: true,
            concurrent_tool_execution: false,
            max_concurrent_tools: default_max_concurrent(),
            context_budget_tokens: default_context_budget(),
            verbose: false,
            environment_probe: true,
            reasoning_effort: String::new(),
            service_tier: String::new(),
            system_prompt: String::new(),
            personality: String::new(),
            personalities: HashMap::new(),
        }
    }
}

/// Delegation limits (port of hermes `delegation` config).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelegationConfig {
    #[serde(default = "default_max_children")]
    pub max_concurrent_children: usize,
    #[serde(default = "default_child_iterations")]
    pub child_max_iterations: usize,
    /// Max depth for nested delegation.
    #[serde(default = "default_max_depth")]
    pub max_depth: usize,
}

fn default_max_children() -> usize {
    3
}
fn default_child_iterations() -> usize {
    30
}
fn default_max_depth() -> usize {
    1
}

impl Default for DelegationConfig {
    fn default() -> Self {
        Self {
            max_concurrent_children: default_max_children(),
            child_max_iterations: default_child_iterations(),
            max_depth: default_max_depth(),
        }
    }
}

/// Terminal tool settings (port of TERMINAL_* env defaults).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalConfig {
    #[serde(default = "default_terminal_timeout")]
    pub timeout: u64,
    /// Foreground max timeout in seconds (hermes FOREGROUND_MAX_TIMEOUT).
    #[serde(default = "default_fg_max")]
    pub foreground_max_timeout: u64,
    #[serde(default)]
    pub cwd: Option<String>,
    /// Execution backend: "local" (default), "docker", or "ssh".
    #[serde(default)]
    pub backend: Option<String>,
    /// Docker container name (backend="docker").
    #[serde(default)]
    pub container: Option<String>,
    /// Docker image to auto-create the container from when missing.
    #[serde(default)]
    pub image: Option<String>,
    /// SSH host (backend="ssh").
    #[serde(default)]
    pub ssh_host: Option<String>,
    /// SSH user (backend="ssh").
    #[serde(default)]
    pub ssh_user: Option<String>,
    /// SSH port (backend="ssh").
    #[serde(default)]
    pub ssh_port: Option<u16>,
    /// Env vars explicitly allowed through the sandbox credential scrub
    /// (hermes `terminal.env_passthrough`).
    #[serde(default)]
    pub env_passthrough: Vec<String>,
    /// SSH identity file (backend="ssh").
    #[serde(default)]
    pub ssh_identity: Option<String>,
    /// Docker: mount the host messaging cwd into `/workspace` (hermes
    /// `terminal.docker_mount_cwd_to_workspace`).
    #[serde(default)]
    pub docker_mount_cwd_to_workspace: bool,
}

fn default_terminal_timeout() -> u64 {
    180
}
fn default_fg_max() -> u64 {
    600
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            timeout: default_terminal_timeout(),
            foreground_max_timeout: default_fg_max(),
            cwd: None,
            backend: None,
            container: None,
            image: None,
            ssh_host: None,
            ssh_user: None,
            ssh_port: None,
            ssh_identity: None,
            docker_mount_cwd_to_workspace: false,
            env_passthrough: Vec::new(),
        }
    }
}

/// Memory limits (port of MemoryStore defaults: 2200/1375 chars).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
    #[serde(default = "default_memory_limit")]
    pub memory_char_limit: usize,
    #[serde(default = "default_user_limit")]
    pub user_char_limit: usize,
}

fn default_memory_limit() -> usize {
    2200
}
fn default_user_limit() -> usize {
    1375
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            memory_char_limit: default_memory_limit(),
            user_char_limit: default_user_limit(),
        }
    }
}

/// Lenient boolean toggle — accepts a real TOML boolean or one of the
/// common string spellings (hermes `is_truthy_value`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum Truthiness {
    Flag(bool),
    Text(String),
}

impl Truthiness {
    /// Resolve to a concrete bool; unrecognized text falls back to
    /// `default` (hermes `is_truthy_value(value, default=...)`).
    pub fn resolve(&self, default: bool) -> bool {
        match self {
            Truthiness::Flag(flag) => *flag,
            Truthiness::Text(text) => match text.trim().to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "on" | "y" | "t" => true,
                "0" | "false" | "no" | "off" | "n" | "f" | "" => false,
                _ => default,
            },
        }
    }
}

/// Per-task auxiliary provider override (port of hermes' `auxiliary.<task>`
/// config consumed by `agent/auxiliary_client.py`).
///
/// Tasks are auxiliary LLM calls such as `compression` (context
/// summarization), `vision` (image analysis), and `title_generation`
/// (session titles). When an entry is present,
/// that task is routed through the configured provider/model instead of the
/// main runtime. Blank values and `"auto"` inherit the main runtime.
///
/// `title_generation` additionally reads `enabled` (kill switch, default
/// true) and `language` (pin the title language instead of matching the
/// user's) — hermes `auxiliary.title_generation.{enabled,language}`.

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuxiliaryTaskConfig {
    /// Provider name ("openai", "anthropic", "ollama", ...); "auto"/blank
    /// inherits the main provider.
    #[serde(default)]
    pub provider: Option<String>,
    /// Model id; "auto"/blank inherits the main model.
    #[serde(default)]
    pub model: Option<String>,
    /// Endpoint override; blank uses the provider default.
    #[serde(default)]
    pub base_url: Option<String>,
    /// API key; blank falls back to `key_env`, then the main runtime key.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Environment variable holding the API key.
    #[serde(default)]
    pub key_env: Option<String>,
    /// Task kill switch — only `title_generation` reads this (hermes
    /// `auxiliary.title_generation.enabled`, default true).
    #[serde(default)]
    pub enabled: Option<Truthiness>,
    /// Title language pin — only `title_generation` reads this (hermes
    /// `auxiliary.title_generation.language`); blank matches the user's
    /// language.
    #[serde(default)]
    pub language: Option<String>,
    /// Task output budget override (hermes `auxiliary.goal_judge.max_tokens`);
    /// unset or non-positive falls back to the task's default.
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

impl AuxiliaryTaskConfig {
    fn nonblank(value: Option<&String>) -> Option<String> {
        value
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    }

    /// Provider override, with "auto" normalized to "inherit".
    pub fn provider(&self) -> Option<String> {
        Self::nonblank(self.provider.as_ref()).filter(|v| v != "auto")
    }

    /// Model override, with "auto" normalized to "inherit".
    pub fn model(&self) -> Option<String> {
        Self::nonblank(self.model.as_ref()).filter(|v| v != "auto")
    }

    pub fn base_url(&self) -> Option<String> {
        Self::nonblank(self.base_url.as_ref())
    }

    pub fn api_key(&self) -> Option<String> {
        Self::nonblank(self.api_key.as_ref())
    }

    /// Resolve the task API key: literal `api_key`, else `key_env` lookup.
    pub fn resolved_api_key(&self) -> Option<String> {
        if let Some(key) = self.api_key() {
            return Some(key);
        }
        Self::nonblank(self.key_env.as_ref()).and_then(|name| get_env_value(&name))
    }

    /// `title_generation` kill switch (hermes
    /// `auxiliary.title_generation.enabled`); defaults to true.
    pub fn enabled(&self) -> bool {
        self.enabled
            .as_ref()
            .map(|v| v.resolve(true))
            .unwrap_or(true)
    }

    /// `title_generation` language pin (hermes
    /// `auxiliary.title_generation.language`); blank = match the user.
    pub fn language(&self) -> Option<String> {
        Self::nonblank(self.language.as_ref())
    }

    /// Task output budget override (hermes `_goal_judge_max_tokens`);
    /// non-positive values fall back to the task default.
    pub fn max_tokens(&self) -> Option<u32> {
        self.max_tokens.filter(|v| *v > 0)
    }

    /// True when the entry carries no usable override at all.
    pub fn is_empty(&self) -> bool {
        self.provider().is_none()
            && self.model().is_none()
            && self.base_url().is_none()
            && self.api_key().is_none()
            && Self::nonblank(self.key_env.as_ref()).is_none()
    }
}

/// Mixture-of-Agents slot — one provider/model selection (reference or
/// aggregator). Port of hermes' MoA preset slots (`moa_config.py`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MoaSlot {
    pub provider: String,
    pub model: String,
    /// Disabled slots are skipped in the fan-out (hermes `enabled`).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Endpoint override; blank uses the provider default.
    #[serde(default)]
    pub base_url: Option<String>,
    /// API key; blank falls back to `key_env`, then the main runtime key.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Environment variable holding the API key.
    #[serde(default)]
    pub key_env: Option<String>,
}

impl MoaSlot {
    /// `provider:model` display label (hermes `_slot_label`).
    pub fn label(&self) -> String {
        format!("{}:{}", self.provider.trim(), self.model.trim())
    }

    /// Resolve the slot API key: literal, else `key_env`, else main key.
    pub fn resolved_api_key(&self, config: &UlncLawConfig) -> Option<String> {
        if let Some(key) = self
            .api_key
            .as_ref()
            .map(|k| k.trim())
            .filter(|k| !k.is_empty())
        {
            return Some(key.to_string());
        }
        if let Some(name) = self
            .key_env
            .as_ref()
            .map(|k| k.trim())
            .filter(|k| !k.is_empty())
        {
            if let Some(key) = get_env_value(name) {
                return Some(key);
            }
        }
        config.resolve_api_key()
    }
}

/// One MoA preset: fan-out references + a synthesizing aggregator.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MoaPreset {
    /// Reference models run in parallel on the user prompt.
    #[serde(default)]
    pub reference_models: Vec<MoaSlot>,
    /// Aggregator synthesizes the reference outputs.
    pub aggregator: MoaSlot,
    /// Optional sampling overrides for the reference fan-out.
    #[serde(default)]
    pub reference_temperature: Option<f32>,
    #[serde(default)]
    pub reference_max_tokens: Option<u32>,
    #[serde(default)]
    pub aggregator_temperature: Option<f32>,
    /// "loud" (default) reports failed references; "silent" hides them.
    #[serde(default = "default_degraded_policy")]
    pub degraded_reference_policy: String,
}

fn default_degraded_policy() -> String {
    "loud".to_string()
}

/// `[moa]` config section (hermes `moa:` presets).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MoaConfig {
    /// Preset used when none is requested (hermes `default_preset`).
    #[serde(default)]
    pub default_preset: Option<String>,
    /// Named presets.
    #[serde(default)]
    pub presets: HashMap<String, MoaPreset>,
    /// Persist one JSONL record per MoA turn under `<home>/moa-traces/`
    /// (hermes `moa.save_traces`).
    #[serde(default)]
    pub save_traces: bool,
    /// Override the trace directory (hermes `moa.trace_dir`).
    #[serde(default)]
    pub trace_dir: Option<String>,
    /// PII filter for reference outputs (hermes `moa.privacy_filter`):
    /// off (default) | `display` (redact user-visible surfaces) | `full`
    /// (also redact the text injected into the aggregator prompt).
    #[serde(default)]
    pub privacy_filter: Option<MoaPrivacyFilter>,
}

/// `moa.privacy_filter` value: tolerant read (hermes `coerce_privacy_filter`
/// contract) — booleans and unknown strings degrade instead of failing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MoaPrivacyFilter {
    Flag(bool),
    Mode(String),
}

impl MoaPrivacyFilter {
    /// Normalize to `""` (off), `"display"`, or `"full"`.
    pub fn mode(&self) -> &'static str {
        match self {
            MoaPrivacyFilter::Flag(true) => "full",
            MoaPrivacyFilter::Flag(false) => "",
            MoaPrivacyFilter::Mode(raw) => {
                let mode = raw.trim().to_lowercase();
                match mode.as_str() {
                    "display" | "full" => match mode.as_str() {
                        "display" => "display",
                        _ => "full",
                    },
                    "true" | "on" | "yes" | "1" => "full",
                    _ => "",
                }
            }
        }
    }
}

impl MoaConfig {
    /// Resolved privacy-filter mode ("" | "display" | "full").
    pub fn privacy_mode(&self) -> &'static str {
        self.privacy_filter.as_ref().map(|v| v.mode()).unwrap_or("")
    }
}

impl MoaConfig {
    /// Resolve a preset by name, falling back to `default_preset` then
    /// `"default"` (hermes `resolve_moa_preset` semantics).
    pub fn resolve_preset(&self, name: Option<&str>) -> Result<(String, &MoaPreset)> {
        let wanted = name
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                self.default_preset
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "default".to_string());
        if let Some(preset) = self.presets.get(&wanted) {
            return Ok((wanted, preset));
        }
        let mut available: Vec<&String> = self.presets.keys().collect();
        available.sort();
        let listed = if available.is_empty() {
            "(none configured)".to_string()
        } else {
            available
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        Err(AgentError::config(format!(
            "MoA preset '{}' was not found. Available presets: {}",
            wanted, listed
        )))
    }
}

/// HTTP gateway settings (`[gateway]`) — port of hermes' api_server platform.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayConfig {
    #[serde(default = "default_gateway_host")]
    pub host: String,
    #[serde(default = "default_gateway_port")]
    pub port: u16,
    /// Bearer token required on API routes (env ULNCLAW_GATEWAY_KEY wins).
    #[serde(default)]
    pub key: Option<String>,
    /// Serve every route additionally under `/p/<profile>/...` mirrors,
    /// each backed by its own agent/store for that `[profiles.<name>]`
    /// override (hermes `gateway.multiplex_profiles`). Off by default:
    /// the prefix is accepted but ignored (single-profile behavior).
    #[serde(default)]
    pub multiplex_profiles: bool,
    /// Hierarchical platform→profile routing rules (hermes
    /// `gateway.profile_routes`): matching inbound messaging chats run
    /// under the routed profile's agent/home instead of the default.
    /// Only active when `multiplex_profiles` is on (hermes
    /// `_profile_name_for_source` gating).
    #[serde(default)]
    pub profile_routes: Vec<crate::profile_routing::ProfileRouteSpec>,
    /// Render one `[Tue 2026-04-28 13:40:53 CST]`-style timestamp
    /// prefix per user message in the LLM context for temporal
    /// awareness (hermes `gateway.message_timestamps.enabled`,
    /// default OFF). Inbound text is ALWAYS stripped of stale
    /// prefixes so persisted transcripts stay clean regardless.
    #[serde(default)]
    pub message_timestamps: bool,
    /// Out-of-loop runtime liveness watchdog (hermes
    /// `gateway.loop_watchdog`, default on): a plain OS thread probes
    /// the async runtime every 30 s and hard-exits with the
    /// service-restart code after 3 consecutive missed probes so the
    /// supervisor can revive a frozen gateway.
    #[serde(default = "default_true")]
    pub loop_watchdog: bool,
    /// systemd watchdog interval in seconds (hermes
    /// `gateway.systemd_watchdog_seconds`, 0 = disabled): arms the
    /// sd_notify WATCHDOG=1 sampler + READY/STOPPING notifications
    /// for `Type=notify` service units. The feed cadence itself
    /// comes from systemd's `WATCHDOG_USEC` stamp.
    #[serde(default)]
    pub systemd_watchdog_seconds: u64,
    /// Cap on concurrently active chat sessions across all surfaces
    /// (hermes `max_concurrent_sessions`; 0/unset disables the cap).
    #[serde(default)]
    pub max_concurrent_sessions: Option<u32>,
    /// Seconds of progress silence before a messaging session with
    /// parked inbound follow-ups receives a one-shot "appears stalled"
    /// notice (hermes `HERMES_SESSION_STALL_TIMEOUT`, default 300;
    /// 0 disables the watchdog). Progress stamps come from the shared
    /// activity contract — turn boundaries, tool calls and streaming
    /// deltas — never from turn-start or inbound clocks.
    #[serde(default = "default_session_stall_timeout")]
    pub session_stall_timeout_secs: f64,
}

fn default_gateway_host() -> String {
    "127.0.0.1".to_string()
}

fn default_gateway_port() -> u16 {
    8642
}

fn default_session_stall_timeout() -> f64 {
    300.0
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            host: default_gateway_host(),
            port: default_gateway_port(),
            key: None,
            multiplex_profiles: false,
            profile_routes: Vec::new(),
            message_timestamps: false,
            loop_watchdog: true,
            systemd_watchdog_seconds: 0,
            max_concurrent_sessions: None,
            session_stall_timeout_secs: default_session_stall_timeout(),
        }
    }
}

impl GatewayConfig {
    /// Resolve effective settings with environment overrides.
    pub fn resolved(&self) -> Self {
        let mut config = self.clone();
        if let Ok(host) = std::env::var("ULNCLAW_GATEWAY_HOST") {
            if !host.trim().is_empty() {
                config.host = host.trim().to_string();
            }
        }
        if let Ok(port) = std::env::var("ULNCLAW_GATEWAY_PORT") {
            if let Ok(port) = port.trim().parse() {
                config.port = port;
            }
        }
        if let Ok(key) = std::env::var("ULNCLAW_GATEWAY_KEY") {
            if !key.trim().is_empty() {
                config.key = Some(key.trim().to_string());
            }
        }
        if let Ok(raw) = std::env::var("ULNCLAW_SESSION_STALL_TIMEOUT") {
            if let Ok(value) = raw.trim().parse::<f64>() {
                config.session_stall_timeout_secs = value;
            }
        }
        config
    }
}

/// Root config — `~/.ulnclaw/config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UlncLawConfig {
    #[serde(default)]
    pub model: ModelConfig,
    /// IANA timezone for prompt timestamps (hermes config.yaml
    /// `timezone`); `ULNCLAW_TIMEZONE`/`HERMES_TIMEZONE` env vars take
    /// priority, blank/unset falls back to server-local time.
    #[serde(default)]
    pub timezone: Option<String>,
    #[serde(default)]
    pub agent: AgentSettings,
    #[serde(default)]
    pub delegation: DelegationConfig,
    #[serde(default)]
    pub terminal: TerminalConfig,
    /// Transparent filesystem checkpoints (hermes checkpoint_manager).
    #[serde(default)]
    pub checkpoints: crate::checkpoint::CheckpointsConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub web: WebConfig,
    /// Browser tool endpoint + cloud browser providers (hermes `browser:`).
    #[serde(default)]
    pub browser: BrowserConfig,
    /// Enabled toolsets (empty = default "coding" set).
    #[serde(default)]
    pub enabled_toolsets: Vec<String>,
    /// Disabled toolsets.
    #[serde(default)]
    pub disabled_toolsets: Vec<String>,
    /// Named profiles (hermes profiles).
    #[serde(default)]
    pub profiles: HashMap<String, ProfileOverride>,
    /// MCP server connections.
    #[serde(default)]
    pub mcp: McpConfig,
    /// Discord tool settings (`[discord]`, hermes parity).
    #[serde(default)]
    pub discord: DiscordConfig,
    /// HTTP gateway (OpenAI-compatible API server).
    #[serde(default)]
    pub gateway: GatewayConfig,
    /// Approval flow settings (hermes `[approvals]`).
    #[serde(default)]
    pub approvals: ApprovalsConfig,
    /// Per-task auxiliary provider overrides (hermes `auxiliary.<task>`):
    /// `[auxiliary.compression]`, `[auxiliary.vision]`, ...
    #[serde(default)]
    pub auxiliary: HashMap<String, AuxiliaryTaskConfig>,
    /// Keyed user-defined providers (`[providers.<slug>]`, hermes v12+
    /// keyed `providers:` form) — surfaced by `/api/model/options`.
    #[serde(default)]
    pub providers: HashMap<String, CustomProviderConfig>,
    /// Picker catalog knobs (hermes `model_catalog:`).
    #[serde(default)]
    pub model_catalog: ModelCatalogConfig,
    /// Mixture-of-Agents presets (`[moa]`, hermes `moa:`).
    #[serde(default)]
    pub moa: MoaConfig,
    /// Tool-output truncation limits (hermes `tool_output:`).
    #[serde(default)]
    pub tool_output: ToolOutputConfig,
    /// URL/SSRF safety settings (hermes `security:`).
    #[serde(default)]
    pub security: SecurityConfig,
    /// Presentation toggles for GUI hosts (hermes `display:`).
    #[serde(default)]
    pub display: DisplayConfig,
    /// Dashboard theme/font persistence (hermes `dashboard:`).
    #[serde(default)]
    pub dashboard: DashboardConfig,
    /// X (Twitter) search via xAI (hermes `x_search:`).
    #[serde(default)]
    pub x_search: XSearchConfig,
    /// Video generation backend selection (hermes `video_gen:`).
    #[serde(default)]
    pub video_gen: VideoGenConfig,
    /// External secret sources (hermes `secrets.*`).
    #[serde(default)]
    pub secrets: crate::secrets::SecretsConfig,
    /// Computer-use / cua-driver settings (hermes `computer_use:`).
    #[serde(default)]
    pub computer_use: crate::computer_use::ComputerUseConfig,
    /// Plugin system settings (hermes `plugins` deny-list).
    #[serde(default)]
    pub plugins: crate::plugins::PluginsConfig,
    /// Shell-hook commands per lifecycle event (hermes `hooks:` block).
    #[serde(default)]
    pub hooks: crate::plugins::HooksConfig,
    /// Messaging platform gateways (hermes gateway/platforms).
    #[serde(default)]
    pub messaging: crate::messaging::MessagingConfig,
    /// Gateway monitoring / OTLP health export (hermes `monitoring:`).
    #[serde(default)]
    pub monitoring: crate::monitoring::MonitoringConfig,
    /// Gateway logging extras (hermes `logging.memory_monitor`).
    #[serde(default)]
    pub logging: LoggingConfig,
    /// Speech-to-text pipeline for voice messages (hermes `stt:`).
    #[serde(default)]
    pub stt: crate::stt::SttConfig,
    /// Text-to-speech for `/api/audio/speak` (hermes `tts:`; lean
    /// openai/elevenlabs providers — P344).
    #[serde(default)]
    pub tts: crate::tts::TtsConfig,
    /// OAuth device-flow login (hermes portal auth, service-agnostic).
    #[serde(default)]
    pub oauth: crate::oauth::OAuthConfig,
    /// Local OpenAI-compatible proxy to OAuth upstreams (hermes
    /// `hermes proxy`).
    #[serde(default)]
    pub proxy: crate::proxy_cmd::ProxyConfig,
    /// Skill sync across devices (hermes `hermes sync`).
    #[serde(default)]
    pub sync: crate::skills_sync::SyncConfig,
    /// Pet hatch image-generation endpoint overrides (hermes
    /// `agent/pet/generate` provider config).
    #[serde(default)]
    pub pets: PetsConfig,
    /// Kanban dispatcher settings (hermes `kanban:` block).
    #[serde(default)]
    pub kanban: KanbanConfig,
    /// Cron delivery + Chronos fire-webhook settings (hermes `cron:`
    /// block: `wrap_response`, `chronos.*`).
    #[serde(default)]
    pub cron: CronConfig,
}

/// `[logging]` — gateway logging extras (hermes `logging.*`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LoggingConfig {
    /// Periodic `[MEMORY] rss=...` gateway logging (hermes
    /// `logging.memory_monitor`, default on).
    pub memory_monitor: bool,
    /// Cadence for the memory monitor in seconds (hermes default 300).
    pub memory_monitor_interval_secs: u64,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            memory_monitor: true,
            memory_monitor_interval_secs: crate::memory_monitor::DEFAULT_INTERVAL_SECONDS,
        }
    }
}

/// `[cron]` — cron delivery settings (hermes `cron.*`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CronConfig {
    /// Wrap delivered cron output with a job-name header/footer so the
    /// user knows it is a cron delivery (hermes `cron.wrap_response`,
    /// default on).
    pub wrap_response: bool,
    /// Mirror successful cron deliveries into the origin chat's session
    /// transcript (hermes `cron.mirror_delivery`, default OFF): the
    /// delivered text lands as a labelled user turn so the next reply
    /// in that chat sees the cron output in context. Per-job
    /// `attach_to_session` overrides this global (hermes
    /// `_cron_mirror_delivery_enabled` precedence).
    #[serde(default)]
    pub mirror_delivery: bool,
    /// Chronos NAS fire-webhook verification (hermes `cron.chronos.*`).
    #[serde(default)]
    pub chronos: ChronosConfig,
}

impl Default for CronConfig {
    fn default() -> Self {
        Self {
            wrap_response: true,
            mirror_delivery: false,
            chronos: ChronosConfig::default(),
        }
    }
}

/// `[cron.chronos]` — NAS fire-token verification (hermes
/// `cron.chronos.*`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ChronosConfig {
    /// Expected `aud` claim (`agent:<instance_id>`).
    pub expected_audience: Option<String>,
    /// JWKS URL (or inline PEM public key) for NAS signatures.
    pub nas_jwks_url: Option<String>,
    /// Expected `iss` claim (the NAS portal URL); unset = not checked.
    pub portal_url: Option<String>,
}

/// `[kanban]` — dispatcher settings (hermes `kanban.*`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct KanbanConfig {
    /// Run dispatcher ticks inside the gateway (hermes
    /// `kanban.dispatch_in_gateway`, default on).
    pub dispatch_in_gateway: bool,
    /// Seconds between dispatch ticks (hermes default 60).
    pub dispatch_interval_secs: u64,
    /// Max concurrent kanban workers (live concurrency cap).
    pub max_spawn: usize,
    /// Spawn each worker in its own git worktree when dispatching from a
    /// repo (hermes worktree workspaces; falls back to in-place otherwise).
    pub worktrees: bool,
    /// Profile that owns the root/orchestration task after a decompose
    /// fan-out (hermes `kanban.orchestrator_profile`).
    pub orchestrator_profile: Option<String>,
    /// Profile that catches child tasks the decomposer cannot route
    /// (hermes `kanban.default_assignee`).
    pub default_assignee: Option<String>,
    /// Promote parent-free decomposed children to ready automatically
    /// (hermes `kanban.auto_promote_children`, default on).
    pub auto_promote_children: bool,
    /// Gateway dispatcher auto-decomposes triage-column tasks before
    /// fanning out workers (hermes `kanban.auto_decompose`, default on).
    /// Safety toggle: re-read every tick, so flipping it off stops a
    /// runaway fan-out on the next tick without a gateway restart.
    pub auto_decompose: bool,
    /// Max triage tasks auto-decomposed per dispatcher tick (hermes
    /// `kanban.auto_decompose_per_tick`, default 3).
    pub auto_decompose_per_tick: usize,
    /// Reclaim running tasks older than this many seconds when their
    /// heartbeat went stale > 1 h (hermes
    /// `kanban.dispatch_stale_timeout_seconds`, default 14400; 0
    /// disables the check).
    pub stale_timeout_seconds: i64,
    /// Per-profile concurrency cap: even with global headroom, refuse
    /// to spawn for an assignee already running this many tasks
    /// (hermes `kanban.max_in_progress_per_profile`, #21582). None or
    /// 0 disables the cap.
    pub max_in_progress_per_profile: Option<usize>,
    /// Global in-progress cap: when the board already has this many
    /// running tasks, the dispatcher skips the tick so slow workers
    /// can drain; otherwise only enough tasks spawn to reach the cap
    /// (hermes `kanban.max_in_progress`, #33488). None or 0 disables
    /// the cap (`max_spawn` still applies).
    pub max_in_progress: Option<usize>,
    /// Rotate a per-task worker log once it reaches this many bytes,
    /// keeping one `.log.1` backup generation (hermes
    /// `kanban.worker_log_rotate_bytes`; default 2 MiB).
    pub worker_log_rotate_bytes: Option<u64>,
}

impl Default for KanbanConfig {
    fn default() -> Self {
        Self {
            dispatch_in_gateway: true,
            dispatch_interval_secs: 60,
            max_spawn: 2,
            worktrees: true,
            orchestrator_profile: None,
            default_assignee: None,
            auto_promote_children: true,
            auto_decompose: true,
            auto_decompose_per_tick: 3,
            stale_timeout_seconds: 14400,
            max_in_progress_per_profile: None,
            max_in_progress: None,
            worker_log_rotate_bytes: None,
        }
    }
}

/// `[pets]` — image-generation endpoint overrides for the pet hatch
/// pipeline (hermes `agent/pet/generate/imagegen.py` provider settings).
/// All optional: key falls back to `OPENAI_API_KEY`/`ULNCLAW_API_KEY`,
/// base URL to `https://api.openai.com/v1`, model to `gpt-image-2`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PetsConfig {
    /// OpenAI-compatible base URL serving `/images/generations` and
    /// `/images/edits`.
    pub image_base_url: Option<String>,
    /// API key for the images endpoint.
    pub image_api_key: Option<String>,
    /// Image model id (hermes default `gpt-image-2`).
    pub image_model: Option<String>,
}

/// `[video_gen]` — video generation backend selection (hermes config.yaml
/// `video_gen:`). The provider names a registered `VideoGenProvider`;
/// blank auto-selects when exactly one available provider is registered.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VideoGenConfig {
    /// Active backend name (`xai`, `fal`, `deepinfra`, ...); blank
    /// auto-selects a single available provider.
    #[serde(default)]
    pub provider: Option<String>,
    /// Default model for the active backend; blank uses the provider's
    /// default.
    #[serde(default)]
    pub model: Option<String>,
    /// FAL-specific settings (hermes `video_gen.fal:`).
    #[serde(default)]
    pub fal: FalVideoGenConfig,
}

/// `[video_gen.fal]` — FAL backend tuning (hermes `video_gen.fal:`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FalVideoGenConfig {
    /// FAL model family id (`pixverse-v6`, `veo3.1`, ...); overrides the
    /// top-level `video_gen.model` when it names a known family.
    #[serde(default)]
    pub model: Option<String>,
}

/// `[x_search]` — xAI X-search tuning (port of hermes `x_search.*`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct XSearchConfig {
    /// Responses-API model used for the x_search server tool
    /// (hermes default `grok-4.5`).
    pub model: String,
    /// Optional reasoning effort: low | medium | high | xhigh.
    pub reasoning_effort: String,
    /// Request timeout in seconds (floor 30, hermes default 180).
    pub timeout_seconds: u64,
    /// Retries on 5xx / transient failures (hermes default 2).
    pub retries: u32,
}

impl Default for XSearchConfig {
    fn default() -> Self {
        Self {
            model: "grok-4.5".to_string(),
            reasoning_effort: String::new(),
            timeout_seconds: 180,
            retries: 2,
        }
    }
}

/// Bool-or-visibility-mode value for `long_running_notifications`
/// (hermes allows `true`/`false` or the `"generic"` visibility mode,
/// which swaps the elapsed-time heartbeat for generic status phrases).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum BoolOrMode {
    Flag(bool),
    Mode(String),
}

/// `[display]` — presentation toggles for GUI hosts (hermes `display.*`).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct DisplayConfig {
    /// Enable message reactions (tapbacks) in desktop GUIs — off by
    /// default, enabled from the desktop app's settings (hermes
    /// `display.message_reactions`).
    pub message_reactions: bool,
    /// Active theme name — one of the built-in skins (default, ares, mono,
    /// slate, daylight, warm-lightmode, poseidon, sisyphus, charizard);
    /// blank/unset = `default` (hermes `display.skin`).
    #[serde(default)]
    pub skin: Option<String>,
    /// Petdex mascot display settings (hermes `display.pet.*`).
    #[serde(default)]
    pub pet: PetDisplayConfig,
    /// Runtime-metadata footer appended to the final reply of an agent
    /// turn (hermes `display.runtime_footer`). Off by default to keep
    /// replies minimal; toggle live with `/footer on|off`.
    #[serde(default)]
    pub runtime_footer: crate::runtime_footer::RuntimeFooterConfig,
    /// Tool-progress bubble mode: `off`|`new`|`all`|`verbose`|`log`
    /// (hermes `display.tool_progress`). Unset = per-platform built-in
    /// default via `display_config`.
    #[serde(default)]
    pub tool_progress: Option<String>,
    /// Progress grouping: `accumulate`|`separate` (hermes
    /// `display.tool_progress_grouping`).
    #[serde(default)]
    pub tool_progress_grouping: Option<String>,
    /// Render reasoning/thinking summaries (hermes
    /// `display.show_reasoning`).
    #[serde(default)]
    pub show_reasoning: Option<bool>,
    /// Reasoning render style: `code`|`blockquote`|`subtext` (hermes
    /// `display.reasoning_style`).
    #[serde(default)]
    pub reasoning_style: Option<String>,
    /// Tool argument preview length, 0 = no limit (hermes
    /// `display.tool_preview_length`).
    #[serde(default)]
    pub tool_preview_length: Option<i64>,
    /// CLI terminal streaming toggle (hermes `display.streaming` —
    /// CLI-only; gateway streaming follows the top-level streaming
    /// config unless a per-platform override is set).
    #[serde(default)]
    pub streaming: Option<bool>,
    /// Surface real mid-turn assistant commentary (hermes
    /// `display.interim_assistant_messages`).
    #[serde(default)]
    pub interim_assistant_messages: Option<bool>,
    /// Periodic "still working" heartbeats on long turns (hermes
    /// `display.long_running_notifications`); `true`/`false` or the
    /// `"generic"` visibility mode (status-phrase rotation).
    #[serde(default)]
    pub long_running_notifications: Option<BoolOrMode>,
    /// Queue-depth/iteration detail in busy acknowledgments (hermes
    /// `display.busy_ack_detail`).
    #[serde(default)]
    pub busy_ack_detail: Option<bool>,
    /// Visible ack after a successful mid-turn steer (hermes
    /// `display.busy_steer_ack_enabled`).
    #[serde(default)]
    pub busy_steer_ack_enabled: Option<bool>,
    /// Delete progress bubbles after the final response lands (hermes
    /// `display.cleanup_progress`).
    #[serde(default)]
    pub cleanup_progress: Option<bool>,
    /// Live working-state status text: `full`|`verb`|`off` (hermes
    /// `display.live_status`).
    #[serde(default)]
    pub live_status: Option<String>,
    /// Generic status-phrase catalog section (hermes
    /// `display.status_phrases`): profile-relative YAML paths and/or
    /// inline phrase lists merged over the built-ins.
    #[serde(default)]
    pub status_phrases: Option<crate::status_phrases::StatusPhrasesSection>,
    /// Legacy alias of `status_phrases` (hermes
    /// `display.generic_status_phrases`).
    #[serde(default)]
    pub generic_status_phrases: Option<crate::status_phrases::StatusPhrasesSection>,
    /// Legacy per-platform tool-progress overrides (hermes
    /// `display.tool_progress_overrides`) — fallback when no
    /// `[display.platforms.<platform>]` entry exists.
    #[serde(default)]
    pub tool_progress_overrides: std::collections::HashMap<String, String>,
    /// Per-platform display overrides (hermes
    /// `display.platforms.<platform>`).
    #[serde(default)]
    pub platforms: std::collections::HashMap<String, PlatformDisplayOverride>,
}

/// `[display.platforms.<platform>]` — per-platform display overrides
/// (hermes `display.platforms.<key>`). Only keys present override the
/// global `[display]` values.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PlatformDisplayOverride {
    /// Partial runtime-footer override (hermes
    /// `display.platforms.<key>.runtime_footer`).
    #[serde(default)]
    pub runtime_footer: crate::runtime_footer::RuntimeFooterOverride,
    /// Per-platform tool-progress mode (hermes
    /// `display.platforms.<key>.tool_progress`).
    #[serde(default)]
    pub tool_progress: Option<String>,
    /// Per-platform progress grouping (hermes
    /// `display.platforms.<key>.tool_progress_grouping`).
    #[serde(default)]
    pub tool_progress_grouping: Option<String>,
    /// Per-platform reasoning visibility (hermes
    /// `display.platforms.<key>.show_reasoning`).
    #[serde(default)]
    pub show_reasoning: Option<bool>,
    /// Per-platform reasoning style (hermes
    /// `display.platforms.<key>.reasoning_style`).
    #[serde(default)]
    pub reasoning_style: Option<String>,
    /// Per-platform tool preview length (hermes
    /// `display.platforms.<key>.tool_preview_length`).
    #[serde(default)]
    pub tool_preview_length: Option<i64>,
    /// Per-platform gateway streaming override (hermes
    /// `display.platforms.<key>.streaming`).
    #[serde(default)]
    pub streaming: Option<bool>,
    /// Per-platform interim commentary (hermes
    /// `display.platforms.<key>.interim_assistant_messages`).
    #[serde(default)]
    pub interim_assistant_messages: Option<bool>,
    /// Per-platform long-turn heartbeats (hermes
    /// `display.platforms.<key>.long_running_notifications`);
    /// `true`/`false` or `"generic"`.
    #[serde(default)]
    pub long_running_notifications: Option<BoolOrMode>,
    /// Per-platform busy-ack detail (hermes
    /// `display.platforms.<key>.busy_ack_detail`).
    #[serde(default)]
    pub busy_ack_detail: Option<bool>,
    /// Per-platform steer-ack toggle (hermes
    /// `display.platforms.<key>.busy_steer_ack_enabled`).
    #[serde(default)]
    pub busy_steer_ack_enabled: Option<bool>,
    /// Per-platform progress cleanup (hermes
    /// `display.platforms.<key>.cleanup_progress`).
    #[serde(default)]
    pub cleanup_progress: Option<bool>,
    /// Per-platform live status text mode (hermes
    /// `display.platforms.<key>.live_status`).
    #[serde(default)]
    pub live_status: Option<String>,
    /// Per-platform status-phrase catalog section (hermes
    /// `display.platforms.<key>.status_phrases`).
    #[serde(default)]
    pub status_phrases: Option<crate::status_phrases::StatusPhrasesSection>,
    /// Per-platform legacy phrase alias (hermes
    /// `display.platforms.<key>.generic_status_phrases`).
    #[serde(default)]
    pub generic_status_phrases: Option<crate::status_phrases::StatusPhrasesSection>,
}

/// `[dashboard]` — web/desktop dashboard appearance persistence (hermes
/// `dashboard:` theme/font keys). The frontend owns the actual theme
/// definitions; the backend only stores the active selection.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DashboardConfig {
    /// Active dashboard theme name; blank/unset = `default`.
    pub theme: String,
    /// Font override id; `theme` = use the theme's own font.
    pub font: String,
}

impl Default for DashboardConfig {
    fn default() -> Self {
        DashboardConfig {
            theme: "default".to_string(),
            font: "theme".to_string(),
        }
    }
}

/// `[display.pet]` — petdex mascot display (hermes `display.pet.*`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PetDisplayConfig {
    /// Whether the pet display is on (`display.pet.enabled`).
    pub enabled: bool,
    /// Active pet slug under `<home>/pets/` (`display.pet.slug`).
    #[serde(default)]
    pub slug: Option<String>,
    /// Master scale 0.1–3.0 shared by every surface (`display.pet.scale`).
    #[serde(default)]
    pub scale: Option<f64>,
    /// Render mode override: auto/kitty/iterm/sixel/unicode/off
    /// (`display.pet.render_mode`).
    #[serde(default)]
    pub render_mode: Option<String>,
    /// Explicit half-block column width override (`display.pet.unicode_cols`).
    #[serde(default)]
    pub unicode_cols: Option<u32>,
}

impl Default for PetDisplayConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            slug: None,
            scale: None,
            render_mode: None,
            unicode_cols: None,
        }
    }
}

/// `[security]` — URL safety + tirith scanner (port of hermes `security.*`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SecurityConfig {
    /// Allow web tools to fetch private/internal addresses (disables the
    /// SSRF block). Cloud metadata endpoints remain blocked regardless
    /// (hermes `security.allow_private_urls`).
    pub allow_private_urls: bool,
    /// Enable the tirith pre-exec content scanner (hermes
    /// `security.tirith_enabled`; env `TIRITH_ENABLED` overrides).
    #[serde(default = "default_tirith_enabled")]
    pub tirith_enabled: bool,
    /// tirith binary: bare name (PATH lookup + auto-install) or explicit
    /// path (hermes `security.tirith_path`; env `TIRITH_BIN` overrides).
    #[serde(default = "default_tirith_path")]
    pub tirith_path: String,
    /// Scan timeout in seconds (hermes `security.tirith_timeout`; env
    /// `TIRITH_TIMEOUT` overrides).
    #[serde(default = "default_tirith_timeout")]
    pub tirith_timeout: u64,
    /// Allow commands when tirith cannot run (hermes
    /// `security.tirith_fail_open`; env `TIRITH_FAIL_OPEN` overrides).
    #[serde(default = "default_tirith_fail_open")]
    pub tirith_fail_open: bool,
}

fn default_tirith_enabled() -> bool {
    true
}
fn default_tirith_path() -> String {
    "tirith".to_string()
}
fn default_tirith_timeout() -> u64 {
    5
}
fn default_tirith_fail_open() -> bool {
    true
}

impl Default for SecurityConfig {
    fn default() -> Self {
        SecurityConfig {
            allow_private_urls: false,
            tirith_enabled: default_tirith_enabled(),
            tirith_path: default_tirith_path(),
            tirith_timeout: default_tirith_timeout(),
            tirith_fail_open: default_tirith_fail_open(),
        }
    }
}

/// `[tool_output]` — configurable tool-output truncation limits (port of
/// hermes `tools/tool_output_limits.py`). Defaults preserve the existing
/// ulnclaw behaviour; invalid or non-positive values fall back to the
/// defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolOutputConfig {
    /// Terminal stdout/stderr character cap (head+tail kept).
    #[serde(default = "default_tool_output_max_bytes")]
    pub max_bytes: usize,
    /// read_file pagination/truncation line cap.
    #[serde(default = "default_tool_output_max_lines")]
    pub max_lines: usize,
    /// Per-line length cap before '... [truncated]'.
    #[serde(default = "default_tool_output_max_line_length")]
    pub max_line_length: usize,
}

fn default_tool_output_max_bytes() -> usize {
    100_000
}
fn default_tool_output_max_lines() -> usize {
    2000
}
fn default_tool_output_max_line_length() -> usize {
    2000
}

fn positive_or(value: usize, default: usize) -> usize {
    if value == 0 {
        default
    } else {
        value
    }
}

impl ToolOutputConfig {
    /// Resolved limits with non-positive values coerced to defaults
    /// (hermes `_coerce_positive_int` semantics).
    pub fn resolved(&self) -> ToolOutputConfig {
        ToolOutputConfig {
            max_bytes: positive_or(self.max_bytes, default_tool_output_max_bytes()),
            max_lines: positive_or(self.max_lines, default_tool_output_max_lines()),
            max_line_length: positive_or(
                self.max_line_length,
                default_tool_output_max_line_length(),
            ),
        }
    }
}

impl Default for ToolOutputConfig {
    fn default() -> Self {
        Self {
            max_bytes: default_tool_output_max_bytes(),
            max_lines: default_tool_output_max_lines(),
            max_line_length: default_tool_output_max_line_length(),
        }
    }
}

/// `[approvals]` config section.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ApprovalsConfig {
    /// Seconds to wait for a human decision before fail-closed auto-deny
    /// (hermes default 300).
    pub timeout: u64,
    /// `manual` (prompt a human), `smart` (auxiliary-LLM guardian first,
    /// escalate to a human when unsure) or `off` (auto-approve everything
    /// except the hardline floor) — hermes `approvals.mode`.
    pub mode: String,
    /// `deny` (default, fail-closed) or `approve` — what happens when a
    /// cron-triggered run hits the approval gate with no human present
    /// (hermes `approvals.cron_mode`).
    pub cron_mode: String,
    /// Operator rules appended to the smart-approval guardian's system
    /// prompt (hermes `approvals.smart_policy`).
    pub smart_policy: String,
    /// Consecutive-denial circuit breaker for smart approvals: after this
    /// many guardian DENY verdicts in a row the deny message escalates to
    /// a hard-stop instruction. Any approval resets the count; 0 disables
    /// (hermes `approvals.denial_breaker_threshold`, default 3).
    #[serde(default = "default_denial_breaker_threshold")]
    pub denial_breaker_threshold: usize,
    /// User-defined deny rules: fnmatch globs matched against terminal
    /// commands. A match blocks the command unconditionally — BEFORE the
    /// mode=off bypass (hermes `approvals.deny`).
    #[serde(default)]
    pub deny: Vec<String>,
    /// Require confirmation before `/reload-mcp` — reloading rebuilds the
    /// tool surface and invalidates the provider prompt cache. "Always
    /// Approve" persists `false` here (hermes
    /// `approvals.mcp_reload_confirm`, default true).
    #[serde(default = "default_mcp_reload_confirm")]
    pub mcp_reload_confirm: bool,
}

fn default_denial_breaker_threshold() -> usize {
    3
}

fn default_mcp_reload_confirm() -> bool {
    true
}

impl Default for ApprovalsConfig {
    fn default() -> Self {
        Self {
            timeout: 300,
            mode: "manual".to_string(),
            cron_mode: "deny".to_string(),
            smart_policy: String::new(),
            denial_breaker_threshold: default_denial_breaker_threshold(),
            deny: Vec::new(),
            mcp_reload_confirm: default_mcp_reload_confirm(),
        }
    }
}

/// MCP configuration section ([[mcp.servers]]).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: Vec<crate::mcp::McpServerConfig>,
}

/// A config value accepted as either a scalar string or a list (hermes
/// config.yaml accepts both for e.g. `discord.server_actions`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StringOrList {
    Single(String),
    List(Vec<String>),
}

/// Discord tool settings (hermes config.yaml `[discord]`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DiscordConfig {
    /// Action allowlist — comma-separated string or list of action names.
    /// Unset/empty exposes every intent-available action (hermes
    /// `discord.server_actions`).
    #[serde(default)]
    pub server_actions: Option<StringOrList>,
}

/// A named profile override section.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProfileOverride {
    #[serde(default)]
    pub model: Option<ModelConfig>,
    #[serde(default)]
    pub enabled_toolsets: Option<Vec<String>>,
    #[serde(default)]
    pub disabled_toolsets: Option<Vec<String>>,
}

/// Default base URL for a provider name (environment overrides aware).
/// Shared by the main runtime and auxiliary task resolution.
pub fn default_base_url(provider: &str) -> String {
    match provider {
        "openai" => get_env_value("OPENAI_BASE_URL")
            .unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
        "anthropic" => "https://api.anthropic.com".to_string(),
        "ollama" => get_env_value("OLLAMA_BASE_URL")
            .unwrap_or_else(|| "http://localhost:11434/v1".to_string()),
        "dashscope" => "https://dashscope.aliyuncs.com/compatible-mode/v1".to_string(),
        _ => get_env_value("OPENAI_BASE_URL")
            .unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
    }
}

impl UlncLawConfig {
    /// Load config from the given path (or default location when `None`).
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let path = path
            .map(PathBuf::from)
            .unwrap_or_else(|| ulnclaw_home().join("config.toml"));
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(&path)
            .map_err(|e| AgentError::config(format!("read {}: {}", path.display(), e)))?;
        let config: UlncLawConfig = toml::from_str(&content)
            .map_err(|e| AgentError::config(format!("parse {}: {}", path.display(), e)))?;
        Ok(config)
    }

    /// Apply a named profile on top of this config.
    pub fn with_profile(mut self, name: &str) -> Self {
        if let Some(profile) = self.profiles.get(name).cloned() {
            if let Some(model) = profile.model {
                self.model = model;
            }
            if let Some(ts) = profile.enabled_toolsets {
                self.enabled_toolsets = ts;
            }
            if let Some(ts) = profile.disabled_toolsets {
                self.disabled_toolsets = ts;
            }
        }
        self
    }

    /// Resolve the API key: config > credential pool (rotating) >
    /// ULNCLAW_API_KEY > OPENAI_API_KEY > ANTHROPIC_API_KEY (.env-aware).
    /// Pool membership is the curation signal (hermes credential-pool
    /// semantics, lean port — `credential_pool.rs`).
    pub fn resolve_api_key(&self) -> Option<String> {
        if let Some(ref key) = self.model.api_key {
            if !key.is_empty() {
                return Some(key.clone());
            }
        }
        let pooled =
            crate::credential_pool::resolve_pooled_key(&ulnclaw_home(), &self.model.provider);
        if let Some(key) = pooled {
            return Some(key);
        }
        get_env_value("ULNCLAW_API_KEY")
            .or_else(|| get_env_value("OPENAI_API_KEY"))
            .or_else(|| get_env_value("ANTHROPIC_API_KEY"))
    }

    /// Resolve the base URL for the configured provider.
    pub fn resolve_base_url(&self) -> String {
        if let Some(ref url) = self.model.base_url {
            if !url.is_empty() {
                return url.clone();
            }
        }
        default_base_url(&self.model.provider)
    }

    /// Write a default config file if none exists.
    pub fn write_default_if_missing() -> Result<PathBuf> {
        let home = ensure_home()?;
        let path = home.join("config.toml");
        if !path.exists() {
            let default = UlncLawConfig::default();
            let content = toml::to_string_pretty(&default)
                .map_err(|e| AgentError::config(format!("serialize config: {}", e)))?;
            let header = "# ulnclaw configuration (ported from hermes-agent config.yaml)\n\
                          # See docs/en/configuration.md for all options.\n\n";
            std::fs::write(&path, format!("{}{}", header, content))
                .map_err(|e| AgentError::config(format!("write {}: {}", path.display(), e)))?;
        }
        Ok(path)
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn test_tool_output_limits_coercion() {
        let cfg = ToolOutputConfig {
            max_bytes: 0,
            max_lines: 0,
            max_line_length: 0,
        };
        let r = cfg.resolved();
        assert_eq!(r.max_bytes, 100_000);
        assert_eq!(r.max_lines, 2000);
        assert_eq!(r.max_line_length, 2000);
        let cfg = ToolOutputConfig {
            max_bytes: 500,
            max_lines: 10,
            max_line_length: 80,
        };
        let r = cfg.resolved();
        assert_eq!((r.max_bytes, r.max_lines, r.max_line_length), (500, 10, 80));
    }
    use super::*;

    #[test]
    fn test_personality_defs_parse_and_resolve() {
        // P619: bare-string and table personalities (hermes
        // agent.personalities), resolution + preview semantics.
        let raw = r#"
[agent.personalities]
pirate = "Talk like a grizzled pirate. Sprinkle nautical slang."

[agent.personalities.poet]
system_prompt = "Answer in verse."
tone = "lyrical"
style = "haiku-first"
description = "Speaks in poems"
"#;
        let config: UlncLawConfig = toml::from_str(raw).unwrap();
        assert_eq!(config.agent.personalities.len(), 2);

        let pirate = &config.agent.personalities["pirate"];
        assert_eq!(
            pirate.resolve_prompt(),
            "Talk like a grizzled pirate. Sprinkle nautical slang."
        );
        // Previews clip to 50 chars (hermes list semantics).
        assert_eq!(
            pirate.preview(),
            "Talk like a grizzled pirate. Sprinkle nautical sla"
        );

        let poet = &config.agent.personalities["poet"];
        assert_eq!(
            poet.resolve_prompt(),
            "Answer in verse.\nTone: lyrical\nStyle: haiku-first"
        );
        assert_eq!(poet.preview(), "Speaks in poems");
    }

    #[test]
    fn test_default_config_roundtrip() {
        let config = UlncLawConfig::default();
        let s = toml::to_string(&config).unwrap();
        let back: UlncLawConfig = toml::from_str(&s).unwrap();
        assert_eq!(back.agent.max_iterations, 90);
        assert_eq!(back.memory.memory_char_limit, 2200);
        assert_eq!(back.delegation.max_concurrent_children, 3);
    }

    #[test]
    fn test_env_file_parsing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        std::fs::write(&path, "# comment\nFOO=bar\nexport BAZ=\"qux\"\n\nEMPTY=\n").unwrap();
        let map = load_env_file(&path);
        assert_eq!(map.get("FOO").unwrap(), "bar");
        assert_eq!(map.get("BAZ").unwrap(), "qux");
    }

    #[test]
    fn test_profile_override() {
        let mut config = UlncLawConfig::default();
        let mut profile = ProfileOverride::default();
        let mut model = ModelConfig::default();
        model.model = "test-model".into();
        profile.model = Some(model);
        config.profiles.insert("work".into(), profile);
        let config = config.with_profile("work");
        assert_eq!(config.model.model, "test-model");
    }

    #[test]
    fn kanban_auto_decompose_defaults_and_overrides() {
        let cfg = crate::config::UlncLawConfig::default();
        assert!(cfg.kanban.auto_decompose);
        assert_eq!(cfg.kanban.auto_decompose_per_tick, 3);

        let parsed: crate::config::UlncLawConfig =
            toml::from_str("[kanban]\nauto_decompose = false\nauto_decompose_per_tick = 1\n")
                .unwrap();
        assert!(!parsed.kanban.auto_decompose);
        assert_eq!(parsed.kanban.auto_decompose_per_tick, 1);
    }

    #[test]
    fn reload_env_applies_and_removes_dotenv_keys() {
        // P689: hermes reload_env parity.
        let _guard = crate::models_dev::test_env_lock();
        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var("ULNCLAW_HOME").ok();
        std::env::set_var("ULNCLAW_HOME", dir.path());
        std::env::remove_var("ULNCLAW_RELOAD_A");
        std::env::remove_var("ULNCLAW_RELOAD_B");

        std::fs::write(dir.path().join(".env"), "ULNCLAW_RELOAD_A=1\n").unwrap();
        let count = super::reload_env();
        assert!(count >= 1, "{count}");
        assert_eq!(std::env::var("ULNCLAW_RELOAD_A").ok().as_deref(), Some("1"));

        // Update + add.
        std::fs::write(
            dir.path().join(".env"),
            "ULNCLAW_RELOAD_A=2\nULNCLAW_RELOAD_B=x\n",
        )
        .unwrap();
        super::reload_env();
        assert_eq!(std::env::var("ULNCLAW_RELOAD_A").ok().as_deref(), Some("2"));
        assert_eq!(std::env::var("ULNCLAW_RELOAD_B").ok().as_deref(), Some("x"));

        // Removal: B disappears from the file -> removed from the env.
        std::fs::write(dir.path().join(".env"), "ULNCLAW_RELOAD_A=2\n").unwrap();
        super::reload_env();
        assert_eq!(std::env::var("ULNCLAW_RELOAD_A").ok().as_deref(), Some("2"));
        assert!(std::env::var("ULNCLAW_RELOAD_B").is_err());

        std::env::remove_var("ULNCLAW_RELOAD_A");
        if let Some(home) = prev {
            std::env::set_var("ULNCLAW_HOME", home);
        }
    }
}

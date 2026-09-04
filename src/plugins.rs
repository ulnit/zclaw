//! Plugin system — Rust-native port of hermes' plugin architecture.
//!
//! Hermes loads Python plugins (`hermes_cli/plugins.py`) and bridges
//! shell scripts (`agent/shell_hooks.py`) through one `invoke_hook()`
//! aggregator. A static Rust binary can't import Python, so ulnclaw uses
//! the shell-hook wire protocol for everything:
//!
//! * **Directory plugins**: `<home>/plugins/<name>/plugin.toml` manifest
//!   declaring `hooks` (scripts at `hooks/<event>`) and `[[tools]]`
//!   (scripts invoked with `{"tool", "arguments"}` on stdin, registered
//!   as `plugin__<plugin>__<tool>`).
//! * **Config shell hooks**: `[hooks]` block in config.toml maps event
//!   names to command lines (shlex-split, no shell — hermes policy).
//!
//! Wire protocol (hermes shell_hooks): stdin JSON
//! `{"hook_event_name", "tool_name", "tool_input", "session_id", "cwd",
//! "extra"}`; stdout JSON `{"decision"|"action": "block", ...}` to block
//! a `pre_tool_call`, `{"text": ...}` to transform output, anything else
//! is an observer no-op. First-use consent for config hooks lives in
//! `<home>/shell-hooks-allowlist.json` (hermes `shell-hooks-allowlist`),
//! bypassed by `[hooks] auto_accept = true` or `ULNCLAW_ACCEPT_HOOKS=1`.

use crate::tools::{tool, ToolAvailability, ToolRegistry};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::OnceCell;

/// Hook names the agent core fires (hermes `VALID_HOOKS`).
pub const VALID_HOOKS: &[&str] = &[
    "pre_tool_call",
    "post_tool_call",
    "transform_terminal_output",
    "transform_tool_result",
    "transform_llm_output",
    "pre_llm_call",
    "post_llm_call",
    "pre_verify",
    "pre_api_request",
    "post_api_request",
    "api_request_error",
    "on_session_start",
    "on_session_end",
    "on_session_finalize",
    "on_session_reset",
    "subagent_start",
    "subagent_stop",
    "pre_gateway_dispatch",
    "pre_approval_request",
    "post_approval_response",
    "kanban_task_claimed",
    "kanban_task_completed",
    "kanban_task_blocked",
];

/// Per-hook subprocess wall-clock cap.
const HOOK_TIMEOUT: Duration = Duration::from_secs(10);

/// `[plugins]` config block.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PluginsConfig {
    /// Plugins never loaded (hermes deny-list semantics).
    #[serde(default)]
    pub disabled: Vec<String>,
    /// Selected memory provider (hermes plugin-providers): `builtin`
    /// (the file MEMORY.md/USER.md pair) or the name of an installed
    /// plugin whose manifest declares `provides = ["memory"]`.
    #[serde(default = "default_provider_selection")]
    pub memory_provider: String,
    /// Selected context engine; same semantics as `memory_provider`.
    #[serde(default = "default_provider_selection")]
    pub context_engine: String,
}

fn default_provider_selection() -> String {
    "builtin".to_string()
}

/// `[hooks]` config block: event → command lines (plus `auto_accept`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HooksConfig {
    /// Accept new hook commands without the first-use consent prompt.
    #[serde(default)]
    pub auto_accept: bool,
    /// Oversized hook-context spill settings (hermes
    /// `hooks.output_spill`); the named field takes precedence over the
    /// flattened event map.
    #[serde(
        default,
        skip_serializing_if = "crate::hook_output_spill::SpillConfig::is_default"
    )]
    pub output_spill: crate::hook_output_spill::SpillConfig,
    /// Event name → command lines (every other key).
    #[serde(flatten)]
    pub events: HashMap<String, Vec<String>>,
}

/// One `[[tools]]` entry in a plugin.toml manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginToolSpec {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Executable, relative to the plugin directory (or absolute).
    pub command: String,
    /// Optional JSON schema for the arguments (inline TOML table).
    #[serde(default)]
    pub parameters: Option<toml::Value>,
}

/// Parsed `plugin.toml` manifest (hermes `plugin.yaml` PluginManifest).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub version: String,
    /// Hook events this plugin handles via `hooks/<event>` scripts.
    #[serde(default)]
    pub hooks: Vec<String>,
    #[serde(default)]
    pub tools: Vec<PluginToolSpec>,
    /// Capability providers this plugin contributes (hermes plugin
    /// providers: `memory`, `context`). Surfaced by the plugin hub so
    /// `PUT /api/dashboard/plugin-providers` can select them.
    #[serde(default)]
    pub provides: Vec<String>,
}

/// A discovered plugin plus its directory.
#[derive(Debug, Clone)]
pub struct LoadedPlugin {
    pub manifest: PluginManifest,
    pub dir: PathBuf,
    /// On the config deny-list: listed by `plugins list` but contributes
    /// no hooks or tools.
    pub disabled: bool,
}

/// One registered hook callback.
#[derive(Debug, Clone)]
struct HookCallback {
    event: String,
    /// Absolute script path (directory plugin) or command line (config).
    command: String,
}

struct PluginRuntime {
    plugins: Vec<LoadedPlugin>,
    hooks: Vec<HookCallback>,
    warnings: Vec<String>,
}

static RUNTIME: OnceCell<PluginRuntime> = OnceCell::const_new();

/// Initialize the plugin runtime (idempotent). Returns discovery warnings.
pub async fn init(home: &Path, config: &crate::config::UlncLawConfig) -> Vec<String> {
    let runtime = RUNTIME
        .get_or_init(|| async { build_runtime(home, config).await })
        .await;
    runtime.warnings.clone()
}

fn plugins_dir(home: &Path) -> PathBuf {
    home.join("plugins")
}

/// `<home>/plugin-hub` — the curated marketplace index directory
/// (ulnclaw extension; hermes serves a vendor hub). `index.json` holds
/// `{"plugins": [{name, identifier, description, version, homepage,
/// tags}]}` entries; each subdirectory holding a `plugin.toml` is a
/// locally installable candidate.
pub fn hub_dir(home: &Path) -> PathBuf {
    home.join("plugin-hub")
}

/// Disk scan of `<home>/plugins` (hermes user-source discovery),
/// shared by the boot-time runtime and the dashboard plugin endpoints
/// (`GET /api/dashboard/plugins*`), which always reflect live disk
/// state. Directories without a parseable manifest become warnings.
pub fn discover_dir_plugins(home: &Path, disabled: &[String]) -> (Vec<LoadedPlugin>, Vec<String>) {
    let mut plugins = Vec::new();
    let mut warnings = Vec::new();
    let dir = plugins_dir(home);
    if dir.is_dir() {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok().map(|e| e.path()))
                    .filter(|p| p.is_dir())
                    .collect()
            })
            .unwrap_or_default();
        entries.sort();
        for entry in entries {
            let manifest_path = entry.join("plugin.toml");
            if !manifest_path.is_file() {
                warnings.push(format!(
                    "plugins: {} skipped (no plugin.toml)",
                    entry.display()
                ));
                continue;
            }
            match std::fs::read_to_string(&manifest_path)
                .map_err(|e| e.to_string())
                .and_then(|text| toml::from_str::<PluginManifest>(&text).map_err(|e| e.to_string()))
            {
                Ok(manifest) => {
                    if manifest.name.trim().is_empty() {
                        warnings.push(format!(
                            "plugins: {} skipped (manifest has no name)",
                            entry.display()
                        ));
                        continue;
                    }
                    let is_disabled = disabled.iter().any(|d| d == &manifest.name);
                    plugins.push(LoadedPlugin {
                        manifest,
                        dir: entry,
                        disabled: is_disabled,
                    });
                }
                Err(e) => {
                    warnings.push(format!("plugins: {} manifest error: {e}", entry.display()));
                }
            }
        }
    }
    (plugins, warnings)
}

fn allowlist_path(home: &Path) -> PathBuf {
    home.join("shell-hooks-allowlist.json")
}

fn load_allowlist(home: &Path) -> Vec<String> {
    std::fs::read_to_string(allowlist_path(home))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|v| v.get("accepted").and_then(|a| a.as_array()).cloned())
        .map(|arr| {
            arr.into_iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn save_allowlist(home: &Path, accepted: &[String]) {
    let value = json!({"accepted": accepted});
    if let Ok(text) = serde_json::to_string_pretty(&value) {
        std::fs::write(allowlist_path(home), text).ok();
    }
}

/// Shlex-style split (hermes `shlex.split`, shell=False): whitespace
/// separates tokens; single/double quotes group; backslash escapes.
pub fn split_command(command: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut has_token = false;
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && !in_single {
            if let Some(&next) = chars.peek() {
                current.push(next);
                chars.next();
                has_token = true;
            }
            continue;
        }
        if c == '\'' && !in_double {
            in_single = !in_single;
            has_token = true;
            continue;
        }
        if c == '"' && !in_single {
            in_double = !in_double;
            has_token = true;
            continue;
        }
        if c.is_whitespace() && !in_single && !in_double {
            if has_token {
                out.push(std::mem::take(&mut current));
                has_token = false;
            }
            continue;
        }
        current.push(c);
        has_token = true;
    }
    if has_token {
        out.push(current);
    }
    out
}

async fn build_runtime(home: &Path, config: &crate::config::UlncLawConfig) -> PluginRuntime {
    let mut warnings = Vec::new();
    let mut hooks: Vec<HookCallback> = Vec::new();

    // 1. Directory plugins (hermes user-source discovery).
    let (plugins, mut dir_warnings) = discover_dir_plugins(home, &config.plugins.disabled);
    warnings.append(&mut dir_warnings);
    for plugin in &plugins {
        if plugin.disabled {
            continue;
        }
        for hook in &plugin.manifest.hooks {
            if !VALID_HOOKS.contains(&hook.as_str()) {
                warnings.push(format!(
                    "plugins: {} declares unknown hook {hook:?}",
                    plugin.manifest.name
                ));
                continue;
            }
            let script = plugin.dir.join("hooks").join(hook);
            if script.is_file() {
                hooks.push(HookCallback {
                    event: hook.clone(),
                    command: script.display().to_string(),
                });
            } else {
                warnings.push(format!(
                    "plugins: {} hook {hook:?} has no hooks/{hook} script",
                    plugin.manifest.name
                ));
            }
        }
    }

    // 2. Config shell hooks with first-use consent (hermes shell_hooks).
    let auto_accept = config.hooks.auto_accept
        || std::env::var("ULNCLAW_ACCEPT_HOOKS")
            .map(|v| {
                matches!(
                    v.trim().to_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(false);
    let mut allowlist = load_allowlist(home);
    let mut allowlist_changed = false;
    let mut events: Vec<(&String, &Vec<String>)> = config.hooks.events.iter().collect();
    events.sort_by(|a, b| a.0.cmp(b.0));
    for (event, commands) in events {
        if !VALID_HOOKS.contains(&event.as_str()) {
            warnings.push(format!(
                "hooks: unknown event {event:?} in config (ignored)"
            ));
            continue;
        }
        for command in commands {
            let key = format!("{event}\t{command}");
            if allowlist.iter().any(|a| a == &key) {
                hooks.push(HookCallback {
                    event: event.clone(),
                    command: command.clone(),
                });
            } else if auto_accept {
                allowlist.push(key);
                allowlist_changed = true;
                hooks.push(HookCallback {
                    event: event.clone(),
                    command: command.clone(),
                });
            } else {
                warnings.push(format!(
                    "hooks: {event:?} -> {command:?} not consented yet — run \
                     `ulnclaw plugins accept-hooks` or set [hooks] auto_accept = true"
                ));
            }
        }
    }
    if allowlist_changed {
        save_allowlist(home, &allowlist);
    }

    PluginRuntime {
        plugins,
        hooks,
        warnings,
    }
}

/// Discovered plugins (empty before `init`).
pub fn loaded_plugins() -> Vec<LoadedPlugin> {
    RUNTIME.get().map(|r| r.plugins.clone()).unwrap_or_default()
}

/// Registered hook callbacks for one event, plugins first (hermes:
/// plugin block decisions win ties over shell hooks).
fn callbacks_for(event: &str) -> Vec<HookCallback> {
    RUNTIME
        .get()
        .map(|r| {
            r.hooks
                .iter()
                .filter(|h| h.event == event)
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// Whether any callback is registered for `event` (hermes `has_hook`) —
/// callers skip building potentially expensive payloads when nothing fires.
pub fn has_hook(event: &str) -> bool {
    !callbacks_for(event).is_empty()
}

/// Run one hook script: JSON payload on stdin, JSON response on stdout.
/// Never fatal — timeouts/parse errors yield no response (hermes policy).
async fn run_hook_callback(callback: &HookCallback, payload: &Value) -> Option<Value> {
    let argv = split_command(&callback.command);
    if argv.is_empty() {
        return None;
    }
    let mut cmd = tokio::process::Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        // Timeout drops the wait future; kill_on_drop reaps the script.
        .kill_on_drop(true);
    let mut child = cmd.spawn().ok()?;
    if let Some(mut stdin) = child.stdin.take() {
        let body = serde_json::to_string(payload).unwrap_or_default();
        if stdin.write_all(body.as_bytes()).await.is_err() {
            child.kill().await.ok();
            return None;
        }
    }
    let output = tokio::time::timeout(HOOK_TIMEOUT, child.wait_with_output()).await;
    let output = match output {
        Ok(Ok(output)) if output.status.success() => output,
        _ => return None,
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    serde_json::from_str::<Value>(trimmed).ok()
}

/// Fire one hook across every registered callback; returns all responses
/// (hermes `invoke_hook` — aggregation is the caller's job).
pub async fn invoke_hook(event: &str, payload: Value) -> Vec<Value> {
    let callbacks = callbacks_for(event);
    if callbacks.is_empty() {
        return Vec::new();
    }
    let mut responses = Vec::new();
    for callback in callbacks {
        if let Some(response) = run_hook_callback(&callback, &payload).await {
            responses.push(response);
        }
    }
    responses
}

/// Build the hermes hook payload envelope.
pub fn hook_payload(
    event: &str,
    session_id: &str,
    cwd: &Path,
    fields: Vec<(&str, Value)>,
    extra: Value,
) -> Value {
    let mut payload = json!({
        "hook_event_name": event,
        "session_id": session_id,
        "cwd": cwd.display().to_string(),
    });
    for (key, value) in fields {
        payload[key] = value;
    }
    payload["extra"] = extra;
    payload
}

/// pre_tool_call aggregation: first block wins (hermes semantics —
/// both Claude-Code and hermes shapes accepted). Returns the reason.
pub fn block_decision(responses: &[Value]) -> Option<String> {
    for response in responses {
        let decision = response
            .get("decision")
            .or_else(|| response.get("action"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if decision.eq_ignore_ascii_case("block") {
            let reason = response
                .get("reason")
                .or_else(|| response.get("message"))
                .and_then(|v| v.as_str())
                .unwrap_or("blocked by plugin hook");
            return Some(reason.to_string());
        }
    }
    None
}

/// transform_* aggregation: first non-empty text wins (hermes).
pub fn transform_text(responses: &[Value]) -> Option<String> {
    for response in responses {
        let candidate = match response {
            Value::String(s) => Some(s.clone()),
            Value::Object(_) => response
                .get("text")
                .or_else(|| response.get("output"))
                .and_then(|v| v.as_str())
                .map(String::from),
            _ => None,
        };
        if let Some(text) = candidate {
            if !text.is_empty() {
                return Some(text);
            }
        }
    }
    None
}

/// pre_llm_call aggregation: collect injected context strings (hermes —
/// a response may be `{"context": "..."}` or a bare non-empty string).
pub fn context_injections(responses: &[Value]) -> Vec<String> {
    responses
        .iter()
        .filter_map(|r| match r {
            Value::String(s) => Some(s.clone()),
            Value::Object(_) => r.get("context").and_then(|v| v.as_str()).map(String::from),
            _ => None,
        })
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// pre_gateway_dispatch aggregation: first dict response carrying an
/// `action` wins (hermes gateway/run.py). Returns the lowercased action
/// plus its `text` (rewrite payload) or `reason` (skip explanation).
pub fn dispatch_decision(responses: &[Value]) -> Option<(String, Option<String>)> {
    for response in responses {
        let Some(object) = response.as_object() else {
            continue;
        };
        let Some(action) = object.get("action").and_then(|v| v.as_str()) else {
            continue;
        };
        let text = object
            .get("text")
            .or_else(|| object.get("reason"))
            .and_then(|v| v.as_str())
            .map(String::from);
        return Some((action.to_lowercase(), text));
    }
    None
}

/// Register every plugin tool into the registry as
/// `plugin__<plugin>__<tool>` (hermes `register_tool` namespacing).
pub fn register_plugin_tools(registry: &mut ToolRegistry) -> usize {
    let mut count = 0;
    for plugin in loaded_plugins() {
        if plugin.disabled {
            continue;
        }
        for spec in &plugin.manifest.tools {
            let qualified = format!("plugin__{}__{}", plugin.manifest.name, spec.name);
            let command_path = if Path::new(&spec.command).is_absolute() {
                PathBuf::from(&spec.command)
            } else {
                plugin.dir.join(&spec.command)
            };
            if !command_path.is_file() {
                continue;
            }
            let parameters = spec
                .parameters
                .as_ref()
                .and_then(|t| serde_json::to_value(t).ok())
                .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
            let description = if spec.description.is_empty() {
                format!("[plugin {}] {}", plugin.manifest.name, spec.name)
            } else {
                format!("[plugin {}] {}", plugin.manifest.name, spec.description)
            };
            let toolset = format!("plugin:{}", plugin.manifest.name);
            let handler_path = command_path.clone();
            let check_path = command_path.clone();
            let panic_name = qualified.clone();
            let tool_name = spec.name.clone();
            registry.register(
                tool(qualified)
                    .description(description)
                    .parameters(parameters)
                    .handler(move |args, _ctx| {
                        let command_path = handler_path.clone();
                        let tool_name = tool_name.clone();
                        async move {
                            let payload = json!({"tool": tool_name, "arguments": args});
                            let mut cmd = tokio::process::Command::new(&command_path);
                            cmd.stdin(std::process::Stdio::piped())
                                .stdout(std::process::Stdio::piped())
                                .stderr(std::process::Stdio::null());
                            let mut child = cmd.spawn().map_err(|e| {
                                crate::error::AgentError::Tool(format!(
                                    "plugin tool spawn failed: {e}"
                                ))
                            })?;
                            if let Some(mut stdin) = child.stdin.take() {
                                stdin
                                    .write_all(
                                        serde_json::to_string(&payload)
                                            .unwrap_or_default()
                                            .as_bytes(),
                                    )
                                    .await
                                    .ok();
                            }
                            let output = tokio::time::timeout(
                                Duration::from_secs(120),
                                child.wait_with_output(),
                            )
                            .await
                            .map_err(|_| {
                                crate::error::AgentError::Tool("plugin tool timed out".into())
                            })?
                            .map_err(|e| {
                                crate::error::AgentError::Tool(format!("plugin tool: {e}"))
                            })?;
                            let text = String::from_utf8_lossy(&output.stdout);
                            if !output.status.success() {
                                return Ok(json!({
                                    "success": false,
                                    "error": format!("plugin tool exited with {}", output.status),
                                }));
                            }
                            Ok(serde_json::from_str::<Value>(text.trim()).unwrap_or_else(
                                |_| json!({"success": true, "output": text.trim()}),
                            ))
                        }
                    })
                    .toolset(toolset)
                    .emoji("🧩")
                    .check_fn(move || {
                        if check_path.is_file() {
                            ToolAvailability::available()
                        } else {
                            ToolAvailability::unavailable("plugin tool script missing")
                        }
                    })
                    .build()
                    .unwrap_or_else(|_| panic!("{panic_name} builds")),
            );
            count += 1;
        }
    }
    count
}

/// transform_llm_output convenience (hermes turn_finalizer site): run the
/// hook over the final assistant text; first non-empty replacement wins.
pub async fn transform_llm_output(session_id: &str, cwd: &Path, text: &str) -> String {
    let payload = hook_payload(
        "transform_llm_output",
        session_id,
        cwd,
        vec![("response_text", serde_json::json!(text))],
        serde_json::json!({}),
    );
    let responses = invoke_hook("transform_llm_output", payload).await;
    transform_text(&responses).unwrap_or_else(|| text.to_string())
}

/// Fire a session lifecycle hook (on_session_start / on_session_end /
/// on_session_finalize / on_session_reset) — observers, never fatal.
pub async fn fire_session_event(event: &str, session_id: &str, cwd: &Path, extra: Value) {
    let payload = hook_payload(event, session_id, cwd, vec![], extra);
    let _ = invoke_hook(event, payload).await;
}

/// Accept all pending config hooks into the allowlist (CLI helper).
pub fn accept_all_hooks(home: &Path, config: &crate::config::UlncLawConfig) -> usize {
    let mut allowlist = load_allowlist(home);
    let mut added = 0;
    for (event, commands) in &config.hooks.events {
        if !VALID_HOOKS.contains(&event.as_str()) {
            continue;
        }
        for command in commands {
            let key = format!("{event}\t{command}");
            if !allowlist.iter().any(|a| a == &key) {
                allowlist.push(key);
                added += 1;
            }
        }
    }
    if added > 0 {
        save_allowlist(home, &allowlist);
    }
    added
}

/// Snapshot of the consent allowlist (`event\tcommand` entries) for the
/// `hooks` CLI.
pub fn allowlist_entries(home: &Path) -> Vec<String> {
    load_allowlist(home)
}

/// Revoke consent for every entry whose command equals `command`
/// (hermes `hooks revoke`). Returns the number of entries removed.
pub fn revoke_allowlist(home: &Path, command: &str) -> usize {
    let entries = load_allowlist(home);
    let before = entries.len();
    let kept: Vec<String> = entries
        .into_iter()
        .filter(|entry| entry.split('\t').nth(1) != Some(command))
        .collect();
    let removed = before - kept.len();
    if removed > 0 {
        save_allowlist(home, &kept);
    }
    removed
}

/// Default per-event test payloads (hermes `_DEFAULT_PAYLOADS`): what a
/// script sees under `hooks test` / `hooks doctor` is shape-identical to
/// runtime payloads.
pub fn default_hook_payload(event: &str) -> Value {
    let raw: &str = match event {
        "pre_tool_call" => {
            r#"{"tool_name":"terminal","args":{"command":"echo hello"},"session_id":"test-session","task_id":"test-task","tool_call_id":"test-call"}"#
        }
        "post_tool_call" => {
            r#"{"tool_name":"terminal","args":{"command":"echo hello"},"session_id":"test-session","task_id":"test-task","tool_call_id":"test-call","result":"{\"output\": \"hello\"}","duration_ms":42}"#
        }
        "pre_llm_call" => {
            r#"{"session_id":"test-session","user_message":"What is the weather?","conversation_history":[],"is_first_turn":true,"model":"gpt-4","platform":"cli"}"#
        }
        "post_llm_call" => r#"{"session_id":"test-session","model":"gpt-4","platform":"cli"}"#,
        "pre_verify" => {
            r#"{"session_id":"test-session","platform":"cli","model":"gpt-4","coding":true,"attempt":0,"final_response":"All done — the change is applied.","changed_paths":["src/app.tsx"]}"#
        }
        "on_session_start" => r#"{"session_id":"test-session"}"#,
        "on_session_end" => {
            r#"{"session_id":"test-session","task_id":"test-task","turn_id":"test-turn","completed":true,"failed":false,"interrupted":false,"turn_exit_reason":"text_response(stop)","model":"gpt-4","platform":"cli"}"#
        }
        "on_session_finalize" => r#"{"session_id":"test-session"}"#,
        "on_session_reset" => r#"{"session_id":"test-session"}"#,
        "pre_api_request" => {
            r#"{"session_id":"test-session","task_id":"test-task","platform":"cli","model":"claude-sonnet-4-6","provider":"anthropic","base_url":"https://api.anthropic.com","api_mode":"anthropic_messages","api_call_count":1,"message_count":4,"tool_count":12,"approx_input_tokens":2048,"request_char_count":8192,"max_tokens":4096}"#
        }
        "post_api_request" => {
            r#"{"session_id":"test-session","task_id":"test-task","platform":"cli","model":"claude-sonnet-4-6","provider":"anthropic","base_url":"https://api.anthropic.com","api_mode":"anthropic_messages","api_call_count":1,"api_duration":1.234,"finish_reason":"stop","message_count":4,"response_model":"claude-sonnet-4-6","usage":{"input_tokens":2048,"output_tokens":512},"assistant_content_chars":1200,"assistant_tool_call_count":0}"#
        }
        "subagent_stop" => {
            r#"{"parent_session_id":"parent-sess","child_role":null,"child_summary":"Synthetic summary for hooks test","child_status":"completed","tool_call_history":[{"tool_name":"write_file","tool_input":{"argument_keys":["content","path"],"targets":{"path":"/tmp/report.txt"}},"input_bytes":128,"output_bytes":32,"status":"ok"}],"duration_ms":1234}"#
        }
        "pre_gateway_dispatch" => {
            r#"{"platform":"telegram","chat_id":"12345","sender_id":"67890","sender_name":"test-user","text":"hello","message_id":"1"}"#
        }
        _ => r#"{"extra":{}}"#,
    };
    serde_json::from_str(raw).unwrap_or_else(|_| json!({"extra": {}}))
}

/// One `hooks doctor` probe result: (event, command, outcome).
pub struct HookProbe {
    pub event: String,
    pub command: String,
    pub consented: bool,
    pub ok: bool,
    pub detail: String,
}

/// Run every configured hook with its default payload and report per-command
/// outcomes (hermes `hooks doctor`). Non-consented hooks are reported but
/// not executed.
pub async fn doctor_hooks(home: &Path, config: &crate::config::UlncLawConfig) -> Vec<HookProbe> {
    let allowlist = load_allowlist(home);
    let mut probes = Vec::new();
    let mut events: Vec<(&String, &Vec<String>)> = config.hooks.events.iter().collect();
    events.sort_by(|a, b| a.0.cmp(b.0));
    for (event, commands) in events {
        if !VALID_HOOKS.contains(&event.as_str()) {
            for command in commands {
                probes.push(HookProbe {
                    event: event.clone(),
                    command: command.clone(),
                    consented: false,
                    ok: false,
                    detail: "unknown hook event".into(),
                });
            }
            continue;
        }
        let payload = default_hook_payload(event);
        for command in commands {
            let key = format!("{event}\t{command}");
            let consented = allowlist.iter().any(|a| a == &key);
            if !consented {
                probes.push(HookProbe {
                    event: event.clone(),
                    command: command.clone(),
                    consented: false,
                    ok: false,
                    detail: "not consented (plugins accept-hooks)".into(),
                });
                continue;
            }
            let callback = HookCallback {
                event: event.clone(),
                command: command.clone(),
            };
            let outcome = run_hook_callback(&callback, &payload).await;
            probes.push(HookProbe {
                event: event.clone(),
                command: command.clone(),
                consented: true,
                ok: outcome.is_some(),
                detail: match outcome {
                    Some(value) => format!(
                        "responded: {}",
                        truncate_for_display(&value.to_string(), 80)
                    ),
                    None => {
                        "no valid JSON response (exit != 0, timeout, or unparseable stdout)".into()
                    }
                },
            });
        }
    }
    probes
}

fn truncate_for_display(text: &str, max: usize) -> String {
    let flat: String = text.chars().filter(|c| *c != '\n').collect();
    if flat.chars().count() <= max {
        flat
    } else {
        flat.chars().take(max).collect::<String>() + "…"
    }
}

/// Read the `plugins.disabled` list from config.toml (source of truth for
/// enable/disable persistence).
fn read_disabled_list(config_path: &Path) -> Vec<String> {
    std::fs::read_to_string(config_path)
        .ok()
        .and_then(|text| text.parse::<toml::Value>().ok())
        .and_then(|doc| doc.get("plugins").and_then(|p| p.get("disabled")).cloned())
        .and_then(|v| v.as_array().cloned())
        .map(|arr| {
            arr.into_iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn write_disabled_list(config_path: &Path, disabled: &[String]) -> Result<(), String> {
    // Never clobber on parse failure — refuse instead.
    let text = std::fs::read_to_string(config_path).unwrap_or_default();
    let mut doc: toml::Table = if text.trim().is_empty() {
        toml::Table::new()
    } else {
        toml::from_str(&text).map_err(|e| format!("config.toml parse failed: {e}"))?
    };
    let plugins = doc
        .entry("plugins")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    let plugins_table = plugins.as_table_mut().ok_or("[plugins] is not a table")?;
    plugins_table.insert(
        "disabled".to_string(),
        toml::Value::Array(
            disabled
                .iter()
                .map(|s| toml::Value::String(s.clone()))
                .collect(),
        ),
    );
    // toml::to_string renders document form ([plugins] section), unlike
    // Value's Display which emits inline tables.
    let rendered = toml::to_string(&doc).map_err(|e| e.to_string())?;
    std::fs::write(config_path, rendered).map_err(|e| e.to_string())
}

/// Current deny-list straight from config.toml (fresh read for `plugins
/// list` after enable/disable in the same process).
pub fn current_disabled(home: &Path) -> Vec<String> {
    read_disabled_list(&home.join("config.toml"))
}

/// Enable a plugin (remove from the deny-list) — hermes `plugins enable`.
pub fn enable_plugin(home: &Path, name: &str) -> Result<String, String> {
    let config_path = home.join("config.toml");
    let mut disabled = read_disabled_list(&config_path);
    if !disabled.iter().any(|d| d == name) {
        return Ok(format!("{name} is already enabled."));
    }
    disabled.retain(|d| d != name);
    write_disabled_list(&config_path, &disabled)?;
    Ok(format!(
        "✓ Enabled plugin {name} (takes effect on next start)."
    ))
}

/// Disable a plugin (add to the deny-list) — hermes `plugins disable`.
pub fn disable_plugin(home: &Path, name: &str) -> Result<String, String> {
    let config_path = home.join("config.toml");
    let mut disabled = read_disabled_list(&config_path);
    if disabled.iter().any(|d| d == name) {
        return Ok(format!("{name} is already disabled."));
    }
    disabled.push(name.to_string());
    write_disabled_list(&config_path, &disabled)?;
    Ok(format!(
        "✓ Disabled plugin {name} (takes effect on next start)."
    ))
}

/// GitHub browser-URL segments that mark a repo page rather than a
/// cloneable URL (hermes `_GITHUB_BROWSER_SEGMENTS`).
const GITHUB_BROWSER_SEGMENTS: &[&str] = &[
    "actions", "blob", "commit", "commits", "issues", "pull", "pulls", "releases", "tree", "wiki",
];

/// Git subprocess wall-clock cap (hermes 60 s clone/pull timeout).
const GIT_TIMEOUT: Duration = Duration::from_secs(60);

/// Resolve a plugin identifier to `(git_url, subdir)` (hermes
/// `_resolve_git_url`). Accepts full URLs (https/http/git@/ssh/file),
/// GitHub browser URLs (`/tree/<branch>/<path>`), `#subdir` fragments,
/// `.git/`-boundary subdirs, and `owner/repo[/subdir]` shorthand.
pub fn resolve_git_url(identifier: &str) -> Result<(String, Option<String>), String> {
    let id = identifier.trim();
    let is_url = id.starts_with("https://")
        || id.starts_with("http://")
        || id.starts_with("git@")
        || id.starts_with("ssh://")
        || id.starts_with("file://");
    if is_url {
        if let Some(path) = id.strip_prefix("https://github.com/") {
            let no_query = path.split('?').next().unwrap_or("");
            let no_frag = no_query.split('#').next().unwrap_or("");
            let parts: Vec<&str> = no_frag.trim_matches('/').split('/').collect();
            if parts.len() >= 3
                && !parts[0].is_empty()
                && !parts[1].is_empty()
                && GITHUB_BROWSER_SEGMENTS.contains(&parts[2])
            {
                let repo = parts[1].strip_suffix(".git").unwrap_or(parts[1]);
                let subdir = if parts[2] == "tree" && parts.len() >= 5 {
                    let joined = parts[4..].join("/").trim_matches('/').to_string();
                    if joined.is_empty() {
                        None
                    } else {
                        Some(joined)
                    }
                } else {
                    None
                };
                return Ok((
                    format!("https://github.com/{}/{repo}.git", parts[0]),
                    subdir,
                ));
            }
        }
        // Explicit `#subdir` fragment — unambiguous for any scheme.
        if let Some(hash_idx) = id.find('#') {
            let (url, frag) = id.split_at(hash_idx);
            let frag = frag[1..].trim_matches('/');
            return Ok((
                url.to_string(),
                if frag.is_empty() {
                    None
                } else {
                    Some(frag.to_string())
                },
            ));
        }
        // Natural `.git/` boundary (GitHub-style URLs).
        if let Some(idx) = id.find(".git/") {
            let url = &id[..idx + 4];
            let subdir = id[idx + 5..].trim_matches('/');
            return Ok((
                url.to_string(),
                if subdir.is_empty() {
                    None
                } else {
                    Some(subdir.to_string())
                },
            ));
        }
        return Ok((id.to_string(), None));
    }
    // owner/repo[/subdir...] shorthand
    let parts: Vec<&str> = id
        .trim_matches('/')
        .split('/')
        .filter(|p| !p.is_empty())
        .collect();
    if parts.len() >= 2 {
        let subdir = parts[2..].join("/");
        let subdir = subdir.trim_matches('/').to_string();
        return Ok((
            format!("https://github.com/{}/{}.git", parts[0], parts[1]),
            if subdir.is_empty() {
                None
            } else {
                Some(subdir)
            },
        ));
    }
    Err(format!(
        "Invalid plugin identifier: '{identifier}'. Use a Git URL or 'owner/repo' shorthand (optionally with a subdirectory: 'owner/repo/path/to/plugin')."
    ))
}

/// Repo name for the plugin directory from a git URL (hermes
/// `_repo_name_from_url`).
pub fn repo_name_from_url(url: &str) -> String {
    let mut name = url.trim_end_matches('/');
    if let Some(stripped) = name.strip_suffix(".git") {
        name = stripped;
    }
    let last = name.rsplit('/').next().unwrap_or(name);
    // ssh-style urls: git@github.com:owner/repo
    let last = last.rsplit(':').next().unwrap_or(last);
    let last = last.rsplit('/').next().unwrap_or(last);
    last.to_string()
}

/// Validate a plugin name and return the safe target directory inside
/// `<home>/plugins` (hermes `_sanitize_plugin_name`, install mode:
/// no subdirectories allowed).
pub fn sanitize_plugin_target(home: &Path, name: &str) -> Result<PathBuf, String> {
    if name.is_empty() {
        return Err("Plugin name must not be empty.".to_string());
    }
    if name == "." || name == ".." {
        return Err(format!(
            "Invalid plugin name '{name}': must not reference the plugins directory itself."
        ));
    }
    if name.contains("..") || name.contains('\\') || name.contains('/') {
        return Err(format!(
            "Invalid plugin name '{name}': path separators and traversal sequences are not allowed."
        ));
    }
    let plugins = plugins_dir(home);
    let target = plugins.join(name);
    // Defense in depth: the joined path must stay inside the plugins dir.
    let norm_plugins = plugins.components().count();
    if target.components().count() != norm_plugins + 1 {
        return Err(format!(
            "Plugin name '{name}' escapes the plugins directory."
        ));
    }
    Ok(target)
}

/// Result of a successful plugin install.
#[derive(Debug, Clone)]
pub struct InstalledPlugin {
    pub dir: PathBuf,
    pub name: String,
    /// `false` when the clone carried no recognizable manifest.
    pub has_manifest: bool,
}

/// Read a plugin name from a `plugin.toml`, falling back to a minimal
/// `name:` scan of `plugin.yaml`/`plugin.yml` (hermes repos).
fn read_manifest_name(dir: &Path) -> (Option<String>, bool) {
    if let Ok(text) = std::fs::read_to_string(dir.join("plugin.toml")) {
        if let Ok(manifest) = toml::from_str::<PluginManifest>(&text) {
            return (Some(manifest.name), true);
        }
        return (None, true);
    }
    for yaml in ["plugin.yaml", "plugin.yml"] {
        if let Ok(text) = std::fs::read_to_string(dir.join(yaml)) {
            for line in text.lines() {
                let trimmed = line.trim();
                if let Some(value) = trimmed.strip_prefix("name:") {
                    let value = value.trim().trim_matches(|c| c == '"' || c == '\'');
                    if !value.is_empty() {
                        return (Some(value.to_string()), true);
                    }
                }
            }
            return (None, true);
        }
    }
    (None, false)
}

/// Non-interactive git env (hermes `noninteractive_git_env`): fail fast
/// instead of prompting for credentials.
fn noninteractive_git_env() -> [(String, String); 2] {
    [
        ("GIT_TERMINAL_PROMPT".to_string(), "0".to_string()),
        ("GCM_INTERACTIVE".to_string(), "Never".to_string()),
    ]
}

/// Run a git command with the hermes non-interactive policy (stdin
/// null, 60 s wall-clock cap; reader threads keep the pipes drained so
/// the deadline is honoured even under heavy output). Returns stdout on
/// success.
fn run_git(args: &[&str], cwd: Option<&Path>) -> Result<String, String> {
    use std::io::Read;
    let mut child = match std::process::Command::new("git")
        .args(args)
        .current_dir(cwd.unwrap_or_else(|| Path::new("/")))
        .envs(noninteractive_git_env())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err("git is not installed or not in PATH.".to_string())
        }
        Err(e) => return Err(format!("git failed to start: {e}")),
    };
    let mut stdout_pipe = child.stdout.take().expect("stdout piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr piped");
    let out_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        stdout_pipe.read_to_end(&mut buf).ok();
        buf
    });
    let err_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        stderr_pipe.read_to_end(&mut buf).ok();
        buf
    });
    let deadline = std::time::Instant::now() + GIT_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    child.kill().ok();
                    child.wait().ok();
                    return Err(format!(
                        "Git {} timed out after {} seconds.",
                        args.first().copied().unwrap_or("run"),
                        GIT_TIMEOUT.as_secs()
                    ));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("git wait failed: {e}")),
        }
    };
    let out_buf = out_handle.join().unwrap_or_default();
    let err_buf = err_handle.join().unwrap_or_default();
    if !status.success() {
        let err = String::from_utf8_lossy(&err_buf);
        let out = String::from_utf8_lossy(&out_buf);
        let detail = err.trim();
        let detail = if detail.is_empty() {
            out.trim()
        } else {
            detail
        };
        return Err(format!(
            "Git {} failed:\n{}",
            args.first().copied().unwrap_or("run"),
            detail
        ));
    }
    Ok(String::from_utf8_lossy(&out_buf).to_string())
}

/// Clone a Git plugin into `<home>/plugins` (hermes
/// `_install_plugin_core`): shallow clone to a temp dir, optional
/// subdir with traversal guard, manifest-name discovery, sanitized
/// target, `force` reinstalls over an existing directory.
pub fn install_plugin(
    home: &Path,
    identifier: &str,
    force: bool,
) -> Result<InstalledPlugin, String> {
    let (git_url, subdir) = resolve_git_url(identifier)?;
    let temp = tempfile::tempdir().map_err(|e| e.to_string())?;
    let clone_root = temp.path().join("plugin");
    run_git(
        &[
            "clone",
            "--depth",
            "1",
            &git_url,
            clone_root.to_str().unwrap_or_default(),
        ],
        None,
    )?;

    // Resolve the directory within the clone that holds the plugin.
    let source_dir = match &subdir {
        Some(sub) => {
            let candidate = clone_root.join(sub);
            let canon_root = clone_root
                .canonicalize()
                .map_err(|e| format!("clone root: {e}"))?;
            let canon = candidate.canonicalize().map_err(|_| {
                format!("Plugin subdirectory '{sub}' does not exist in the repository.")
            })?;
            if canon != canon_root && !canon.starts_with(&canon_root) {
                return Err(format!(
                    "Plugin subdirectory '{sub}' escapes the repository."
                ));
            }
            if !canon.is_dir() {
                return Err(format!("Plugin subdirectory '{sub}' is not a directory."));
            }
            canon
        }
        None => clone_root.clone(),
    };

    let (manifest_name, has_manifest) = read_manifest_name(&source_dir);
    let plugin_name = manifest_name
        .clone()
        .filter(|n| !n.is_empty())
        .or_else(|| {
            subdir.as_ref().map(|s| {
                s.trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .unwrap_or("")
                    .to_string()
            })
        })
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| repo_name_from_url(&git_url));
    let target = sanitize_plugin_target(home, &plugin_name)?;

    if target.exists() {
        if !force {
            return Err(format!(
                "Plugin '{plugin_name}' already exists. Use --force to reinstall or run `ulnclaw plugins update {plugin_name}`."
            ));
        }
        std::fs::remove_dir_all(&target).map_err(|e| e.to_string())?;
    }
    std::fs::create_dir_all(target.parent().unwrap_or(home)).map_err(|e| e.to_string())?;
    if std::fs::rename(&source_dir, &target).is_err() {
        // Cross-device fallback (temp on another mount).
        copy_dir_recursive(&source_dir, &target)?;
    }
    let final_name = read_manifest_name(&target).0.unwrap_or(plugin_name);
    Ok(InstalledPlugin {
        dir: target,
        name: final_name,
        has_manifest,
    })
}

/// Recursive directory copy (rename fallback path).
fn copy_dir_recursive(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|e| e.to_string())?;
    for entry in std::fs::read_dir(from).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        let dest = to.join(entry.file_name());
        if path.is_dir() {
            copy_dir_recursive(&path, &dest)?;
        } else {
            std::fs::copy(&path, &dest).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// Update an installed plugin by pulling latest from its git remote
/// (hermes `cmd_update`). Returns the git output.
pub fn update_plugin(home: &Path, name: &str) -> Result<String, String> {
    let target = sanitize_plugin_target(home, name)?;
    if !target.exists() {
        return Err(format!("Plugin '{name}' is not installed."));
    }
    if !target.join(".git").exists() {
        return Err(format!(
            "Plugin '{name}' was not installed from git (no .git directory). Cannot update."
        ));
    }
    run_git(&["pull"], Some(&target))
}

/// Remove an installed plugin directory (hermes `cmd_remove`).
pub fn remove_plugin(home: &Path, name: &str) -> Result<String, String> {
    let target = sanitize_plugin_target(home, name)?;
    if !target.exists() {
        return Err(format!("Plugin '{name}' is not installed."));
    }
    std::fs::remove_dir_all(&target).map_err(|e| e.to_string())?;
    Ok(format!("✓ Removed plugin {name}."))
}

/// One entry in the plugin hub catalog (ulnclaw extension over hermes'
/// vendor hub): a curated `index.json` record or a local-directory
/// candidate discovered under `<home>/plugin-hub/`.
#[derive(Debug, Clone, Serialize)]
pub struct HubCatalogEntry {
    pub name: String,
    pub description: String,
    /// Git URL / `owner/repo` shorthand (index entries) or an absolute
    /// directory path (local-dir candidates).
    pub identifier: String,
    pub version: String,
    pub homepage: String,
    pub tags: Vec<String>,
    /// `index` (curated `index.json`) or `local-dir`.
    pub source: String,
}

/// Load the hub catalog: curated `<home>/plugin-hub/index.json`
/// entries first, then local-directory candidates (a subdirectory
/// holding `plugin.toml`). Deduplicated by name, index wins.
pub fn load_hub_catalog(home: &Path) -> Vec<HubCatalogEntry> {
    let hub = hub_dir(home);
    let mut entries: Vec<HubCatalogEntry> = Vec::new();

    let index_path = hub.join("index.json");
    if let Ok(text) = std::fs::read_to_string(&index_path) {
        if let Ok(value) = serde_json::from_str::<Value>(&text) {
            let list = value
                .get("plugins")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            for item in list {
                let name = item
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                let identifier = item
                    .get("identifier")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                let (Some(name), Some(identifier)) = (name, identifier) else {
                    continue;
                };
                entries.push(HubCatalogEntry {
                    name: name.to_string(),
                    description: item
                        .get("description")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    identifier: identifier.to_string(),
                    version: item
                        .get("version")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    homepage: item
                        .get("homepage")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    tags: item
                        .get("tags")
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default(),
                    source: "index".to_string(),
                });
            }
        }
    }

    if hub.is_dir() {
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(&hub)
            .map(|rd| {
                rd.filter_map(|e| e.ok().map(|e| e.path()))
                    .filter(|p| p.is_dir())
                    .collect()
            })
            .unwrap_or_default();
        dirs.sort();
        for dir in dirs {
            let manifest_path = dir.join("plugin.toml");
            if !manifest_path.is_file() {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&manifest_path) else {
                continue;
            };
            let Ok(manifest) = toml::from_str::<PluginManifest>(&text) else {
                continue;
            };
            if manifest.name.trim().is_empty() {
                continue;
            }
            if entries.iter().any(|e| e.name == manifest.name) {
                continue;
            }
            entries.push(HubCatalogEntry {
                name: manifest.name,
                description: manifest.description,
                identifier: dir.display().to_string(),
                version: manifest.version,
                homepage: String::new(),
                tags: Vec::new(),
                source: "local-dir".to_string(),
            });
        }
    }

    entries
}

/// Install a plugin from a local directory candidate (hub `local-dir`
/// source): manifest-name discovery, sanitized target, recursive copy.
/// Git-backed identifiers go through [`install_plugin`] instead.
pub fn install_local_dir(
    home: &Path,
    source: &Path,
    force: bool,
) -> Result<InstalledPlugin, String> {
    let source = source
        .canonicalize()
        .map_err(|e| format!("Plugin source directory '{}': {e}", source.display()))?;
    if !source.is_dir() {
        return Err(format!(
            "Plugin source '{}' is not a directory.",
            source.display()
        ));
    }
    let (manifest_name, has_manifest) = read_manifest_name(&source);
    let plugin_name = manifest_name.filter(|n| !n.is_empty()).ok_or_else(|| {
        format!(
            "Plugin source '{}' carries no readable manifest name.",
            source.display()
        )
    })?;
    let target = sanitize_plugin_target(home, &plugin_name)?;
    if target.exists() {
        if !force {
            return Err(format!(
                "Plugin '{plugin_name}' already exists. Use force to reinstall."
            ));
        }
        std::fs::remove_dir_all(&target).map_err(|e| e.to_string())?;
    }
    std::fs::create_dir_all(target.parent().unwrap_or(home)).map_err(|e| e.to_string())?;
    copy_dir_recursive(&source, &target)?;
    let final_name = read_manifest_name(&target).0.unwrap_or(plugin_name);
    Ok(InstalledPlugin {
        dir: target,
        name: final_name,
        has_manifest,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_command_handles_quotes_and_escapes() {
        assert_eq!(split_command("a b c"), vec!["a", "b", "c"]);
        assert_eq!(
            split_command("echo 'hello world' \"x y\""),
            vec!["echo", "hello world", "x y"]
        );
        assert_eq!(split_command("a\\ b c"), vec!["a b", "c"]);
        assert_eq!(split_command("  spaced   out  "), vec!["spaced", "out"]);
        assert!(split_command("").is_empty());
    }

    #[test]
    fn manifest_parses() {
        let manifest: PluginManifest = toml::from_str(
            r#"
            name = "demo"
            description = "demo plugin"
            version = "0.1.0"
            hooks = ["pre_tool_call", "on_session_end"]
            [[tools]]
            name = "hello"
            description = "say hi"
            command = "tools/hello"
            "#,
        )
        .unwrap();
        assert_eq!(manifest.name, "demo");
        assert_eq!(manifest.hooks.len(), 2);
        assert_eq!(manifest.tools[0].name, "hello");
    }

    #[test]
    fn block_decision_shapes() {
        let responses = vec![json!({}), json!({"decision": "block", "reason": "nope"})];
        assert_eq!(block_decision(&responses).as_deref(), Some("nope"));
        let responses = vec![json!({"action": "block", "message": "halt"})];
        assert_eq!(block_decision(&responses).as_deref(), Some("halt"));
        let responses = vec![json!({"decision": "allow"})];
        assert!(block_decision(&responses).is_none());
    }

    #[test]
    fn transform_first_nonempty_wins() {
        let responses = vec![json!({}), json!({"text": ""}), json!({"text": "hi"})];
        assert_eq!(transform_text(&responses).as_deref(), Some("hi"));
        let responses = vec![json!("plain string")];
        assert_eq!(transform_text(&responses).as_deref(), Some("plain string"));
        assert!(transform_text(&[]).is_none());
    }

    #[test]
    fn context_injections_collected() {
        let responses = vec![json!({"context": "a"}), json!({}), json!({"context": "b"})];
        assert_eq!(
            context_injections(&responses),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn dispatch_decision_shapes() {
        let responses = vec![
            json!("noise"),
            json!({}),
            json!({"action": "skip", "reason": "handled elsewhere"}),
        ];
        let (action, detail) = dispatch_decision(&responses).unwrap();
        assert_eq!(action, "skip");
        assert_eq!(detail.as_deref(), Some("handled elsewhere"));

        let responses = vec![json!({"action": "REWRITE", "text": "sanitized"})];
        let (action, detail) = dispatch_decision(&responses).unwrap();
        assert_eq!(action, "rewrite");
        assert_eq!(detail.as_deref(), Some("sanitized"));

        let responses = vec![json!({"action": "allow"})];
        let (action, detail) = dispatch_decision(&responses).unwrap();
        assert_eq!(action, "allow");
        assert!(detail.is_none());

        assert!(dispatch_decision(&[]).is_none());
        assert!(dispatch_decision(&[json!("string"), json!(42)]).is_none());
    }

    #[test]
    fn context_injections_accepts_bare_strings() {
        let responses = vec![
            json!({"context": "from-dict"}),
            json!("  bare string  "),
            json!(""),
            json!(3),
            json!({"context": ""}),
        ];
        assert_eq!(
            context_injections(&responses),
            vec!["from-dict".to_string(), "bare string".to_string()]
        );
    }

    #[test]
    fn default_hook_payload_covers_catalog() {
        let payload = default_hook_payload("pre_tool_call");
        assert_eq!(payload["tool_name"], json!("terminal"));
        assert_eq!(payload["session_id"], json!("test-session"));
        let payload = default_hook_payload("post_api_request");
        assert_eq!(payload["usage"]["input_tokens"], json!(2048));
        let payload = default_hook_payload("pre_gateway_dispatch");
        assert_eq!(payload["platform"], json!("telegram"));
        // Unknown events fall back to the generic envelope payload.
        assert_eq!(default_hook_payload("not-a-hook"), json!({"extra": {}}));
    }

    #[test]
    fn revoke_allowlist_removes_matching_entries() {
        let dir = std::env::temp_dir().join(format!("ulnclaw-revoke-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        save_allowlist(
            &dir,
            &[
                "pre_tool_call\t/tmp/a.sh".to_string(),
                "post_llm_call\t/tmp/b.sh".to_string(),
                "pre_tool_call\t/tmp/b.sh".to_string(),
            ],
        );
        assert_eq!(revoke_allowlist(&dir, "/tmp/b.sh"), 2);
        assert_eq!(
            allowlist_entries(&dir),
            vec!["pre_tool_call\t/tmp/a.sh".to_string()]
        );
        assert_eq!(revoke_allowlist(&dir, "/tmp/missing.sh"), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn doctor_hooks_reports_consent_state() {
        let dir = std::env::temp_dir().join(format!("ulnclaw-doctor-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut config = crate::config::UlncLawConfig::default();
        config.hooks.events.insert(
            "on_session_start".to_string(),
            vec!["/bin/echo ignored".to_string()],
        );
        config
            .hooks
            .events
            .insert("not_an_event".to_string(), vec!["/bin/true".to_string()]);
        // Nothing consented: both probes report not-consented / unknown.
        let probes = doctor_hooks(&dir, &config).await;
        assert_eq!(probes.len(), 2);
        assert!(probes.iter().all(|p| !p.ok));
        assert!(probes
            .iter()
            .any(|p| p.detail.contains("unknown hook event")));
        assert!(probes.iter().any(|p| p.detail.contains("not consented")));
        // Consent the echo hook: it runs and responds (echo emits JSON-ish
        // text that fails to parse, so ok stays false but the probe ran).
        save_allowlist(&dir, &["on_session_start\t/bin/echo ignored".to_string()]);
        let probes = doctor_hooks(&dir, &config).await;
        let echo_probe = probes
            .iter()
            .find(|p| p.command == "/bin/echo ignored")
            .unwrap();
        assert!(echo_probe.consented);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn hook_payload_shape() {
        let payload = hook_payload(
            "pre_tool_call",
            "sess1",
            Path::new("/tmp"),
            vec![
                ("tool_name", json!("terminal")),
                ("tool_input", json!({"command": "ls"})),
            ],
            json!({"turn_id": 3}),
        );
        assert_eq!(payload["hook_event_name"], json!("pre_tool_call"));
        assert_eq!(payload["tool_name"], json!("terminal"));
        assert_eq!(payload["session_id"], json!("sess1"));
        assert_eq!(payload["extra"]["turn_id"], json!(3));
    }

    #[test]
    fn valid_hooks_match_hermes() {
        // Spot-check the hermes VALID_HOOKS set is fully enumerated.
        for hook in [
            "pre_tool_call",
            "post_tool_call",
            "transform_llm_output",
            "pre_verify",
            "pre_gateway_dispatch",
            "kanban_task_blocked",
        ] {
            assert!(VALID_HOOKS.contains(&hook), "missing hook {hook}");
        }
        assert_eq!(VALID_HOOKS.len(), 23);
    }

    #[tokio::test]
    async fn run_hook_callback_parses_stdout() {
        if cfg!(windows) {
            return;
        }
        // Serialize against tests that temporarily override ULNCLAW_HOME.
        let _guard = crate::models_dev::test_env_lock();
        let dir = std::env::temp_dir().join(format!("ulnclaw-hook-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("hook.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\ncat > /dev/null\necho '{\"decision\": \"block\", \"reason\": \"test-block\"}'\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let callback = HookCallback {
            event: "pre_tool_call".into(),
            command: script.display().to_string(),
        };
        let response =
            run_hook_callback(&callback, &json!({"hook_event_name": "pre_tool_call"})).await;
        assert_eq!(
            response.and_then(|r| r.get("reason").and_then(|v| v.as_str()).map(String::from)),
            Some("test-block".to_string())
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn run_hook_callback_timeout_is_noop() {
        if cfg!(windows) {
            return;
        }
        let callback = HookCallback {
            event: "pre_tool_call".into(),
            command: "/nonexistent-ulnclaw-hook".into(),
        };
        assert!(run_hook_callback(&callback, &json!({})).await.is_none());
    }

    #[test]
    fn resolve_git_url_shorthand() {
        let (url, sub) = resolve_git_url("owner/repo").unwrap();
        assert_eq!(url, "https://github.com/owner/repo.git");
        assert_eq!(sub, None);
        let (url, sub) = resolve_git_url("owner/repo/path/to/plugin").unwrap();
        assert_eq!(url, "https://github.com/owner/repo.git");
        assert_eq!(sub.as_deref(), Some("path/to/plugin"));
    }

    #[test]
    fn resolve_git_url_full_and_fragment() {
        let (url, sub) = resolve_git_url("https://github.com/o/r.git").unwrap();
        assert_eq!(url, "https://github.com/o/r.git");
        assert_eq!(sub, None);
        let (url, sub) = resolve_git_url("https://github.com/o/r.git/deep/plugin").unwrap();
        assert_eq!(url, "https://github.com/o/r.git");
        assert_eq!(sub.as_deref(), Some("deep/plugin"));
        let (url, sub) = resolve_git_url("git@github.com:o/r.git#sub/dir").unwrap();
        assert_eq!(url, "git@github.com:o/r.git");
        assert_eq!(sub.as_deref(), Some("sub/dir"));
    }

    #[test]
    fn resolve_git_url_browser_tree() {
        let (url, sub) = resolve_git_url("https://github.com/o/r/tree/main/plugins/foo").unwrap();
        assert_eq!(url, "https://github.com/o/r.git");
        assert_eq!(sub.as_deref(), Some("plugins/foo"));
        let (url, sub) = resolve_git_url("https://github.com/o/r/issues").unwrap();
        assert_eq!(url, "https://github.com/o/r.git");
        assert_eq!(sub, None);
    }

    #[test]
    fn resolve_git_url_rejects_garbage() {
        assert!(resolve_git_url("justone").is_err());
        assert!(resolve_git_url("").is_err());
    }

    #[test]
    fn repo_name_from_url_variants() {
        assert_eq!(repo_name_from_url("https://github.com/o/r.git"), "r");
        assert_eq!(repo_name_from_url("https://github.com/o/r"), "r");
        assert_eq!(repo_name_from_url("git@github.com:o/r.git"), "r");
        assert_eq!(repo_name_from_url("file:///tmp/foo.git/"), "foo");
    }

    #[test]
    fn sanitize_plugin_target_guards() {
        let home = Path::new("/tmp/ulnclaw-home");
        assert!(sanitize_plugin_target(home, "good-name").is_ok());
        assert!(sanitize_plugin_target(home, "").is_err());
        assert!(sanitize_plugin_target(home, ".").is_err());
        assert!(sanitize_plugin_target(home, "..").is_err());
        assert!(sanitize_plugin_target(home, "../evil").is_err());
        assert!(sanitize_plugin_target(home, "a/b").is_err());
        assert!(sanitize_plugin_target(home, "a\\b").is_err());
    }

    #[test]
    fn read_manifest_name_toml_and_yaml() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("plugin.toml"),
            "name = \"toml-plugin\"\ndescription = \"d\"",
        )
        .unwrap();
        assert_eq!(
            read_manifest_name(dir.path()),
            (Some("toml-plugin".to_string()), true)
        );
        let dir2 = tempfile::tempdir().unwrap();
        std::fs::write(
            dir2.path().join("plugin.yaml"),
            "name: yaml-plugin
version: 1",
        )
        .unwrap();
        assert_eq!(
            read_manifest_name(dir2.path()),
            (Some("yaml-plugin".to_string()), true)
        );
        let dir3 = tempfile::tempdir().unwrap();
        assert_eq!(read_manifest_name(dir3.path()), (None, false));
    }

    #[test]
    fn install_update_remove_roundtrip_with_local_git() {
        let git_ok = std::process::Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !git_ok {
            return; // git unavailable in this environment
        }
        let run_git_init = |args: &[&str], dir: &std::path::Path| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.com")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.com")
                .output()
                .expect("git runs");
            assert!(
                out.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        // Source repo with a plugin.toml manifest.
        let source = tempfile::tempdir().unwrap();
        run_git_init(&["init", "-q"], source.path());
        std::fs::write(
            source.path().join("plugin.toml"),
            "name = \"roundtrip\"\ndescription = \"d\"",
        )
        .unwrap();
        std::fs::write(source.path().join("hook.sh"), "#!/bin/sh\necho hi").unwrap();
        run_git_init(&["add", "-A"], source.path());
        run_git_init(&["commit", "-qm", "init"], source.path());

        // Install from a file:// URL.
        let home = tempfile::tempdir().unwrap();
        let url = format!("file://{}", source.path().display());
        let installed = install_plugin(home.path(), &url, false).unwrap();
        assert_eq!(installed.name, "roundtrip");
        assert!(installed.has_manifest);
        assert!(installed.dir.join("plugin.toml").exists());
        // Reinstall without --force errors; --force succeeds.
        assert!(install_plugin(home.path(), &url, false).is_err());
        assert!(install_plugin(home.path(), &url, true).is_ok());

        // Update (clone carries .git); a second repo-less plugin cannot.
        let output = update_plugin(home.path(), "roundtrip").unwrap();
        assert!(
            output.contains("up to date")
                || output.contains("Updating")
                || !output.trim().is_empty()
        );
        std::fs::create_dir_all(home.path().join("plugins").join("nogy")).unwrap();
        assert!(update_plugin(home.path(), "nogy").is_err());

        // Remove.
        let message = remove_plugin(home.path(), "roundtrip").unwrap();
        assert!(message.contains("Removed"));
        assert!(!installed.dir.exists());
        assert!(remove_plugin(home.path(), "roundtrip").is_err());
    }
}

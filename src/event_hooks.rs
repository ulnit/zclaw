//! Event hook system — port of hermes `gateway/hooks.py`.
//!
//! A lightweight event-driven system that fires handlers at key
//! lifecycle points. Hooks are discovered from `<home>/hooks/`
//! directories, each containing:
//! - `HOOK.yaml` (metadata: name, description, events list)
//! - `handler.py` (hermes-style Python handler with a top-level
//!   `handle(event_type, context)` function, sync or async), or an
//!   executable `handler` file (any language) that receives the JSON
//!   payload on stdin.
//!
//! Events fired by the gateway:
//! - `gateway:startup` — gateway process starts
//! - `agent:start` — agent begins processing a message
//! - `agent:end` — agent finishes processing
//! - `command:*` — any slash command executed (wildcard match)
//!
//! Errors in hooks are caught and logged but never block the main
//! pipeline. The context passed to `agent:start` / `agent:end`
//! handlers carries: `platform`, `user_id`, `chat_id`, `thread_id`,
//! `chat_type`, `session_id`, `message` (truncated to 500 chars);
//! `agent:end` adds `response` (truncated to 500 chars).
//!
//! Rust cannot load Python modules in-process, so handlers run as
//! bounded subprocesses: `handler.py` is executed through a bootstrap
//! that loads the module and calls its `handle(event_type, context)`
//! function exactly like hermes (so hermes hooks work unmodified);
//! executable handlers receive the raw JSON payload on stdin.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Per-handler subprocess timeout (hooks must never hang the gateway).
pub const HANDLER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Context field values longer than this are truncated (hermes
/// truncates `message`/`response` to 500 chars).
pub const CONTEXT_TEXT_LIMIT: usize = 500;

/// Truncate a context string to the hermes 500-char limit.
pub fn truncate_context_text(text: &str) -> String {
    let mut out: String = text.chars().take(CONTEXT_TEXT_LIMIT).collect();
    if text.chars().count() > CONTEXT_TEXT_LIMIT {
        out.push('…');
    }
    out
}

/// Metadata of one loaded hook (hermes `loaded_hooks` entry).
#[derive(Debug, Clone, serde::Serialize)]
pub struct HookMeta {
    pub name: String,
    pub description: String,
    pub events: Vec<String>,
    pub path: String,
}

#[derive(Debug, Clone)]
enum HandlerKind {
    /// hermes-style `handler.py` run through the compat bootstrap.
    Python { handler_path: PathBuf },
    /// Executable handler receiving the JSON payload on stdin.
    Exec { bin_path: PathBuf },
}

#[derive(Debug, Clone)]
struct Handler {
    name: String,
    kind: HandlerKind,
}

/// Discovered hooks and their event registrations (hermes
/// `HookRegistry`).
#[derive(Debug, Default)]
pub struct HookRegistry {
    /// event_type -> handlers registered for it.
    handlers: HashMap<String, Vec<Handler>>,
    loaded: Vec<HookMeta>,
}

/// Bootstrap that loads a hermes-style `handler.py` and calls its
/// `handle(event_type, context)` function (sync or async), reading the
/// JSON payload from stdin and printing a non-None JSON return value.
const PYTHON_BOOTSTRAP: &str = r#"
import asyncio, importlib.util, json, os, sys
path = os.environ["ULNCLAW_HOOK_HANDLER"]
spec = importlib.util.spec_from_file_location("ulnclaw_hook", path)
if spec is None or spec.loader is None:
    raise RuntimeError("cannot load handler module")
module = importlib.util.module_from_spec(spec)
sys.modules["ulnclaw_hook"] = module
spec.loader.exec_module(module)
if not hasattr(module, "handle"):
    raise RuntimeError("no 'handle' function found")
payload = json.load(sys.stdin)
result = module.handle(payload.get("event_type", ""), payload.get("context", {}))
if asyncio.iscoroutine(result):
    result = asyncio.run(result)
if result is not None:
    json.dump(result, sys.stdout)
"#;

impl HookRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Scan `<home>/hooks/` for hook directories and load their
    /// handlers (hermes `discover_and_load`). Invalid hooks are
    /// skipped with a log line, never fatal.
    pub fn discover_and_load(home: &Path) -> Self {
        let mut registry = Self::new();
        let hooks_dir = home.join("hooks");
        if !hooks_dir.exists() {
            return registry;
        }
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(&hooks_dir)
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| p.is_dir())
                    .collect()
            })
            .unwrap_or_default();
        dirs.sort();
        for hook_dir in dirs {
            let manifest_path = hook_dir.join("HOOK.yaml");
            let python_handler = hook_dir.join("handler.py");
            let exec_handler = hook_dir.join("handler");
            if !manifest_path.exists() || (!python_handler.exists() && !exec_handler.exists()) {
                continue;
            }
            let manifest: Value = match std::fs::read_to_string(&manifest_path)
                .ok()
                .and_then(|raw| serde_yaml::from_str(&raw).ok())
            {
                Some(Value::Object(map)) => Value::Object(map),
                _ => {
                    tracing::warn!("[hooks] Skipping {}: invalid HOOK.yaml", hook_dir.display());
                    continue;
                }
            };
            let hook_name = manifest
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| {
                    hook_dir
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default()
                });
            let events: Vec<String> = manifest
                .get("events")
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            if events.is_empty() {
                tracing::warn!("[hooks] Skipping {hook_name}: no events declared");
                continue;
            }
            let kind = if python_handler.exists() {
                HandlerKind::Python {
                    handler_path: python_handler,
                }
            } else {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    let mode = std::fs::metadata(&exec_handler)
                        .map(|m| m.permissions().mode())
                        .unwrap_or(0);
                    if mode & 0o111 == 0 {
                        tracing::warn!("[hooks] Skipping {hook_name}: handler is not executable");
                        continue;
                    }
                }
                HandlerKind::Exec {
                    bin_path: exec_handler,
                }
            };
            let handler = Handler {
                name: hook_name.clone(),
                kind,
            };
            for event in &events {
                registry
                    .handlers
                    .entry(event.clone())
                    .or_default()
                    .push(handler.clone());
            }
            registry.loaded.push(HookMeta {
                name: hook_name.clone(),
                description: manifest
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                events,
                path: hook_dir.to_string_lossy().to_string(),
            });
            tracing::info!("[hooks] Loaded hook '{hook_name}'");
        }
        registry
    }

    /// Metadata about all loaded hooks (hermes `loaded_hooks`).
    pub fn loaded_hooks(&self) -> Vec<HookMeta> {
        self.loaded.clone()
    }

    /// All handlers that should fire for `event_type`: exact matches
    /// first, then wildcard matches (`command:*` fires for
    /// `command:reset`; a bare `agent` registration does NOT fire for
    /// `agent:start` — hermes `_resolve_handlers`).
    fn resolve_handlers(&self, event_type: &str) -> Vec<&Handler> {
        let mut handlers: Vec<&Handler> = self
            .handlers
            .get(event_type)
            .map(|v| v.iter().collect())
            .unwrap_or_default();
        if let Some(colon) = event_type.find(':') {
            let wildcard = format!("{}:*", &event_type[..colon]);
            if let Some(extra) = self.handlers.get(&wildcard) {
                handlers.extend(extra.iter());
            }
        }
        handlers
    }

    /// Fire all handlers for an event, discarding return values.
    /// Errors are logged, never propagated (hermes `emit`).
    pub async fn emit(&self, event_type: &str, context: &Value) {
        for handler in self.resolve_handlers(event_type) {
            if let Err(e) = run_handler(handler, event_type, context).await {
                tracing::warn!(
                    "[hooks] Error in handler '{}' for '{event_type}': {e}",
                    handler.name
                );
            }
        }
    }

    /// Fire handlers and return their non-null return values in order
    /// (hermes `emit_collect`) — used for decision-style hooks.
    pub async fn emit_collect(&self, event_type: &str, context: &Value) -> Vec<Value> {
        let mut results = Vec::new();
        for handler in self.resolve_handlers(event_type) {
            match run_handler(handler, event_type, context).await {
                Ok(Some(value)) => results.push(value),
                Ok(None) => {}
                Err(e) => tracing::warn!(
                    "[hooks] Error in handler '{}' for '{event_type}': {e}",
                    handler.name
                ),
            }
        }
        results
    }
}

/// Run one handler subprocess with the JSON payload on stdin; returns
/// its parsed JSON stdout when non-empty.
async fn run_handler(
    handler: &Handler,
    event_type: &str,
    context: &Value,
) -> Result<Option<Value>, String> {
    let payload = json!({ "event_type": event_type, "context": context });
    let payload_bytes = serde_json::to_vec(&payload).map_err(|e| e.to_string())?;
    let mut command = match &handler.kind {
        HandlerKind::Python { handler_path } => {
            let mut cmd = tokio::process::Command::new("python3");
            cmd.arg("-c").arg(PYTHON_BOOTSTRAP);
            cmd.env("ULNCLAW_HOOK_HANDLER", handler_path);
            cmd
        }
        HandlerKind::Exec { bin_path } => tokio::process::Command::new(bin_path),
    };
    command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = command.spawn().map_err(|e| e.to_string())?;
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt as _;
        stdin
            .write_all(&payload_bytes)
            .await
            .map_err(|e| e.to_string())?;
    }
    let output = tokio::time::timeout(HANDLER_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| "handler timed out".to_string())?
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "handler exited {:?}: {}",
            output.status.code(),
            stderr.trim()
        ));
    }
    let stdout = std::str::from_utf8(&output.stdout)
        .unwrap_or("")
        .trim()
        .to_string();
    if stdout.is_empty() {
        return Ok(None);
    }
    serde_json::from_str(&stdout)
        .map(Some)
        .map_err(|_| format!("handler returned non-JSON output: {stdout}"))
}

/// Interpreted outcome of a `command:<name>` hook sweep (hermes
/// decision protocol: handlers may return `{"decision": ...}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandHookDecision {
    /// Proceed with normal dispatch (no decision / "allow").
    Allow,
    /// Block the command; `message` is the reply (or a default line).
    Deny(String),
    /// The hook fully handled the command; `message` is the reply
    /// (empty stays silent).
    Handled(String),
    /// Rewrite the command text before core handling.
    Rewrite { command: String, args: String },
}

/// Apply hermes' `command:*` decision protocol to collected hook
/// results: first decisive result wins; "rewrite" re-resolves and
/// stops the sweep (hermes `break`); empty/unknown decisions and
/// non-object results are ignored.
pub fn interpret_command_hook_results(results: &[Value]) -> CommandHookDecision {
    for result in results {
        let Value::Object(map) = result else {
            continue;
        };
        let decision = map
            .get("decision")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_lowercase();
        if decision.is_empty() || decision == "allow" {
            continue;
        }
        let message = map
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        match decision.as_str() {
            "deny" => {
                return CommandHookDecision::Deny(if message.is_empty() {
                    String::new()
                } else {
                    message
                });
            }
            "handled" => return CommandHookDecision::Handled(message),
            "rewrite" => {
                let command = map
                    .get("command_name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .trim_start_matches('/')
                    .to_string();
                if command.is_empty() {
                    continue;
                }
                let args = map
                    .get("raw_args")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_string();
                return CommandHookDecision::Rewrite { command, args };
            }
            _ => continue,
        }
    }
    CommandHookDecision::Allow
}

// ------------------------------------------------------------------
// Process-wide registry (the gateway discovers once at startup and
// fires from many call sites).
// ------------------------------------------------------------------

fn global_registry() -> &'static std::sync::RwLock<Option<Arc<HookRegistry>>> {
    static REGISTRY: std::sync::OnceLock<std::sync::RwLock<Option<Arc<HookRegistry>>>> =
        std::sync::OnceLock::new();
    REGISTRY.get_or_init(|| std::sync::RwLock::new(None))
}

/// Discover hooks under `home` and install the process-wide registry
/// (called once at gateway startup).
pub fn init(home: &Path) {
    let registry = Arc::new(HookRegistry::discover_and_load(home));
    let count = registry.loaded_hooks().len();
    *global_registry().write().unwrap() = Some(registry);
    if count > 0 {
        tracing::info!("[hooks] {count} hook(s) active");
    }
}

/// Metadata for the loaded hooks (empty until `init`).
pub fn loaded_hooks() -> Vec<HookMeta> {
    global_registry()
        .read()
        .unwrap()
        .as_ref()
        .map(|r| r.loaded_hooks())
        .unwrap_or_default()
}

/// Fire-and-forget emit from sync contexts: spawns the handler sweep
/// so hooks never block the pipeline.
pub fn emit(event_type: &str, context: Value) {
    let Some(registry) = global_registry().read().unwrap().clone() else {
        return;
    };
    let event = event_type.to_string();
    tokio::spawn(async move {
        registry.emit(&event, &context).await;
    });
}

/// Awaited emit for call sites that already run async (returns when
/// every handler finished or timed out).
pub async fn emit_async(event_type: &str, context: Value) {
    let Some(registry) = global_registry().read().unwrap().clone() else {
        return;
    };
    registry.emit(event_type, &context).await;
}

/// Decision-style emit (hermes `emit_collect`).
pub async fn emit_collect(event_type: &str, context: Value) -> Vec<Value> {
    let Some(registry) = global_registry().read().unwrap().clone() else {
        return Vec::new();
    };
    registry.emit_collect(event_type, &context).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_hook(home: &Path, dir: &str, manifest: &str, handler: Option<(&str, &str)>) {
        let hook_dir = home.join("hooks").join(dir);
        std::fs::create_dir_all(&hook_dir).unwrap();
        std::fs::write(hook_dir.join("HOOK.yaml"), manifest).unwrap();
        if let Some((name, body)) = handler {
            std::fs::write(hook_dir.join(name), body).unwrap();
            if name == "handler" {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    std::fs::set_permissions(
                        hook_dir.join(name),
                        std::fs::Permissions::from_mode(0o755),
                    )
                    .unwrap();
                }
            }
        }
    }

    #[test]
    fn test_discover_valid_hooks() {
        let temp = tempfile::tempdir().unwrap();
        write_hook(
            temp.path(),
            "greeter",
            "name: greeter\ndescription: says hi\nevents:\n- agent:start\n- command:*\n",
            Some((
                "handler.py",
                "def handle(event_type, context):\n    return None\n",
            )),
        );
        let registry = HookRegistry::discover_and_load(temp.path());
        let hooks = registry.loaded_hooks();
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].name, "greeter");
        assert_eq!(hooks[0].description, "says hi");
        assert_eq!(
            hooks[0].events,
            vec!["agent:start".to_string(), "command:*".to_string()]
        );
    }

    #[test]
    fn test_discover_skips_invalid() {
        let temp = tempfile::tempdir().unwrap();
        // No events declared.
        write_hook(
            temp.path(),
            "noevents",
            "name: noevents\nevents: []\n",
            Some(("handler.py", "def handle(e, c):\n    return None\n")),
        );
        // Invalid yaml.
        write_hook(
            temp.path(),
            "badyaml",
            ": : :",
            Some(("handler.py", "def handle(e, c):\n    return None\n")),
        );
        // Missing handler file.
        write_hook(
            temp.path(),
            "nohandler",
            "name: nohandler\nevents:\n- agent:start\n",
            None,
        );
        // Missing manifest.
        let bare = temp.path().join("hooks").join("bare");
        std::fs::create_dir_all(&bare).unwrap();
        std::fs::write(bare.join("handler.py"), "def handle(e, c):\n    pass\n").unwrap();
        let registry = HookRegistry::discover_and_load(temp.path());
        assert!(registry.loaded_hooks().is_empty());
    }

    #[test]
    fn test_wildcard_resolution() {
        let temp = tempfile::tempdir().unwrap();
        write_hook(
            temp.path(),
            "cmdwatch",
            "name: cmdwatch\nevents:\n- command:*\n",
            Some(("handler.py", "def handle(e, c):\n    return None\n")),
        );
        write_hook(
            temp.path(),
            "exact",
            "name: exact\nevents:\n- command:reset\n",
            Some(("handler.py", "def handle(e, c):\n    return None\n")),
        );
        let registry = HookRegistry::discover_and_load(temp.path());
        // Exact + wildcard both fire for command:reset.
        assert_eq!(registry.resolve_handlers("command:reset").len(), 2);
        // Wildcard alone fires for other commands.
        assert_eq!(registry.resolve_handlers("command:new").len(), 1);
        // Bare registrations do not wildcard-match.
        assert!(registry.resolve_handlers("agent:start").is_empty());
    }

    #[tokio::test]
    async fn test_emit_exec_handler() {
        let temp = tempfile::tempdir().unwrap();
        let out_file = temp.path().join("seen.txt");
        let script = format!("#!/bin/sh\ncat >> {}\n", out_file.to_string_lossy());
        write_hook(
            temp.path(),
            "writer",
            "name: writer\nevents:\n- gateway:startup\n",
            Some(("handler", &script)),
        );
        let registry = HookRegistry::discover_and_load(temp.path());
        registry
            .emit("gateway:startup", &json!({"boot": true}))
            .await;
        let seen = std::fs::read_to_string(&out_file).unwrap();
        assert!(seen.contains("gateway:startup"), "{seen}");
        assert!(seen.contains("\"boot\":true"), "{seen}");
    }

    #[tokio::test]
    async fn test_emit_collect_exec_handler() {
        let temp = tempfile::tempdir().unwrap();
        write_hook(
            temp.path(),
            "decider",
            "name: decider\nevents:\n- command:reset\n",
            Some((
                "handler",
                "#!/bin/sh\ncat > /dev/null\necho '{\"verdict\": \"allow\"}'\n",
            )),
        );
        let registry = HookRegistry::discover_and_load(temp.path());
        let results = registry.emit_collect("command:reset", &json!({})).await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["verdict"], "allow");
    }

    #[tokio::test]
    async fn test_emit_python_handler_hermes_compat() {
        if std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_err()
        {
            return; // python3 unavailable — skip silently
        }
        let temp = tempfile::tempdir().unwrap();
        let out_file = temp.path().join("py_seen.txt");
        let mut handler =
            String::from("import json\n\ndef handle(event_type, context):\n    open(r'");
        handler.push_str(&out_file.to_string_lossy());
        handler.push_str(
            "', 'w').write(json.dumps([event_type, context]))\n    return {\"ok\": True}\n",
        );
        write_hook(
            temp.path(),
            "pyhook",
            "name: pyhook\nevents:\n- agent:start\n",
            Some(("handler.py", &handler)),
        );
        let registry = HookRegistry::discover_and_load(temp.path());
        let results = registry
            .emit_collect("agent:start", &json!({"platform": "telegram"}))
            .await;
        assert_eq!(results.len(), 1, "python handler should return its dict");
        assert_eq!(results[0]["ok"], true);
        let seen = std::fs::read_to_string(&out_file).unwrap();
        assert!(seen.contains("agent:start"), "{seen}");
        assert!(seen.contains("telegram"), "{seen}");
    }

    #[tokio::test]
    async fn test_handler_errors_do_not_propagate() {
        let temp = tempfile::tempdir().unwrap();
        write_hook(
            temp.path(),
            "broken",
            "name: broken\nevents:\n- agent:start\n",
            Some(("handler", "#!/bin/sh\necho boom >&2\nexit 1\n")),
        );
        let registry = HookRegistry::discover_and_load(temp.path());
        // Must not panic; broken handler is logged and skipped.
        registry.emit("agent:start", &json!({})).await;
        assert!(registry
            .emit_collect("agent:start", &json!({}))
            .await
            .is_empty());
    }

    #[test]
    fn test_truncate_context_text() {
        let long = "x".repeat(600);
        let out = truncate_context_text(&long);
        assert_eq!(out.chars().count(), CONTEXT_TEXT_LIMIT + 1);
        assert!(out.ends_with('…'));
        assert_eq!(truncate_context_text("short"), "short");
    }

    #[test]
    fn test_command_hook_decision_protocol() {
        use super::CommandHookDecision::*;
        // No results → allow.
        assert_eq!(interpret_command_hook_results(&[]), Allow);
        // Allow / empty decisions pass through.
        assert_eq!(
            interpret_command_hook_results(&[json!({"decision": "allow"}), json!({})]),
            Allow
        );
        // Deny with custom message.
        assert_eq!(
            interpret_command_hook_results(&[json!({"decision": "deny", "message": "nope"})]),
            Deny("nope".into())
        );
        // Deny without message → empty string (caller supplies default).
        assert_eq!(
            interpret_command_hook_results(&[json!({"decision": "DENY"})]),
            Deny(String::new())
        );
        // Handled silently.
        assert_eq!(
            interpret_command_hook_results(&[json!({"decision": "handled"})]),
            Handled(String::new())
        );
        // Rewrite strips the leading slash from command_name.
        assert_eq!(
            interpret_command_hook_results(&[
                json!({"decision": "rewrite", "command_name": "/status", "raw_args": " full"})
            ]),
            Rewrite {
                command: "status".into(),
                args: "full".into()
            }
        );
        // Rewrite without command_name is ignored; first decisive wins.
        assert_eq!(
            interpret_command_hook_results(&[
                json!({"decision": "rewrite"}),
                json!({"decision": "handled", "message": "done"})
            ]),
            Handled("done".into())
        );
        // Non-object results ignored.
        assert_eq!(
            interpret_command_hook_results(&[json!("telemetry"), json!(42)]),
            Allow
        );
    }

    #[test]
    fn test_missing_hooks_dir_is_empty() {
        let temp = tempfile::tempdir().unwrap();
        let registry = HookRegistry::discover_and_load(temp.path());
        assert!(registry.loaded_hooks().is_empty());
    }
}

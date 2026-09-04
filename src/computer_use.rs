//! Computer-use backend — port of hermes `tools/computer_use/` (cua-driver).
//!
//! Drives the desktop in the background through the `cua-driver` daemon,
//! speaking MCP over stdio (hermes `cua_backend.py`). Unlike foreground
//! automation, cua-driver posts events scoped to the target pid/window, so
//! the agent and the user can co-work on the same machine.
//!
//! * `resolve_cua_driver_cmd` — env override → PATH → well-known install dirs
//! * `cua_driver_child_env`  — telemetry policy (`CUA_DRIVER_RS_TELEMETRY_ENABLED`)
//! * `handle_computer_use`    — the `computer_use` tool dispatch (schema.py)
//!
//! Every non-`capture` action goes through the context approval callback;
//! unattended runs without a callback are refused (hermes approval hook).

use crate::error::{AgentError, Result};
use crate::mcp::{McpClient, McpServerConfig};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, OnceCell};

/// Env var cua-driver reads for its telemetry opt-out.
pub const CUA_TELEMETRY_ENV_VAR: &str = "CUA_DRIVER_RS_TELEMETRY_ENABLED";

/// `[computer_use]` config block (hermes `config_defaults.computer_use`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComputerUseConfig {
    /// cua-driver ships PostHog telemetry ENABLED upstream; ulnclaw disables
    /// it unless the user opts in here (hermes `cua_telemetry`).
    #[serde(default)]
    pub cua_telemetry: bool,
    /// Longest-edge cap (px) for driver screenshots, applied via
    /// `set_config` at session start; 0 disables (hermes default 1456).
    #[serde(default = "default_max_image_dimension")]
    pub max_image_dimension: u32,
    /// Follow-up capture mode: som | ax | vision (hermes `capture_after_mode`).
    #[serde(default = "default_capture_after_mode")]
    pub capture_after_mode: String,
    /// Cursor overlay tri-state (hermes `no_overlay`): None = auto-detect
    /// (off on macOS + headless/WSL2 Linux), true = always off, false = on.
    #[serde(default)]
    pub no_overlay: Option<bool>,
}

fn default_max_image_dimension() -> u32 {
    1456
}

fn default_capture_after_mode() -> String {
    "som".to_string()
}

impl Default for ComputerUseConfig {
    fn default() -> Self {
        Self {
            cua_telemetry: false,
            max_image_dimension: default_max_image_dimension(),
            capture_after_mode: default_capture_after_mode(),
            no_overlay: None,
        }
    }
}

/// Resolve the `cua-driver` command (hermes `resolve_cua_driver_cmd`):
/// `ULNCLAW_CUA_DRIVER_CMD` override → PATH → well-known install dirs.
pub fn resolve_cua_driver_cmd() -> Option<String> {
    if let Ok(override_cmd) = std::env::var("ULNCLAW_CUA_DRIVER_CMD") {
        let trimmed = override_cmd.trim().to_string();
        if !trimmed.is_empty() {
            return Some(trimmed);
        }
    }
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in path_var.split(':') {
            let candidate = Path::new(dir).join("cua-driver");
            if candidate.is_file() {
                return Some(candidate.display().to_string());
            }
        }
    }
    let home = dirs_home()?;
    for rel in [
        ".local/bin/cua-driver",
        ".cua/bin/cua-driver",
        "bin/cua-driver",
    ] {
        let candidate = home.join(rel);
        if candidate.is_file() {
            return Some(candidate.display().to_string());
        }
    }
    for abs in ["/usr/local/bin/cua-driver", "/opt/homebrew/bin/cua-driver"] {
        if Path::new(abs).is_file() {
            return Some(abs.to_string());
        }
    }
    None
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// True when the overlay should be disabled (hermes `_cua_no_overlay`):
/// explicit config wins; auto-detect disables on macOS and headless/WSL2
/// Linux where the idle overlay is a known CPU-burn failure mode.
pub fn no_overlay_resolved(cfg: &ComputerUseConfig) -> bool {
    if let Some(explicit) = cfg.no_overlay {
        return explicit;
    }
    if cfg!(target_os = "macos") {
        return true;
    }
    if !cfg!(target_os = "linux") {
        return false;
    }
    if std::env::var("DISPLAY").unwrap_or_default().is_empty() {
        return true;
    }
    if let Ok(version) = std::fs::read_to_string("/proc/version") {
        if version.to_lowercase().contains("microsoft") {
            return true;
        }
    }
    false
}

/// Child env for every cua-driver spawn (hermes `cua_driver_child_env`):
/// telemetry disabled unless the user opted in.
pub fn cua_driver_child_env(cfg: &ComputerUseConfig) -> HashMap<String, String> {
    let mut env: HashMap<String, String> = std::env::vars().collect();
    if !cfg.cua_telemetry {
        env.insert(CUA_TELEMETRY_ENV_VAR.to_string(), "0".to_string());
    }
    env
}

/// Install hint shown when cua-driver is missing (hermes
/// `cua_driver_install_hint`).
pub fn cua_driver_install_hint() -> String {
    let installer = if cfg!(windows) {
        "  irm https://raw.githubusercontent.com/trycua/cua/main/\
         libs/cua-driver/scripts/install.ps1 | iex"
            .to_string()
    } else {
        "  /bin/bash -c \"$(curl -fsSL \
         https://raw.githubusercontent.com/trycua/cua/main/\
         libs/cua-driver/scripts/install.sh)\""
            .to_string()
    };
    format!(
        "cua-driver is not installed. Install with one of:\n  \
         ulnclaw computer-use install\nOr run the upstream installer directly:\n{}\n",
        installer
    )
}

/// Shared cua-driver MCP session state (hermes `_SESSION` module global).
struct CuaSession {
    client: McpClient,
    session_id: String,
    active_pid: Option<u64>,
    active_window_id: Option<u64>,
    active_app: String,
    active_title: String,
    tools: Vec<String>,
}

static SESSION: OnceCell<Arc<Mutex<Option<CuaSession>>>> = OnceCell::const_new();

async fn session_cell() -> &'static Arc<Mutex<Option<CuaSession>>> {
    SESSION
        .get_or_init(|| async { Arc::new(Mutex::new(None)) })
        .await
}

/// Does the driver binary advertise `--no-overlay` (hermes
/// `_cua_driver_supports_no_overlay`)? Probe `mcp --help` with a short
/// timeout; unknown → assume unsupported so old drivers still spawn.
fn driver_supports_no_overlay(driver: &str) -> bool {
    let out = std::process::Command::new(driver)
        .args(["mcp", "--help"])
        .output();
    match out {
        Ok(output) => {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            text.contains("--no-overlay")
        }
        Err(_) => false,
    }
}

/// Connect (or reuse) the shared cua-driver MCP session.
async fn with_session<F, T>(cfg: &ComputerUseConfig, f: F) -> Result<T>
where
    F: FnOnce(
        &mut CuaSession,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<T>> + Send + '_>>,
{
    let cell = session_cell().await;
    let mut guard = cell.lock().await;
    if guard.is_none() {
        let driver =
            resolve_cua_driver_cmd().ok_or_else(|| AgentError::Tool(cua_driver_install_hint()))?;
        let mut args = vec!["mcp".to_string()];
        if no_overlay_resolved(cfg) && driver_supports_no_overlay(&driver) {
            args.push("--no-overlay".to_string());
        }
        let server_cfg = McpServerConfig {
            name: "cua-driver".to_string(),
            command: driver.clone(),
            args,
            env: cua_driver_child_env(cfg),
            url: None,
            transport: None,
            headers: HashMap::new(),
            auth: None,
            oauth: crate::mcp::oauth::McpOAuthConfig::default(),
            lazy: false,
            enabled: true,
        };
        let mut client = McpClient::connect(&server_cfg)
            .await
            .map_err(|e| AgentError::Tool(format!("cua-driver MCP connect failed: {e}")))?;
        let tools: Vec<String> = client
            .list_tools()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter_map(|t| t.get("name").and_then(|v| v.as_str()).map(String::from))
            .collect();
        let session_id = uuid::Uuid::new_v4().to_string();
        let start = client
            .call_tool("start_session", json!({"session": session_id}))
            .await;
        if let Err(e) = start {
            let msg = e.to_string();
            // Older drivers without session support still serve input tools.
            if !msg.contains("not found") && !msg.contains("unknown tool") {
                return Err(AgentError::Tool(format!("start_session failed: {msg}")));
            }
        }
        if cfg.max_image_dimension > 0 && tools.iter().any(|t| t == "set_config") {
            client
                .call_tool(
                    "set_config",
                    json!({
                        "session": session_id,
                        "max_image_dimension": cfg.max_image_dimension,
                    }),
                )
                .await
                .ok();
        }
        if no_overlay_resolved(cfg) && tools.iter().any(|t| t == "set_agent_cursor_enabled") {
            client
                .call_tool(
                    "set_agent_cursor_enabled",
                    json!({"session": session_id, "enabled": false}),
                )
                .await
                .ok();
        }
        *guard = Some(CuaSession {
            client,
            session_id,
            active_pid: None,
            active_window_id: None,
            active_app: String::new(),
            active_title: String::new(),
            tools,
        });
    }
    let session = guard.as_mut().expect("session just populated");
    f(session).await
}

/// Flatten an MCP tool result into the cua payload object.
/// MCP content items: {type: text, text: "<json>"} or {type: image, data}.
fn unwrap_mcp_result(result: &Value) -> Value {
    if let Some(content) = result.get("content").and_then(|v| v.as_array()) {
        for item in content {
            match item.get("type").and_then(|v| v.as_str()) {
                Some("text") => {
                    if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                        if let Ok(parsed) = serde_json::from_str::<Value>(text) {
                            return parsed;
                        }
                        return json!({"message": text});
                    }
                }
                Some("image") => {
                    return json!({
                        "screenshot_png_b64": item.get("data").cloned().unwrap_or(Value::Null),
                    });
                }
                _ => {}
            }
        }
    }
    result.clone()
}

/// Extract an `[x, y]` coordinate array argument (hermes schema
/// `coordinate` / `from_coordinate` / `to_coordinate`).
fn coord(args: &Value, key: &str) -> Option<(i64, i64)> {
    let arr = args.get(key)?.as_array()?;
    if arr.len() != 2 {
        return None;
    }
    Some((arr[0].as_i64()?, arr[1].as_i64()?))
}

/// One action outcome (hermes `_action_result_from`): normalize the many
/// cua-driver result spellings into {ok, action, message, ...}.
fn action_result(action: &str, payload: Value) -> Value {
    let pick = |keys: &[&str]| -> Option<Value> {
        for key in keys {
            if let Some(v) = payload.get(key) {
                if !v.is_null() {
                    return Some(v.clone());
                }
            }
        }
        None
    };
    let ok = pick(&["ok", "success"])
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let message = pick(&["message", "error", "detail"])
        .map(|v| match v {
            Value::String(s) => s,
            other => other.to_string(),
        })
        .unwrap_or_default();
    let mut out = json!({"ok": ok, "action": action});
    if !message.is_empty() {
        out["message"] = json!(message);
    }
    for key in [
        "screenshot_png_b64",
        "screenshot_file_path",
        "elements",
        "tree",
        "width",
        "height",
        "app",
        "window_title",
        "windows",
        "apps",
        "pid",
        "window_id",
    ] {
        if let Some(v) = payload.get(key) {
            if !v.is_null() {
                out[key] = v.clone();
            }
        }
    }
    out
}

/// Client-side longest-edge enforcement for driver screenshots — the
/// second layer over the `set_config max_image_dimension` cap applied at
/// session start. Drivers that honor `set_config` already return capped
/// PNGs and this stays a cheap no-op (dimension check after decode);
/// drivers that ignore it (old builds, forks) get downscaled here so
/// SOM/vision payloads stay bounded regardless. Aspect ratio is
/// preserved; `width`/`height` fields are refreshed when present.
/// Fail-open: any decode or encode problem leaves the payload untouched.
fn enforce_screenshot_dimension(result: &mut Value, max_dimension: u32) {
    if max_dimension == 0 {
        return;
    }
    let Some(b64) = result
        .get("screenshot_png_b64")
        .and_then(|v| v.as_str())
        .map(str::to_string)
    else {
        return;
    };
    use base64::Engine as _;
    let engine = base64::engine::general_purpose::STANDARD;
    let Ok(bytes) = engine.decode(b64.as_bytes()) else {
        return;
    };
    let Ok(img) = image::load_from_memory(&bytes) else {
        return;
    };
    let longest = img.width().max(img.height());
    if longest <= max_dimension {
        return;
    }
    let resized = img.resize(
        max_dimension,
        max_dimension,
        image::imageops::FilterType::Triangle,
    );
    let mut buffer = Vec::new();
    if resized
        .write_to(
            &mut std::io::Cursor::new(&mut buffer),
            image::ImageFormat::Png,
        )
        .is_err()
    {
        return;
    }
    result["screenshot_png_b64"] = json!(engine.encode(&buffer));
    if result.get("width").is_some() {
        result["width"] = json!(resized.width());
    }
    if result.get("height").is_some() {
        result["height"] = json!(resized.height());
    }
}

/// The `computer_use` tool schema (hermes `schema.py`
/// `COMPUTER_USE_SCHEMA`), parsed from a static string because the literal
/// is too large for the `json!` macro's recursion budget.
pub fn tool_schema() -> Value {
    static SCHEMA: &str = r###"{
  "name": "computer_use",
  "description": "Drive the desktop in the background via cua-driver — screenshots, mouse, keyboard, scroll, drag — without stealing the user's cursor or keyboard focus. Supported on macOS, Windows, and Linux. Preferred workflow: call with action='capture' (mode='som' gives numbered element overlays), then click by `element` index for reliability. Pixel coordinates are supported for models trained on them. Works on any window — hidden, minimized, or behind another app. Requires cua-driver to be installed.",
  "parameters": {
    "type": "object",
    "properties": {
      "action": {
        "type": "string",
        "enum": [
          "capture",
          "click",
          "double_click",
          "right_click",
          "middle_click",
          "drag",
          "scroll",
          "type",
          "key",
          "set_value",
          "wait",
          "list_apps",
          "list_windows",
          "focus_app",
          "cua_browser_state",
          "cua_browser_prepare",
          "cua_browser_navigate",
          "cua_browser_click",
          "cua_browser_type",
          "cua_browser_pointer",
          "cua_browser_dialog",
          "cua_browser_set_input_files",
          "cua_browser_download"
        ],
        "description": "Which action to perform. `capture` is free (no side effects). All other actions require approval unless auto-approved. Use `set_value` for select/popup elements and sliders — it selects the matching option directly without opening the native menu (no focus steal)."
      },
      "mode": {
        "type": "string",
        "enum": [
          "som",
          "vision",
          "ax"
        ],
        "description": "Capture mode. `som` (default) is a screenshot with numbered overlays on every interactable element plus the AX tree — best for vision models, lets you click by element index. `vision` is a plain screenshot. `ax` is the accessibility tree only (no image; useful for text-only models)."
      },
      "app": {
        "type": "string",
        "description": "Optional. Limit capture/action to a specific app (by name, e.g. 'Safari', or bundle ID, 'com.apple.Safari'). If omitted, operates on the frontmost app's window. Pass app='screen' (or 'desktop') to capture the OS desktop/shell surface — e.g. to see the wallpaper or click the taskbar. Note: capture is per-window; a single image cannot span multiple monitors, so on a multi-screen setup capture one window or display at a time."
      },
      "pid": {
        "type": "integer",
        "description": "Optional exact process target for action='capture'. Pair with window_id when discovery cannot resolve an X11 app."
      },
      "window_id": {
        "type": "integer",
        "description": "Optional exact native window target for action='capture'. Pair with pid when an external cua-driver list_windows lookup has already identified the window."
      },
      "max_elements": {
        "type": "integer",
        "description": "Optional cap on the AX `elements` array returned by `action='capture'`. Default 100, hard maximum 1000. Dense UIs (Electron apps such as Obsidian or VS Code, JetBrains IDEs) can publish 500+ AX nodes — capping prevents a single capture from blowing session context. When the cap trims the response, `total_elements` and `truncated_elements` are surfaced in the result so you can re-call with `app=` to narrow scope or raise `max_elements` when the full tree is required. Has no effect on `mode='som'` / `mode='vision'` when a screenshot is included in the response; only the rare image-missing fallback returns an `elements` array and is subject to the cap.",
        "default": 100,
        "minimum": 1,
        "maximum": 1000
      },
      "element": {
        "type": "integer",
        "description": "The 1-based SOM index returned by the last `capture(mode='som')` call. Strongly preferred over raw coordinates."
      },
      "coordinate": {
        "type": "array",
        "items": {
          "type": "integer"
        },
        "minItems": 2,
        "maxItems": 2,
        "description": "Pixel coordinates [x, y] relative to the captured window screenshot (top-left origin). Only use this if no element index is available."
      },
      "button": {
        "type": "string",
        "enum": [
          "left",
          "right",
          "middle"
        ],
        "description": "Mouse button. Defaults to left."
      },
      "modifiers": {
        "type": "array",
        "items": {
          "type": "string",
          "enum": [
            "cmd",
            "shift",
            "option",
            "alt",
            "ctrl",
            "fn",
            "win",
            "windows",
            "super",
            "meta"
          ]
        },
        "description": "Modifier keys held during the action."
      },
      "from_element": {
        "type": "integer",
        "description": "Source element index (drag)."
      },
      "to_element": {
        "type": "integer",
        "description": "Target element index (drag)."
      },
      "from_coordinate": {
        "type": "array",
        "items": {
          "type": "integer"
        },
        "minItems": 2,
        "maxItems": 2,
        "description": "Source [x,y] (drag; use when no element available)."
      },
      "to_coordinate": {
        "type": "array",
        "items": {
          "type": "integer"
        },
        "minItems": 2,
        "maxItems": 2,
        "description": "Target [x,y] (drag; use when no element available)."
      },
      "direction": {
        "type": "string",
        "enum": [
          "up",
          "down",
          "left",
          "right"
        ],
        "description": "Scroll direction."
      },
      "amount": {
        "type": "integer",
        "description": "Scroll wheel ticks. Default 3."
      },
      "value": {
        "type": "string",
        "description": "For action='set_value': the value to set on the element. For AXPopUpButton / select dropdowns, pass the option's display label (e.g. 'Blue'). For sliders and other AXValue-settable elements, pass the numeric or string value."
      },
      "text": {
        "type": "string",
        "description": "Text to type (respects the current layout)."
      },
      "keys": {
        "type": "string",
        "description": "Key combo, e.g. 'cmd+s', 'ctrl+alt+t', 'return', 'escape', 'tab'. Use '+' to combine."
      },
      "seconds": {
        "type": "number",
        "description": "Seconds to wait. Max 30."
      },
      "raise_window": {
        "type": "boolean",
        "description": "Only for action='focus_app'. If true, brings the window to front (DISRUPTS the user). Default false — input is routed to the app without raising, matching the background co-work model."
      },
      "delivery_mode": {
        "type": "string",
        "enum": [
          "background",
          "foreground"
        ],
        "description": "How input is delivered, for the input actions (click, double_click, right_click, drag, scroll, type, key). `background` (DEFAULT) routes input to the target without raising it or stealing focus — the co-work model. `foreground` briefly fronts the window, acts, then restores the prior frontmost app. A `confirmed` effect is done. For `unverifiable`, inspect fresh state before any retry even if escalation is recommended. Escalate only after `suspected_noop` or a structured refusal. Do not predict the rung from the app being Electron/Chromium. Foreground is a visible focus change and needs its own approval."
      },
      "bring_to_front": {
        "type": "boolean",
        "description": "Optional and only valid with delivery_mode='foreground'. Explicitly invokes cua-driver's standalone bring_to_front tool before the input; it is never passed as an input property. This persistent focus change has a separate approval scope. Default false."
      },
      "tab_id": {
        "type": "string",
        "description": "Opaque tab capability returned by cua_browser_state."
      },
      "ref": {
        "type": "string",
        "description": "Current semantic ref from the latest cua_browser_state snapshot."
      },
      "destination_ref": {
        "type": "string",
        "description": "Current destination ref for a typed pointer action."
      },
      "url": {
        "type": "string",
        "description": "URL for cua_browser_navigate."
      },
      "input_route": {
        "type": "string",
        "enum": [
          "trusted",
          "dom_event"
        ],
        "description": "Typed-browser trust class. Defaults to trusted. dom_event is an explicit downgrade and is never selected silently."
      },
      "snapshot_format": {
        "type": "string",
        "enum": [
          "semantic_v2",
          "dom_refs_v1"
        ],
        "description": "Typed-browser snapshot format; semantic_v2 is the default."
      },
      "query": {
        "type": "string",
        "description": "Optional browser-state query."
      },
      "scope_ref": {
        "type": "string",
        "description": "Optional current ref to scope a snapshot."
      },
      "continuation": {
        "type": "string",
        "description": "Continuation minted by the current snapshot."
      },
      "profile_mode": {
        "type": "string",
        "enum": [
          "isolated_new",
          "isolated_named",
          "existing_profile"
        ],
        "description": "Browser preparation mode. existing_profile is decided by cua-driver's immutable permission mode: standard requires a certified protected host; explicit Hermes YOLO uses a private unrestricted daemon."
      },
      "profile_name": {
        "type": "string",
        "description": "Name for isolated_named setup."
      },
      "allow_launch": {
        "type": "boolean",
        "description": "Explicitly allow launch of a driver-owned isolated browser."
      },
      "browser_pointer_action": {
        "type": "string",
        "enum": [
          "hover",
          "right_click",
          "double_click",
          "scroll",
          "drag"
        ],
        "description": "Operation for cua_browser_pointer."
      },
      "browser_dialog_action": {
        "type": "string",
        "enum": [
          "inspect",
          "accept",
          "dismiss"
        ],
        "description": "Page JavaScript dialog action; native prompts stay on the native ladder."
      },
      "browser_type_mode": {
        "type": "string",
        "enum": [
          "insert_text",
          "keystrokes"
        ],
        "description": "Delivery form for cua_browser_type; defaults to insert_text."
      },
      "dialog_id": {
        "type": "string",
        "description": "Opaque page-dialog capability."
      },
      "prompt_text": {
        "type": "string",
        "description": "Optional text for a page prompt dialog."
      },
      "files": {
        "type": "array",
        "items": {
          "type": "string"
        },
        "description": "Explicit paths for cua_browser_set_input_files."
      },
      "destination_root": {
        "type": "string",
        "description": "Approved destination root for cua_browser_download."
      },
      "delta_x": {
        "type": "number",
        "description": "Typed pointer horizontal delta."
      },
      "delta_y": {
        "type": "number",
        "description": "Typed pointer vertical delta."
      },
      "x": {
        "type": "number",
        "description": "Typed browser viewport x coordinate."
      },
      "y": {
        "type": "number",
        "description": "Typed browser viewport y coordinate."
      },
      "to_x": {
        "type": "number",
        "description": "Typed browser drag destination x."
      },
      "to_y": {
        "type": "number",
        "description": "Typed browser drag destination y."
      },
      "capture_after": {
        "type": "boolean",
        "description": "If true, take a follow-up capture after the action and include it in the response. Saves a round-trip when you need to verify an action's effect."
      }
    },
    "required": [
      "action"
    ]
  }
}"###;
    serde_json::from_str(SCHEMA).expect("computer_use schema parses")
}

/// The `computer_use` tool handler (hermes `handle_computer_use`).
pub async fn handle_computer_use(
    args: Value,
    ctx: Arc<crate::tools::ToolContext>,
) -> Result<Value> {
    let action = args
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if action.is_empty() {
        return Ok(json!({"ok": false, "error": "computer_use requires action="}));
    }
    let cfg = ctx.config.computer_use.clone();
    if resolve_cua_driver_cmd().is_none() {
        return Ok(json!({
            "ok": false,
            "action": action,
            "error": cua_driver_install_hint(),
        }));
    }

    // Approval gate: capture is free; every other action needs a human
    // approval channel (hermes approval hook; unattended fails closed).
    if action != "capture" && action != "list_apps" && action != "list_windows" {
        let description = format!("computer_use {action}");
        let approved = match &ctx.approve {
            Some(approve) => approve(description.clone(), description).await,
            None => false,
        };
        if !approved {
            return Ok(json!({
                "ok": false,
                "action": action,
                "error": "computer_use action requires approval and no approval channel \
                          is available (interactive REPL or gateway run approval).",
            }));
        }
    }

    let outcome = match action.as_str() {
        "capture" => capture(&cfg, &args).await,
        "click" => click(&cfg, &args, 1, "left").await,
        "double_click" => click(&cfg, &args, 2, "left").await,
        "right_click" => click(&cfg, &args, 1, "right").await,
        "middle_click" => click(&cfg, &args, 1, "middle").await,
        "drag" => drag(&cfg, &args).await,
        "scroll" => scroll(&cfg, &args).await,
        "type" => type_text(&cfg, &args).await,
        "key" => key(&cfg, &args).await,
        "set_value" => set_value(&cfg, &args).await,
        "wait" => {
            let seconds = args.get("seconds").and_then(|v| v.as_f64()).unwrap_or(1.0);
            let seconds = seconds.clamp(0.0, 60.0);
            tokio::time::sleep(Duration::from_secs_f64(seconds)).await;
            Ok(json!({"ok": true, "action": "wait", "message": format!("waited {seconds}s")}))
        }
        "list_apps" => simple_call(&cfg, "list_apps", json!({})).await,
        "list_windows" => list_windows(&cfg, &args).await,
        "focus_app" => focus_app(&cfg, &args).await,
        other if other.starts_with("cua_browser_") => browser_passthrough(&cfg, other, &args).await,
        _ => Ok(
            json!({"ok": false, "action": action, "error": format!("unknown action {action:?}")}),
        ),
    };
    // Belt-and-braces over the driver-side `set_config` cap: downscale
    // oversized screenshots client-side (no-op when the driver already
    // honored the cap or the payload carries no image).
    outcome.map(|mut value| {
        enforce_screenshot_dimension(&mut value, cfg.max_image_dimension);
        value
    })
}

/// capture: get_window_state with mode som|vision|ax (hermes `capture()`).
async fn capture(cfg: &ComputerUseConfig, args: &Value) -> Result<Value> {
    let mode = args
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("som")
        .to_string();
    let app = args
        .get("app")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let window_title = args
        .get("window_title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    with_session(cfg, |session| {
        Box::pin(async move {
            let mut call = json!({
                "session": session.session_id,
                "mode": mode,
            });
            if !app.is_empty() {
                call["app"] = json!(app);
            }
            if !window_title.is_empty() {
                call["window_title"] = json!(window_title);
            }
            let result = session.client.call_tool("get_window_state", call).await?;
            let payload = unwrap_mcp_result(&result);
            // Track the active window for subsequent input actions.
            session.active_pid = payload.get("pid").and_then(|v| v.as_u64());
            session.active_window_id = payload.get("window_id").and_then(|v| v.as_u64());
            session.active_app = payload
                .get("app")
                .and_then(|v| v.as_str())
                .unwrap_or(&app)
                .to_string();
            session.active_title = payload
                .get("window_title")
                .and_then(|v| v.as_str())
                .unwrap_or(&window_title)
                .to_string();
            Ok(action_result("capture", payload))
        })
    })
    .await
}

/// click / double_click / right_click / middle_click (hermes `click()`):
/// element index or coordinates against the active window.
async fn click(
    cfg: &ComputerUseConfig,
    args: &Value,
    click_count: u32,
    button: &str,
) -> Result<Value> {
    let element = args.get("element").and_then(|v| v.as_u64());
    let (x, y) = match coord(args, "coordinate") {
        Some((x, y)) => (Some(x), Some(y)),
        None => (
            args.get("x").and_then(|v| v.as_i64()),
            args.get("y").and_then(|v| v.as_i64()),
        ),
    };
    // Schema `button` arg refines the click variant.
    let button = args
        .get("button")
        .and_then(|v| v.as_str())
        .unwrap_or(button)
        .to_string();
    let modifiers = args
        .get("modifiers")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    with_session(cfg, move |session| {
        Box::pin(async move {
            let Some(pid) = session.active_pid else {
                return Ok(json!({"ok": false, "action": "click",
                    "message": "No active window — call capture first."}));
            };
            let Some(window_id) = session.active_window_id else {
                return Ok(json!({"ok": false, "action": "click",
                    "message": "No active window_id — call capture first."}));
            };
            let tool = if click_count == 2 {
                "double_click"
            } else {
                "click"
            };
            let mut call = json!({"session": session.session_id, "pid": pid,
                "window_id": window_id, "button": button.as_str()});
            if let Some(element) = element {
                call["element_index"] = json!(element);
            } else if let (Some(x), Some(y)) = (x, y) {
                call["x"] = json!(x);
                call["y"] = json!(y);
            } else {
                return Ok(json!({"ok": false, "action": tool,
                    "message": "click requires element= or x/y."}));
            }
            if !modifiers.is_empty() {
                call["modifier"] = json!(modifiers);
            }
            let result = session.client.call_tool(tool, call).await?;
            Ok(action_result(tool, unwrap_mcp_result(&result)))
        })
    })
    .await
}

/// drag: from/to element indices or coordinates (hermes `drag()`).
async fn drag(cfg: &ComputerUseConfig, args: &Value) -> Result<Value> {
    let from_element = args.get("from_element").and_then(|v| v.as_u64());
    let to_element = args.get("to_element").and_then(|v| v.as_u64());
    let (from_x, from_y) = match coord(args, "from_coordinate") {
        Some((x, y)) => (Some(x), Some(y)),
        None => (
            args.get("from_x").and_then(|v| v.as_i64()),
            args.get("from_y").and_then(|v| v.as_i64()),
        ),
    };
    let (to_x, to_y) = match coord(args, "to_coordinate") {
        Some((x, y)) => (Some(x), Some(y)),
        None => (
            args.get("to_x").and_then(|v| v.as_i64()),
            args.get("to_y").and_then(|v| v.as_i64()),
        ),
    };
    with_session(cfg, move |session| {
        Box::pin(async move {
            let Some(pid) = session.active_pid else {
                return Ok(json!({"ok": false, "action": "drag",
                    "message": "No active window — call capture first."}));
            };
            let Some(window_id) = session.active_window_id else {
                return Ok(json!({"ok": false, "action": "drag",
                    "message": "No active window_id — call capture first."}));
            };
            let mut call = json!({"session": session.session_id, "pid": pid,
                "window_id": window_id});
            if let (Some(from), Some(to)) = (from_element, to_element) {
                call["from_element_index"] = json!(from);
                call["to_element_index"] = json!(to);
            } else if let (Some(fx), Some(fy), Some(tx), Some(ty)) = (from_x, from_y, to_x, to_y) {
                call["from_x"] = json!(fx);
                call["from_y"] = json!(fy);
                call["to_x"] = json!(tx);
                call["to_y"] = json!(ty);
            } else {
                return Ok(json!({"ok": false, "action": "drag",
                    "message": "drag requires from_element/to_element or from_x/from_y/to_x/to_y."}));
            }
            let result = session.client.call_tool("drag", call).await?;
            Ok(action_result("drag", unwrap_mcp_result(&result)))
        })
    })
    .await
}

/// scroll: direction + amount, or explicit coordinates (hermes `scroll()`).
async fn scroll(cfg: &ComputerUseConfig, args: &Value) -> Result<Value> {
    let direction = args
        .get("direction")
        .and_then(|v| v.as_str())
        .unwrap_or("down")
        .to_string();
    let amount = args.get("amount").and_then(|v| v.as_i64()).unwrap_or(3);
    let (x, y) = match coord(args, "coordinate") {
        Some((x, y)) => (Some(x), Some(y)),
        None => (
            args.get("x").and_then(|v| v.as_i64()),
            args.get("y").and_then(|v| v.as_i64()),
        ),
    };
    if !["up", "down", "left", "right"].contains(&direction.as_str()) {
        return Ok(json!({"ok": false, "action": "scroll",
            "message": format!("unknown scroll direction {direction:?}")}));
    }
    with_session(cfg, move |session| {
        Box::pin(async move {
            let Some(pid) = session.active_pid else {
                return Ok(json!({"ok": false, "action": "scroll",
                    "message": "No active window — call capture first."}));
            };
            let mut call = json!({"session": session.session_id, "pid": pid,
                "direction": direction, "amount": amount});
            if let Some(window_id) = session.active_window_id {
                call["window_id"] = json!(window_id);
            }
            if let (Some(x), Some(y)) = (x, y) {
                call["x"] = json!(x);
                call["y"] = json!(y);
            }
            let result = session.client.call_tool("scroll", call).await?;
            Ok(action_result("scroll", unwrap_mcp_result(&result)))
        })
    })
    .await
}

/// type: text entry (hermes `type_text()`).
async fn type_text(cfg: &ComputerUseConfig, args: &Value) -> Result<Value> {
    let text = args
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if text.is_empty() {
        return Ok(json!({"ok": false, "action": "type", "message": "type requires text="}));
    }
    with_session(cfg, move |session| {
        Box::pin(async move {
            let Some(pid) = session.active_pid else {
                return Ok(json!({"ok": false, "action": "type",
                    "message": "No active window — call capture first."}));
            };
            let mut call = json!({"session": session.session_id, "pid": pid, "text": text});
            if let Some(window_id) = session.active_window_id {
                call["window_id"] = json!(window_id);
            }
            let result = session.client.call_tool("type_text", call).await?;
            Ok(action_result("type", unwrap_mcp_result(&result)))
        })
    })
    .await
}

/// key: single key (`press_key`) or `mod+key` combo (`hotkey`)
/// (hermes `key()`).
async fn key(cfg: &ComputerUseConfig, args: &Value) -> Result<Value> {
    let keys = args
        .get("keys")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if keys.is_empty() {
        return Ok(json!({"ok": false, "action": "key", "message": "key requires keys="}));
    }
    let parts: Vec<String> = keys
        .split('+')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if parts.is_empty() {
        return Ok(
            json!({"ok": false, "action": "key", "message": format!("unparseable key combo {keys:?}")}),
        );
    }
    let is_combo = parts.len() > 1;
    with_session(cfg, move |session| {
        Box::pin(async move {
            let Some(pid) = session.active_pid else {
                return Ok(json!({"ok": false, "action": "key",
                    "message": "No active window — call capture first."}));
            };
            let (tool, call) = if is_combo {
                (
                    "hotkey",
                    json!({"session": session.session_id, "pid": pid, "keys": parts}),
                )
            } else {
                let mut call = json!({"session": session.session_id, "pid": pid, "key": parts[0]});
                if let Some(window_id) = session.active_window_id {
                    call["window_id"] = json!(window_id);
                }
                ("press_key", call)
            };
            let result = session.client.call_tool(tool, call).await?;
            Ok(action_result("key", unwrap_mcp_result(&result)))
        })
    })
    .await
}

/// set_value: direct value setting for selects/popups/sliders
/// (hermes `set_value()`).
async fn set_value(cfg: &ComputerUseConfig, args: &Value) -> Result<Value> {
    let element = args.get("element").and_then(|v| v.as_u64());
    let value = args.get("value").cloned().unwrap_or(Value::Null);
    let Some(element) = element else {
        return Ok(json!({"ok": false, "action": "set_value",
            "message": "set_value requires element= (element index)."}));
    };
    if value.is_null() {
        return Ok(
            json!({"ok": false, "action": "set_value", "message": "set_value requires value="}),
        );
    }
    with_session(cfg, move |session| {
        Box::pin(async move {
            let Some(pid) = session.active_pid else {
                return Ok(json!({"ok": false, "action": "set_value",
                    "message": "No active window — call capture first."}));
            };
            let mut call = json!({"session": session.session_id, "pid": pid,
                "element_index": element, "value": value});
            if let Some(window_id) = session.active_window_id {
                call["window_id"] = json!(window_id);
            }
            let result = session.client.call_tool("set_value", call).await?;
            Ok(action_result("set_value", unwrap_mcp_result(&result)))
        })
    })
    .await
}

/// list_windows with light filtering (hermes `list_windows()`).
async fn list_windows(cfg: &ComputerUseConfig, args: &Value) -> Result<Value> {
    let app_filter = args
        .get("app")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();
    with_session(cfg, move |session| {
        Box::pin(async move {
            let result = session
                .client
                .call_tool("list_windows", json!({"session": session.session_id}))
                .await?;
            let mut payload = unwrap_mcp_result(&result);
            if !app_filter.is_empty() {
                if let Some(windows) = payload.get_mut("windows").and_then(|v| v.as_array_mut()) {
                    windows.retain(|w| {
                        let owner = w
                            .get("owner")
                            .or_else(|| w.get("app"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_lowercase();
                        owner.contains(&app_filter)
                    });
                }
            }
            Ok(action_result("list_windows", payload))
        })
    })
    .await
}

/// focus_app: bring an app to front without stealing cursor focus
/// (hermes `focus_app()`): match via list_apps, then `focus_app`.
async fn focus_app(cfg: &ComputerUseConfig, args: &Value) -> Result<Value> {
    let app = args
        .get("app")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if app.is_empty() {
        return Ok(
            json!({"ok": false, "action": "focus_app", "message": "focus_app requires app="}),
        );
    }
    with_session(cfg, move |session| {
        Box::pin(async move {
            let list = session
                .client
                .call_tool("list_apps", json!({"session": session.session_id}))
                .await?;
            let list_payload = unwrap_mcp_result(&list);
            let apps = list_payload
                .get("apps")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let needle = app.to_lowercase();
            let mut found: Option<Value> = None;
            for candidate in &apps {
                let name = candidate
                    .get("name")
                    .or_else(|| candidate.get("app"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_lowercase();
                let bundle = candidate
                    .get("bundle_identifier")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_lowercase();
                if name == needle
                    || bundle == needle
                    || name.contains(&needle)
                    || bundle.contains(&needle)
                {
                    found = Some(candidate.clone());
                    break;
                }
            }
            let Some(target) = found else {
                return Ok(json!({"ok": false, "action": "focus_app",
                    "message": format!("app {app:?} not found in list_apps")}));
            };
            let mut call = json!({"session": session.session_id});
            if let Some(pid) = target.get("pid") {
                call["pid"] = pid.clone();
            }
            if let Some(bundle) = target.get("bundle_identifier") {
                call["bundle_identifier"] = bundle.clone();
            }
            if let Some(name) = target.get("name") {
                call["app"] = name.clone();
            }
            let result = session.client.call_tool("focus_app", call).await?;
            let payload = unwrap_mcp_result(&result);
            if let Some(pid) = payload.get("pid").and_then(|v| v.as_u64()) {
                session.active_pid = Some(pid);
            }
            if let Some(window_id) = payload.get("window_id").and_then(|v| v.as_u64()) {
                session.active_window_id = Some(window_id);
            }
            Ok(action_result("focus_app", payload))
        })
    })
    .await
}

/// cua_browser_* actions: pass through to cua-driver's browser tools with
/// the same names (hermes browser_route.py routes the same way).
async fn browser_passthrough(cfg: &ComputerUseConfig, tool: &str, args: &Value) -> Result<Value> {
    let mut call = args.clone();
    if let Some(obj) = call.as_object_mut() {
        obj.remove("action");
    }
    let tool_name = tool.to_string();
    with_session(cfg, move |session| {
        Box::pin(async move {
            if !session.tools.iter().any(|t| *t == tool_name) {
                return Ok(json!({
                    "ok": false,
                    "action": tool_name,
                    "error": format!(
                        "cua-driver does not expose {tool_name:?} in this build \
                         (available: {} tools)",
                        session.tools.len()
                    ),
                }));
            }
            if let Some(obj) = call.as_object_mut() {
                obj.insert("session".to_string(), json!(session.session_id));
            }
            let result = session.client.call_tool(&tool_name, call).await?;
            Ok(action_result(&tool_name, unwrap_mcp_result(&result)))
        })
    })
    .await
}

/// Simple fire-and-report call (list_apps).
async fn simple_call(cfg: &ComputerUseConfig, tool: &str, call: Value) -> Result<Value> {
    let tool_name = tool.to_string();
    with_session(cfg, move |session| {
        Box::pin(async move {
            let mut call = call;
            call["session"] = json!(session.session_id);
            let result = session.client.call_tool(&tool_name, call).await?;
            Ok(action_result(&tool_name, unwrap_mcp_result(&result)))
        })
    })
    .await
}

/// Release the shared cua-driver session (hermes
/// `release_computer_use_session`): end_session + kill the MCP child.
pub async fn release_computer_use_session() {
    let cell = session_cell().await;
    let mut guard = cell.lock().await;
    if let Some(mut session) = guard.take() {
        session
            .client
            .call_tool("end_session", json!({"session": session.session_id}))
            .await
            .ok();
        session.client.close().await;
    }
}

/// `cua-driver --version` (for `computer-use status`).
pub fn driver_version(driver: &str) -> Option<String> {
    let out = std::process::Command::new(driver)
        .arg("--version")
        .output()
        .ok()?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let line = text.lines().next()?.trim().to_string();
    if line.is_empty() {
        None
    } else {
        Some(line)
    }
}

/// Run cua-driver's `health_report` MCP tool once (hermes `computer-use
/// doctor`): connect, call, disconnect. Returns the structured payload.
pub async fn health_report(cfg: &ComputerUseConfig) -> Result<Value> {
    let driver =
        resolve_cua_driver_cmd().ok_or_else(|| AgentError::Tool(cua_driver_install_hint()))?;
    let mut args = vec!["mcp".to_string()];
    if no_overlay_resolved(cfg) && driver_supports_no_overlay(&driver) {
        args.push("--no-overlay".to_string());
    }
    let server_cfg = McpServerConfig {
        name: "cua-driver".to_string(),
        command: driver,
        args,
        env: cua_driver_child_env(cfg),
        url: None,
        transport: None,
        headers: HashMap::new(),
        auth: None,
        oauth: crate::mcp::oauth::McpOAuthConfig::default(),
        lazy: false,
        enabled: true,
    };
    let mut client = McpClient::connect(&server_cfg)
        .await
        .map_err(|e| AgentError::Tool(format!("cua-driver MCP connect failed: {e}")))?;
    let result = client.call_tool("health_report", json!({})).await;
    client.close().await;
    let result = result?;
    Ok(unwrap_mcp_result(&result))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_match_hermes() {
        let cfg = ComputerUseConfig::default();
        assert!(!cfg.cua_telemetry, "telemetry off by default");
        assert_eq!(cfg.max_image_dimension, 1456);
        assert_eq!(cfg.capture_after_mode, "som");
        assert!(cfg.no_overlay.is_none());
    }

    #[test]
    fn telemetry_env_policy() {
        let mut cfg = ComputerUseConfig::default();
        let env = cua_driver_child_env(&cfg);
        assert_eq!(
            env.get(CUA_TELEMETRY_ENV_VAR).map(|s| s.as_str()),
            Some("0")
        );
        cfg.cua_telemetry = true;
        let env = cua_driver_child_env(&cfg);
        // Opt-in: the var is whatever the process env has (often absent).
        assert!(
            env.get(CUA_TELEMETRY_ENV_VAR).map(|s| s.as_str()) != Some("0")
                || std::env::var(CUA_TELEMETRY_ENV_VAR).as_deref() == Ok("0")
        );
    }

    #[test]
    fn explicit_no_overlay_wins() {
        let mut cfg = ComputerUseConfig::default();
        cfg.no_overlay = Some(true);
        assert!(no_overlay_resolved(&cfg));
        cfg.no_overlay = Some(false);
        assert!(!no_overlay_resolved(&cfg));
    }

    #[test]
    fn install_hint_mentions_installer() {
        let hint = cua_driver_install_hint();
        assert!(hint.contains("cua-driver"));
        assert!(hint.contains("install"));
    }

    #[test]
    fn unwrap_mcp_text_json() {
        let mcp = json!({"content": [{"type": "text", "text": "{\"ok\": true, \"pid\": 42}"}]});
        let payload = unwrap_mcp_result(&mcp);
        assert_eq!(payload.get("pid").and_then(|v| v.as_u64()), Some(42));
    }

    #[test]
    fn unwrap_mcp_plain_text() {
        let mcp = json!({"content": [{"type": "text", "text": "hello world"}]});
        let payload = unwrap_mcp_result(&mcp);
        assert_eq!(
            payload.get("message").and_then(|v| v.as_str()),
            Some("hello world")
        );
    }

    #[test]
    fn action_result_picks_fields() {
        let out = action_result(
            "click",
            json!({"ok": true, "message": "clicked", "pid": 7, "window_id": 9}),
        );
        assert_eq!(out["ok"], json!(true));
        assert_eq!(out["action"], json!("click"));
        assert_eq!(out["pid"], json!(7));
        let out = action_result("scroll", json!({"success": false, "error": "nope"}));
        assert_eq!(out["ok"], json!(false));
        assert_eq!(out["message"], json!("nope"));
    }

    #[tokio::test]
    async fn handle_rejects_missing_action() {
        let ctx = Arc::new(crate::tools::ToolContext::default());
        let out = handle_computer_use(json!({}), ctx).await.unwrap();
        assert_eq!(out["ok"], json!(false));
    }

    #[tokio::test]
    async fn unknown_action_reports_error() {
        // With no driver installed the handler short-circuits to the install
        // hint; either outcome must be ok=false without panicking.
        let ctx = Arc::new(crate::tools::ToolContext::default());
        let out = handle_computer_use(json!({"action": "fly"}), ctx)
            .await
            .unwrap();
        assert_eq!(out["ok"], json!(false));
    }

    // ── client-side screenshot dimension enforcement ─────────────────

    /// Encode a solid-color `w x h` PNG as base64.
    fn solid_png_b64(w: u32, h: u32) -> String {
        use base64::Engine as _;
        let img = image::RgbImage::from_pixel(w, h, image::Rgb([30, 60, 90]));
        let mut buffer = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(
                &mut std::io::Cursor::new(&mut buffer),
                image::ImageFormat::Png,
            )
            .unwrap();
        base64::engine::general_purpose::STANDARD.encode(&buffer)
    }

    fn decode_png_dims(b64: &str) -> (u32, u32) {
        use base64::Engine as _;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap();
        let img = image::load_from_memory(&bytes).unwrap();
        (img.width(), img.height())
    }

    #[test]
    fn enforce_downscales_oversized_screenshot_preserving_aspect() {
        let mut result = json!({
            "ok": true,
            "action": "capture",
            "screenshot_png_b64": solid_png_b64(2000, 1000),
            "width": 2000,
            "height": 1000,
        });
        enforce_screenshot_dimension(&mut result, 1000);
        let (w, h) = decode_png_dims(result["screenshot_png_b64"].as_str().unwrap());
        assert_eq!((w, h), (1000, 500), "longest edge capped, aspect kept");
        assert_eq!(result["width"], json!(1000));
        assert_eq!(result["height"], json!(500));
    }

    #[test]
    fn enforce_noop_when_within_cap() {
        let b64 = solid_png_b64(640, 480);
        let mut result = json!({"screenshot_png_b64": b64.clone()});
        enforce_screenshot_dimension(&mut result, 1456);
        assert_eq!(
            result["screenshot_png_b64"].as_str().unwrap(),
            b64,
            "already-capped payload passes through byte-identical"
        );
    }

    #[test]
    fn enforce_disabled_with_zero_cap() {
        let b64 = solid_png_b64(2000, 1000);
        let mut result = json!({"screenshot_png_b64": b64.clone()});
        enforce_screenshot_dimension(&mut result, 0);
        assert_eq!(result["screenshot_png_b64"].as_str().unwrap(), b64);
    }

    #[test]
    fn enforce_fail_open_on_bad_payloads() {
        // Not base64.
        let mut result = json!({"screenshot_png_b64": "!!!not-base64!!!"});
        enforce_screenshot_dimension(&mut result, 100);
        assert_eq!(result["screenshot_png_b64"], json!("!!!not-base64!!!"));
        // Base64 but not an image.
        use base64::Engine as _;
        let junk = base64::engine::general_purpose::STANDARD.encode(b"plainly not a png");
        let mut result = json!({"screenshot_png_b64": junk.clone()});
        enforce_screenshot_dimension(&mut result, 100);
        assert_eq!(result["screenshot_png_b64"], json!(junk));
        // No screenshot field at all.
        let mut result = json!({"ok": true, "action": "click"});
        enforce_screenshot_dimension(&mut result, 100);
        assert_eq!(result, json!({"ok": true, "action": "click"}));
    }

    #[test]
    fn resolve_env_override() {
        // SAFETY: test-local env mutation; other tests don't read this var.
        unsafe { std::env::set_var("ULNCLAW_CUA_DRIVER_CMD", "/tmp/fake-cua-driver") };
        assert_eq!(
            resolve_cua_driver_cmd().as_deref(),
            Some("/tmp/fake-cua-driver")
        );
        unsafe { std::env::remove_var("ULNCLAW_CUA_DRIVER_CMD") };
    }
}

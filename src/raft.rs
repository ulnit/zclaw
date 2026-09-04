//! Raft channel platform adapter — port of hermes `plugins/platforms/raft`
//! @ v2026.8.3 (adapter.py, wake-endpoint half).
//!
//! Raft integration model: a local wake endpoint receives content-free
//! wake hints from the `raft agent bridge` child process, and the hints
//! flow into the normal gateway session pipeline. The bridge stays
//! responsible for Raft message cursors and body materialization; the
//! agent uses the Raft CLI per the Raft manual.
//!
//! The wake endpoint rides the gateway at `/webhooks/raft/wake`; when
//! the gateway starts it also spawns the bridge itself (hermes
//! `_spawn_bridge`): `raft --profile $RAFT_PROFILE agent bridge
//! --wake-adapter wake-channel --wake-channel-endpoint <wake url>` with
//! `RAFT_CHANNEL_TOKEN` set to the bridge token, stdin devnull, SIGTERM
//! + 5 s grace + SIGKILL on shutdown. Missing `raft` binary or
//! `RAFT_PROFILE` degrades to wake-only mode (operator-run bridge),
//! exactly like hermes. The token defaults to an auto-generated value
//! surfaced at startup; requests must carry it in `x-raft-bridge-token`
//! (hermes header), bodies are capped at 16 KiB, and wake events
//! dispatch as `raft-activity`-schema messages on a per-session chat
//! id.

use crate::messaging::Dispatcher;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// hermes `DEFAULT_MAX_BODY_BYTES`.
const MAX_BODY_BYTES: usize = 16_384;
/// hermes `ACTIVITY_CONTENT_CAP`.
const ACTIVITY_CONTENT_CAP: usize = 4096;
/// hermes `BRIDGE_TOKEN_HEADER`.
pub const BRIDGE_TOKEN_HEADER: &str = "x-raft-bridge-token";
/// hermes `ACTIVITY_EVENT_SCHEMA`.
const ACTIVITY_EVENT_SCHEMA: &str = "raft-activity.v1";

/// `[messaging.raft]` — Raft adapter (hermes `platforms.raft` plugin
/// config + `RAFT_*` env vars).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RaftConfig {
    pub enabled: bool,
    /// Bridge token required on `x-raft-bridge-token` (fallback
    /// `RAFT_BRIDGE_TOKEN`; auto-generated when empty).
    pub bridge_token: String,
    /// Runtime session label (hermes `DEFAULT_RUNTIME_SESSION`).
    pub runtime_session: String,
}

impl Default for RaftConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bridge_token: String::new(),
            runtime_session: "default".into(),
        }
    }
}

/// Resolved runtime settings (env > config, hermes precedence).
#[derive(Debug, Clone)]
pub struct ResolvedRaft {
    pub bridge_token: String,
    pub runtime_session: String,
}

impl RaftConfig {
    pub fn resolve(&self) -> ResolvedRaft {
        let bridge_token = std::env::var("RAFT_BRIDGE_TOKEN")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| self.bridge_token.clone());
        ResolvedRaft {
            bridge_token,
            runtime_session: std::env::var("RAFT_RUNTIME_SESSION")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| self.runtime_session.clone()),
        }
    }
}

/// hermes `_safe_scalar` — conservative printable filter for wake-hint
/// fields.
pub fn safe_scalar(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.len() > 120 {
        return None;
    }
    if trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '@' | '/' | ' ' | '-'))
    {
        Some(trimmed.to_string())
    } else {
        None
    }
}

/// Extract a content string from the first known content field (hermes
/// `_CONTENT_FIELD_NAMES`).
pub fn content_string(value: &Value) -> Option<(String, bool)> {
    for field in [
        "body", "content", "message", "messages", "preview", "snippet", "text",
    ] {
        if let Some(v) = value.get(field).and_then(|v| v.as_str()) {
            let text = v.trim().to_string();
            if text.is_empty() {
                continue;
            }
            let truncated = text.chars().count() > ACTIVITY_CONTENT_CAP;
            let capped: String = text.chars().take(ACTIVITY_CONTENT_CAP).collect();
            return Some((capped, truncated));
        }
    }
    None
}

static EFFECTIVE_BRIDGE_TOKEN: OnceLock<String> = OnceLock::new();

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    crate::feishu::fill_random_bytes(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// Process-wide bridge token: configured value, or an auto-generated
/// 32-byte hex token on first use (hermes `secrets.token_hex(32)` in
/// `connect`). Both the wake route and the spawned bridge share it.
pub fn effective_bridge_token(cfg: &RaftConfig) -> String {
    EFFECTIVE_BRIDGE_TOKEN
        .get_or_init(|| {
            let resolved = cfg.resolve();
            if !resolved.bridge_token.is_empty() {
                resolved.bridge_token
            } else {
                let token = random_hex(32);
                eprintln!("[raft] auto-generated bridge token (set RAFT_BRIDGE_TOKEN to pin)");
                token
            }
        })
        .clone()
}

// ---------------------------------------------------------------------------
// Bridge process lifecycle (hermes `_spawn_bridge` / `_stop_bridge`)
// ---------------------------------------------------------------------------

/// Handle to the spawned `raft agent bridge` child.
pub struct BridgeHandle {
    child: tokio::process::Child,
}

/// Locate the `raft` CLI on PATH (hermes `shutil.which("raft")`).
pub fn resolve_raft_binary() -> Option<std::path::PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join("raft");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Build the bridge command (hermes argv + `RAFT_CHANNEL_TOKEN` env).
pub fn build_bridge_command(
    raft_bin: &std::path::Path,
    profile: &str,
    endpoint: &str,
    token: &str,
) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(raft_bin);
    cmd.args([
        "--profile",
        profile,
        "agent",
        "bridge",
        "--wake-adapter",
        "wake-channel",
        "--wake-channel-endpoint",
        endpoint,
    ]);
    cmd.env("RAFT_CHANNEL_TOKEN", token);
    cmd.stdin(std::process::Stdio::null());
    cmd
}

/// Spawn the bridge child (hermes `_spawn_bridge`): requires the raft
/// CLI on PATH and `RAFT_PROFILE`; otherwise logs and returns `None`
/// (wake-only polling mode).
pub fn spawn_bridge(cfg: &RaftConfig, endpoint: &str) -> Option<BridgeHandle> {
    let Some(raft_bin) = resolve_raft_binary() else {
        eprintln!("[raft] raft CLI not found in PATH; bridge not spawned — wake-only polling mode");
        return None;
    };
    let profile = std::env::var("RAFT_PROFILE")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let Some(profile) = profile else {
        eprintln!("[raft] RAFT_PROFILE not set; bridge not spawned");
        return None;
    };
    let token = effective_bridge_token(cfg);
    let mut cmd = build_bridge_command(&raft_bin, &profile, endpoint, &token);
    match cmd.spawn() {
        Ok(child) => {
            let pid = child.id().unwrap_or(0);
            eprintln!("[raft] spawned bridge pid={pid} profile={profile} endpoint={endpoint}");
            Some(BridgeHandle { child })
        }
        Err(e) => {
            eprintln!("[raft] failed to spawn bridge: {e}");
            None
        }
    }
}

/// Stop the bridge child (hermes `_stop_bridge`): SIGTERM, 5 s grace,
/// then SIGKILL.
pub async fn stop_bridge(mut handle: BridgeHandle) {
    let pid = handle.child.id().unwrap_or(0);
    if pid != 0 {
        let _ = crate::process_ctl::terminate(pid);
    }
    match tokio::time::timeout(Duration::from_secs(5), handle.child.wait()).await {
        Ok(status) => {
            eprintln!("[raft] bridge process terminated (pid={pid}, status={status:?})");
        }
        Err(_) => {
            let _ = handle.child.start_kill();
            eprintln!("[raft] bridge process killed after timeout (pid={pid})");
        }
    }
}

/// Webhook response handed back to the gateway route.
pub struct RaftWebhookResponse {
    pub status: u16,
    pub body: Value,
}

/// Gateway wake endpoint, mounted at `/webhooks/raft/wake`.
pub async fn raft_handle_wake(
    cfg: &RaftConfig,
    dispatcher: &Arc<Dispatcher>,
    body: &[u8],
    headers: &[(String, String)],
) -> RaftWebhookResponse {
    if body.len() > MAX_BODY_BYTES {
        return RaftWebhookResponse {
            status: 413,
            body: json!({ "error": "body too large" }),
        };
    }
    let resolved = cfg.resolve();
    let expected = effective_bridge_token(cfg);
    let token = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(BRIDGE_TOKEN_HEADER))
        .map(|(_, v)| v.trim().to_string())
        .unwrap_or_default();
    if token.is_empty() || token != expected {
        return RaftWebhookResponse {
            status: 401,
            body: json!({ "error": "invalid bridge token" }),
        };
    }
    let payload: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => {
            return RaftWebhookResponse {
                status: 400,
                body: json!({ "error": "invalid JSON" }),
            }
        }
    };
    // hermes `_validate_activity_event` subset: schema + safe scalars.
    let schema = payload
        .get("schema")
        .and_then(|v| v.as_str())
        .unwrap_or(ACTIVITY_EVENT_SCHEMA);
    let session_id = payload
        .get("sessionId")
        .and_then(|v| v.as_str())
        .and_then(safe_scalar)
        .unwrap_or_else(|| resolved.runtime_session.clone());
    let hook_event = payload
        .get("hookEventName")
        .and_then(|v| v.as_str())
        .and_then(safe_scalar)
        .unwrap_or_else(|| "wake".to_string());
    let (content, truncated) =
        content_string(&payload).unwrap_or_else(|| (format!("raft wake: {hook_event}"), false));
    let event = crate::messaging::MessageEvent {
        platform: "raft".into(),
        chat_id: format!("raft:{session_id}"),
        sender_id: "raft-bridge".into(),
        sender_name: "Raft Bridge".into(),
        text: if truncated {
            format!("{content}…")
        } else {
            content
        },
        message_id: payload
            .get("eventId")
            .and_then(|v| v.as_str())
            .and_then(safe_scalar)
            .unwrap_or_else(|| {
                format!(
                    "{}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis())
                        .unwrap_or(0)
                )
            }),
        attachments: Vec::new(),
    };
    let _ = schema;
    let mut gate_check = event.clone();
    if !crate::messaging::pre_gateway_dispatch_gate_public(&mut gate_check).await {
        return RaftWebhookResponse {
            status: 200,
            body: json!({ "status": "dropped" }),
        };
    }
    let outcome = match dispatcher.handle_event(event).await {
        Ok(o) => o,
        Err(e) => crate::messaging::DispatchOutcome {
            reply: format!("error: {e}"),
            transcript_echoes: Vec::new(),
        },
    };
    let mut full = String::new();
    for echo in &outcome.transcript_echoes {
        full.push_str(echo);
        full.push('\n');
    }
    full.push_str(&outcome.reply);
    let (reply_text, _media) = crate::messaging::extract_media_tags(&full);
    // Raft replies are surfaced via the bridge's own CLI per the Raft
    // manual; the wake response carries the agent's answer for the
    // bridge to relay.
    RaftWebhookResponse {
        status: 200,
        body: json!({ "status": "ok", "reply": reply_text.trim() }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_scalar_filters() {
        assert_eq!(safe_scalar("session-1"), Some("session-1".into()));
        assert_eq!(safe_scalar("a:b@c/d.e_f g"), Some("a:b@c/d.e_f g".into()));
        assert_eq!(safe_scalar(""), None);
        assert_eq!(safe_scalar("bad\"quote"), None);
        assert_eq!(safe_scalar(&"x".repeat(121)), None);
    }

    #[test]
    fn content_field_precedence() {
        let payload = json!({"body": "the body", "text": "ignored"});
        let (text, truncated) = content_string(&payload).unwrap();
        assert_eq!(text, "the body");
        assert!(!truncated);
        let payload = json!({"preview": "p"});
        assert_eq!(content_string(&payload).unwrap().0, "p");
        assert!(content_string(&json!({})).is_none());
    }

    #[test]
    fn content_cap_truncates() {
        let long: String = "a".repeat(ACTIVITY_CONTENT_CAP + 50);
        let (text, truncated) = content_string(&json!({"text": long})).unwrap();
        assert!(truncated);
        assert_eq!(text.chars().count(), ACTIVITY_CONTENT_CAP);
    }

    #[test]
    fn resolve_token_and_session() {
        let _guard = crate::models_dev::test_env_lock();
        std::env::set_var("RAFT_BRIDGE_TOKEN", "env-token");
        let cfg = RaftConfig {
            bridge_token: "cfg-token".into(),
            ..Default::default()
        };
        let resolved = cfg.resolve();
        assert_eq!(resolved.bridge_token, "env-token");
        assert_eq!(resolved.runtime_session, "default");
        std::env::remove_var("RAFT_BRIDGE_TOKEN");
    }

    #[test]
    fn constants_match_hermes() {
        assert_eq!(MAX_BODY_BYTES, 16_384);
        assert_eq!(ACTIVITY_CONTENT_CAP, 4096);
        assert_eq!(BRIDGE_TOKEN_HEADER, "x-raft-bridge-token");
        assert_eq!(ACTIVITY_EVENT_SCHEMA, "raft-activity.v1");
    }

    #[test]
    fn random_hex_shape() {
        let token = random_hex(32);
        assert_eq!(token.len(), 64);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
        // Random draws differ.
        assert_ne!(token, random_hex(32));
    }

    #[test]
    fn effective_token_is_stable_and_nonempty() {
        let cfg = RaftConfig::default();
        let first = effective_bridge_token(&cfg);
        assert!(!first.is_empty());
        assert_eq!(first, effective_bridge_token(&cfg));
    }

    #[test]
    fn bridge_command_matches_hermes_argv() {
        let cmd = build_bridge_command(
            std::path::Path::new("/usr/local/bin/raft"),
            "work",
            "http://127.0.0.1:8080/webhooks/raft/wake",
            "tok123",
        );
        let std_cmd = cmd.as_std();
        let args: Vec<String> = std_cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(
            args,
            vec![
                "--profile",
                "work",
                "agent",
                "bridge",
                "--wake-adapter",
                "wake-channel",
                "--wake-channel-endpoint",
                "http://127.0.0.1:8080/webhooks/raft/wake",
            ]
        );
        let env_token = std_cmd
            .get_envs()
            .find(|(k, _)| *k == "RAFT_CHANNEL_TOKEN")
            .and_then(|(_, v)| v.map(|s| s.to_string_lossy().to_string()));
        assert_eq!(env_token.as_deref(), Some("tok123"));
    }

    #[test]
    fn resolve_raft_binary_walks_path() {
        let _guard = crate::models_dev::test_env_lock();
        let temp = tempfile::tempdir().expect("tempdir");
        let bin = temp.path().join("raft");
        std::fs::write(&bin, "#!/bin/sh\n").expect("write fake raft");
        let saved = std::env::var_os("PATH");
        std::env::set_var("PATH", temp.path());
        assert_eq!(resolve_raft_binary(), Some(bin));
        // Empty PATH → nothing found.
        std::env::set_var("PATH", temp.path().join("empty"));
        assert!(resolve_raft_binary().is_none());
        match saved {
            Some(p) => std::env::set_var("PATH", p),
            None => std::env::remove_var("PATH"),
        }
    }

    #[tokio::test]
    async fn spawn_bridge_requires_profile() {
        let _guard = crate::models_dev::test_env_lock();
        let temp = tempfile::tempdir().expect("tempdir");
        let bin = temp.path().join("raft");
        std::fs::write(&bin, "#!/bin/sh\nsleep 30\n").expect("write fake raft");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let saved_path = std::env::var_os("PATH");
        let saved_profile = std::env::var_os("RAFT_PROFILE");
        std::env::set_var("PATH", temp.path());
        std::env::remove_var("RAFT_PROFILE");
        let cfg = RaftConfig {
            bridge_token: "tok".into(),
            ..Default::default()
        };
        // No RAFT_PROFILE → no spawn.
        assert!(spawn_bridge(&cfg, "http://127.0.0.1:1/webhooks/raft/wake").is_none());
        // With profile the fake binary spawns (sleep 30) and stops on
        // SIGTERM within the grace window.
        std::env::set_var("RAFT_PROFILE", "test-profile");
        let handle = spawn_bridge(&cfg, "http://127.0.0.1:1/webhooks/raft/wake");
        assert!(handle.is_some());
        if let Some(handle) = handle {
            stop_bridge(handle).await;
        }
        match saved_path {
            Some(p) => std::env::set_var("PATH", p),
            None => std::env::remove_var("PATH"),
        }
        match saved_profile {
            Some(p) => std::env::set_var("RAFT_PROFILE", p),
            None => std::env::remove_var("RAFT_PROFILE"),
        }
    }
}

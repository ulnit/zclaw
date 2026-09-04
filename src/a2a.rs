//! A2A inbound platform adapter — port of hermes `plugins/platforms/a2a`
//! @ v2026.8.3 (adapter.py core).
//!
//! Exposes ulnclaw as an A2A v1.0-discoverable agent on the gateway:
//! `GET /.well-known/agent-card.json` (plus legacy `agent.json`) serves
//! the Agent Card, and `POST /a2a` speaks JSON-RPC 2.0 — `message/send`
//! dispatches the inbound message parts through the normal agent
//! pipeline and returns a completed task with the reply as an artifact,
//! `tasks/get`/`tasks/list` inspect recent tasks, `tasks/cancel` marks
//! them cancelled. `message/stream` (SSE) and push-notification
//! config are acknowledged with capability-false JSON-RPC errors
//! (hermes serves them; documented divergence).
//!
//! hermes runs a dedicated ThreadingHTTPServer on `A2A_PORT` (default
//! 9900); ulnclaw mounts the same surface on the gateway router.
//! Requests are capped at 1 MiB (hermes `_MAX_BODY`) and optionally
//! bearer-gated with `A2A_TOKEN`.

use crate::messaging::Dispatcher;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

/// hermes `_MAX_BODY`.
const MAX_BODY_BYTES: usize = 1_048_576;
/// Bound on the in-memory task ledger.
const TASK_LEDGER_CAP: usize = 100;

/// `[messaging.a2a]` — A2A adapter (hermes `platforms.a2a` plugin
/// config + `A2A_*` env vars).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct A2aConfig {
    pub enabled: bool,
    /// Advertised agent name (fallback `A2A_AGENT_NAME`, default
    /// `ulnclaw-agent`).
    pub agent_name: String,
    /// Agent card description (fallback `A2A_AGENT_DESCRIPTION`).
    pub agent_description: String,
    /// Public URL advertised in the card (fallback `A2A_PUBLIC_URL`).
    pub public_url: String,
    /// Optional bearer token required on all requests (fallback
    /// `A2A_TOKEN`). Empty = open.
    pub token: String,
}

impl Default for A2aConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            agent_name: String::new(),
            agent_description: String::new(),
            public_url: String::new(),
            token: String::new(),
        }
    }
}

/// Resolved runtime settings (env > config, hermes precedence).
#[derive(Debug, Clone)]
pub struct ResolvedA2a {
    pub agent_name: String,
    pub agent_description: String,
    pub public_url: String,
    pub token: String,
}

impl A2aConfig {
    pub fn resolve(&self) -> ResolvedA2a {
        ResolvedA2a {
            agent_name: std::env::var("A2A_AGENT_NAME")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| {
                    if self.agent_name.is_empty() {
                        "ulnclaw-agent".to_string()
                    } else {
                        self.agent_name.clone()
                    }
                }),
            agent_description: std::env::var("A2A_AGENT_DESCRIPTION")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| {
                    if self.agent_description.is_empty() {
                        "UlncLaw agent exposed over the A2A protocol".to_string()
                    } else {
                        self.agent_description.clone()
                    }
                }),
            public_url: std::env::var("A2A_PUBLIC_URL")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| self.public_url.clone()),
            token: std::env::var("A2A_TOKEN")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| self.token.clone()),
        }
    }
}

/// hermes `_method_info` — supported JSON-RPC methods.
pub fn method_info(method: &str) -> Option<(&'static str, bool)> {
    match method {
        "message/send" => Some(("send", false)),
        "message/stream" => Some(("stream", true)),
        "tasks/get" => Some(("get", false)),
        "tasks/list" => Some(("list", false)),
        "tasks/cancel" => Some(("cancel", false)),
        _ => None,
    }
}

/// hermes `_clean_slug` — URL-safe single-segment slug.
pub fn clean_slug(value: &str) -> String {
    let slug = value.trim().trim_matches('/');
    if slug.is_empty() || slug == "default" || slug == "root" {
        return String::new();
    }
    slug.split('/').next().unwrap_or("").to_string()
}

struct StoredTask {
    context_id: String,
    status: String,
    reply: String,
}

struct Runtime {
    cfg: ResolvedA2a,
    /// task id -> stored task (bounded ledger).
    tasks: Mutex<HashMap<String, StoredTask>>,
    order: Mutex<Vec<String>>,
}

static RUNTIME: std::sync::OnceLock<Arc<Runtime>> = std::sync::OnceLock::new();

/// Register the adapter (called from `run_messaging` when enabled).
pub fn register(cfg: &A2aConfig) {
    let runtime = Arc::new(Runtime {
        cfg: cfg.resolve(),
        tasks: Mutex::new(HashMap::new()),
        order: Mutex::new(Vec::new()),
    });
    let _ = RUNTIME.set(runtime);
}

fn runtime() -> Option<Arc<Runtime>> {
    RUNTIME.get().cloned()
}

/// A2A v1.0 Agent Card (hermes card shape).
pub fn agent_card(cfg: &ResolvedA2a) -> Value {
    json!({
        "name": cfg.agent_name,
        "description": cfg.agent_description,
        "url": cfg.public_url,
        "version": "1.0.0",
        "protocolVersion": "1.0",
        "capabilities": {
            "streaming": false,
            "pushNotifications": false,
        },
        "defaultInputModes": ["text"],
        "defaultOutputModes": ["text"],
        "skills": [],
    })
}

fn rpc_error(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn rpc_result(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// Extract concatenated text parts from an A2A message (hermes
/// message.parts handling).
pub fn extract_text_parts(message: &Value) -> String {
    let mut out = String::new();
    if let Some(parts) = message.get("parts").and_then(|v| v.as_array()) {
        for part in parts {
            let kind = part.get("kind").and_then(|v| v.as_str()).unwrap_or("");
            if kind == "text" {
                if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(text);
                }
            }
        }
    }
    out
}

/// Webhook response handed back to the gateway routes.
pub struct A2aResponse {
    pub status: u16,
    pub body: Value,
}

/// `POST /a2a` JSON-RPC entry point.
pub async fn a2a_handle_rpc(
    dispatcher: &Arc<Dispatcher>,
    body: &[u8],
    headers: &[(String, String)],
) -> A2aResponse {
    let Some(runtime) = runtime() else {
        return A2aResponse {
            status: 503,
            body: json!({ "error": "a2a adapter not registered" }),
        };
    };
    if body.len() > MAX_BODY_BYTES {
        return A2aResponse {
            status: 413,
            body: json!({ "error": "body too large" }),
        };
    }
    if !runtime.cfg.token.is_empty() {
        let auth = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        let token = auth.strip_prefix("Bearer ").unwrap_or("").trim();
        if token != runtime.cfg.token {
            return A2aResponse {
                status: 401,
                body: json!({ "error": "invalid bearer token" }),
            };
        }
    }
    let request: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => {
            return A2aResponse {
                status: 400,
                body: rpc_error(&Value::Null, -32700, "Parse error"),
            }
        }
    };
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = request.get("method").and_then(|v| v.as_str()).unwrap_or("");
    let params = request.get("params").cloned().unwrap_or(json!({}));
    let Some((verb, _streaming)) = method_info(method) else {
        return A2aResponse {
            status: 200,
            body: rpc_error(&id, -32601, "Method not found"),
        };
    };
    match verb {
        "send" => {
            let message = params.get("message").cloned().unwrap_or(json!({}));
            let text = extract_text_parts(&message);
            if text.trim().is_empty() {
                return A2aResponse {
                    status: 200,
                    body: rpc_error(&id, -32602, "message has no text parts"),
                };
            }
            let context_id = params
                .get("contextId")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| {
                    message
                        .get("contextId")
                        .and_then(|v| v.as_str())
                        .unwrap_or("default")
                        .to_string()
                });
            let message_id = message
                .get("messageId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let event = crate::messaging::MessageEvent {
                platform: "a2a".into(),
                chat_id: format!("a2a:{context_id}"),
                sender_id: "a2a-client".into(),
                sender_name: "A2A Client".into(),
                text,
                message_id: if message_id.is_empty() {
                    uuid::Uuid::new_v4().to_string()
                } else {
                    message_id
                },
                attachments: Vec::new(),
            };
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
            let reply_text = reply_text.trim().to_string();
            let task_id = format!("task-{}", uuid::Uuid::new_v4().simple());
            // Record in the ledger.
            {
                let mut tasks = runtime.tasks.lock().await;
                let mut order = runtime.order.lock().await;
                tasks.insert(
                    task_id.clone(),
                    StoredTask {
                        context_id: context_id.clone(),
                        status: "completed".into(),
                        reply: reply_text.clone(),
                    },
                );
                order.push(task_id.clone());
                while order.len() > TASK_LEDGER_CAP {
                    if let Some(old) = order.first().cloned() {
                        order.remove(0);
                        tasks.remove(&old);
                    }
                }
            }
            let result = json!({
                "kind": "task",
                "id": task_id,
                "contextId": context_id,
                "status": { "state": "completed" },
                "artifacts": [{
                    "artifactId": format!("{task_id}-artifact"),
                    "parts": [{ "kind": "text", "text": reply_text }],
                }],
            });
            A2aResponse {
                status: 200,
                body: rpc_result(&id, result),
            }
        }
        "get" => {
            let task_id = params
                .get("id")
                .or_else(|| params.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let tasks = runtime.tasks.lock().await;
            match tasks.get(task_id) {
                Some(task) => A2aResponse {
                    status: 200,
                    body: rpc_result(
                        &id,
                        json!({
                            "kind": "task",
                            "id": task_id,
                            "contextId": task.context_id,
                            "status": { "state": task.status },
                            "artifacts": [{
                                "artifactId": format!("{task_id}-artifact"),
                                "parts": [{ "kind": "text", "text": task.reply }],
                            }],
                        }),
                    ),
                },
                None => A2aResponse {
                    status: 200,
                    body: rpc_error(&id, -32001, "Task not found"),
                },
            }
        }
        "list" => {
            let order = runtime.order.lock().await;
            let tasks = runtime.tasks.lock().await;
            let items: Vec<Value> = order
                .iter()
                .rev()
                .filter_map(|tid| {
                    tasks.get(tid).map(|t| {
                        json!({
                            "id": tid,
                            "contextId": t.context_id,
                            "status": { "state": t.status },
                        })
                    })
                })
                .collect();
            A2aResponse {
                status: 200,
                body: rpc_result(&id, json!({ "tasks": items })),
            }
        }
        "cancel" => {
            let task_id = params
                .get("id")
                .or_else(|| params.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let mut tasks = runtime.tasks.lock().await;
            match tasks.get_mut(task_id) {
                Some(task) => {
                    task.status = "canceled".into();
                    A2aResponse {
                        status: 200,
                        body: rpc_result(
                            &id,
                            json!({ "kind": "task", "id": task_id, "status": { "state": "canceled" } }),
                        ),
                    }
                }
                None => A2aResponse {
                    status: 200,
                    body: rpc_error(&id, -32001, "Task not found"),
                },
            }
        }
        "stream" => A2aResponse {
            status: 200,
            body: rpc_error(
                &id,
                -32601,
                "message/stream not supported (streaming capability false)",
            ),
        },
        _ => A2aResponse {
            status: 200,
            body: rpc_error(&id, -32601, "Method not found"),
        },
    }
}

/// `GET /.well-known/agent-card.json` handler.
pub fn a2a_agent_card_response() -> Option<Value> {
    runtime().map(|rt| agent_card(&rt.cfg))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_table() {
        assert_eq!(method_info("message/send"), Some(("send", false)));
        assert_eq!(method_info("message/stream"), Some(("stream", true)));
        assert_eq!(method_info("tasks/get"), Some(("get", false)));
        assert_eq!(method_info("tasks/list"), Some(("list", false)));
        assert_eq!(method_info("tasks/cancel"), Some(("cancel", false)));
        assert_eq!(method_info("tasks/subscribe"), None);
    }

    #[test]
    fn slug_cleaning() {
        assert_eq!(clean_slug("myagent"), "myagent");
        assert_eq!(clean_slug("/myagent/extra"), "myagent");
        assert_eq!(clean_slug("default"), "");
        assert_eq!(clean_slug("root"), "");
        assert_eq!(clean_slug("  "), "");
    }

    #[test]
    fn text_part_extraction() {
        let message = json!({
            "role": "user",
            "parts": [
                { "kind": "text", "text": "hello" },
                { "kind": "file", "file": {} },
                { "kind": "text", "text": "world" },
            ],
        });
        assert_eq!(extract_text_parts(&message), "hello\nworld");
        assert_eq!(extract_text_parts(&json!({"parts": []})), "");
    }

    #[test]
    fn agent_card_shape() {
        let cfg = ResolvedA2a {
            agent_name: "test-agent".into(),
            agent_description: "desc".into(),
            public_url: "https://example.com/a2a".into(),
            token: String::new(),
        };
        let card = agent_card(&cfg);
        assert_eq!(card["name"], "test-agent");
        assert_eq!(card["protocolVersion"], "1.0");
        assert_eq!(card["capabilities"]["streaming"], false);
        assert_eq!(card["capabilities"]["pushNotifications"], false);
    }

    #[test]
    fn resolve_env_overrides() {
        let _guard = crate::models_dev::test_env_lock();
        std::env::set_var("A2A_AGENT_NAME", "env-agent");
        std::env::set_var("A2A_TOKEN", "secret");
        let cfg = A2aConfig {
            agent_name: "cfg-agent".into(),
            ..Default::default()
        };
        let resolved = cfg.resolve();
        assert_eq!(resolved.agent_name, "env-agent");
        assert_eq!(resolved.token, "secret");
        std::env::remove_var("A2A_AGENT_NAME");
        std::env::remove_var("A2A_TOKEN");
    }

    #[test]
    fn rpc_envelope_shapes() {
        let err = rpc_error(&json!(1), -32601, "Method not found");
        assert_eq!(err["jsonrpc"], "2.0");
        assert_eq!(err["error"]["code"], -32601);
        let ok = rpc_result(&json!("x"), json!({"a": 1}));
        assert_eq!(ok["id"], "x");
        assert_eq!(ok["result"]["a"], 1);
    }

    #[test]
    fn constants_match_hermes() {
        assert_eq!(MAX_BODY_BYTES, 1_048_576);
    }
}

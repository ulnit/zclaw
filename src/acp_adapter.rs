//! ACP adapter (P261) — port of hermes `acp_adapter/`: run ulnclaw as an
//! Agent Client Protocol (ACP) stdio server so editors like Zed can drive
//! it natively (`ulnclaw acp`).
//!
//! Implemented surface (hermes `server.py` core):
//! - `initialize` — protocol version + capabilities (load_session,
//!   prompt image support)
//! - `authenticate` — no auth methods advertised (local credentials)
//! - `session/new` / `session/load` — per-editor conversations with
//!   history persistence in `state.db` (`acp-<sessionId>` keys) and
//!   replay on load
//! - `session/prompt` — runs a full agent turn, streaming
//!   `session/update` notifications: `agent_message_chunk`,
//!   `agent_thought_chunk`, `tool_call` / `tool_call_update`, and `plan`
//!   updates from the `todo` tool (hermes events.py parity)
//! - `session/cancel` — cooperative cancellation flag
//! - `session/set_mode` / `session/set_model` — acknowledged no-ops
//! - Tool approvals ride `session/request_permission` (hermes
//!   edit_approval parity): Allow Once / Always Allow / Reject
//!
//! Transport: newline-delimited JSON-RPC 2.0, bidirectional (the server
//! also issues requests/notifications toward the client).

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub const PROTOCOL_VERSION: u64 = 1;

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

type StdoutWriter = tokio::io::BufWriter<tokio::io::Stdout>;

/// Bidirectional JSON-RPC state shared across inbound/outbound tasks.
pub struct ServerState {
    sessions: Mutex<HashMap<String, Arc<AcpSession>>>,
    agent: tokio::sync::OnceCell<Arc<crate::agent::Agent>>,
    next_request_id: AtomicU64,
    /// Outbound requests awaiting a client response (id → responder).
    pending_client_requests: Mutex<HashMap<u64, tokio::sync::oneshot::Sender<Value>>>,
    writer: Arc<tokio::sync::Mutex<StdoutWriter>>,
    verbose: bool,
}

pub struct AcpSession {
    pub id: String,
    pub cwd: String,
    history: tokio::sync::Mutex<Vec<crate::provider::Message>>,
    cancelled: AtomicBool,
    running: AtomicBool,
}

impl ServerState {
    fn new(verbose: bool) -> Arc<Self> {
        Arc::new(Self {
            sessions: Mutex::new(HashMap::new()),
            agent: tokio::sync::OnceCell::new(),
            next_request_id: AtomicU64::new(1),
            pending_client_requests: Mutex::new(HashMap::new()),
            writer: Arc::new(tokio::sync::Mutex::new(tokio::io::BufWriter::new(
                tokio::io::stdout(),
            ))),
            verbose,
        })
    }

    async fn send(&self, message: Value) {
        use tokio::io::AsyncWriteExt;
        let mut raw = serde_json::to_string(&message).unwrap_or_default();
        raw.push('\n');
        if self.verbose {
            eprintln!("[acp] -> {}", raw.trim_end_matches('\n'));
        }
        let mut writer = self.writer.lock().await;
        if writer.write_all(raw.as_bytes()).await.is_ok() {
            let _ = writer.flush().await;
        }
    }

    async fn respond_success(&self, id: Value, result: Value) {
        self.send(json!({"jsonrpc": "2.0", "id": id, "result": result}))
            .await;
    }

    async fn respond_error(&self, id: Value, code: i64, message: &str) {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": code, "message": message},
        }))
        .await;
    }

    async fn notify(&self, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await;
    }

    /// `session/update` notification (hermes `conn.session_update`).
    async fn session_update(&self, session_id: &str, update: Value) {
        self.notify(
            "session/update",
            json!({"sessionId": session_id, "update": update}),
        )
        .await;
    }

    /// Outbound request toward the client with response matching (hermes
    /// `session/request_permission` flow).
    async fn request_from_client(&self, method: &str, params: Value) -> Option<Value> {
        let request_id = self.next_request_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending_client_requests
            .lock()
            .unwrap()
            .insert(request_id, tx);
        self.send(json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": method,
            "params": params,
        }))
        .await;
        let result = rx.await.ok();
        self.pending_client_requests
            .lock()
            .unwrap()
            .remove(&request_id);
        result
    }
}

// ---------------------------------------------------------------------------
// Agent construction (hermes session.py per-session agent wiring)
// ---------------------------------------------------------------------------

async fn build_agent(state: Arc<ServerState>) -> Result<Arc<crate::agent::Agent>, String> {
    let config = crate::config::UlncLawConfig::load(None).unwrap_or_default();
    let api_key = config.resolve_api_key();
    let base_url = config.resolve_base_url();
    let provider: Arc<dyn crate::provider::Provider> = if config.model.provider == "anthropic" {
        let mut builder = crate::provider::anthropic::AnthropicProvider::builder()
            .endpoint(&base_url)
            .model(&config.model.model)
            .name(&config.model.provider)
            .max_retries(config.model.max_retries);
        if let Some(ref key) = api_key {
            builder = builder.api_key(key);
        }
        Arc::new(builder.build().map_err(|e| e.to_string())?)
    } else {
        let mut builder = crate::provider::openai::OpenAiProvider::builder()
            .endpoint(&base_url)
            .model(&config.model.model)
            .name(&config.model.provider)
            .max_retries(config.model.max_retries);
        if let Some(ref key) = api_key {
            builder = builder.api_key(key);
        }
        Arc::new(builder.build().map_err(|e| e.to_string())?)
    };

    let mut registry = crate::tools::ToolRegistry::new();
    crate::tools::builtin::register_builtin_tools(&mut registry);
    crate::toolsets::apply_toolset_policy(
        &mut registry,
        &config.enabled_toolsets,
        &config.disabled_toolsets,
    );

    let home = crate::config::ulnclaw_home();
    std::fs::create_dir_all(&home).ok();
    let store = Arc::new(
        crate::session::sqlite::SqliteSessionStore::open(home.join("state.db"))
            .map_err(|e| e.to_string())?,
    );

    let mut context = crate::tools::context::ToolContext::new()
        .with_home(home)
        .with_config(config.clone())
        .with_store(store.clone())
        .with_provider(provider.clone());

    // Tool approvals → ACP session/request_permission (hermes
    // edit_approval.py parity).
    let approval_state = state.clone();
    context = context.with_approve(Arc::new(move |reason, command| {
        let st = approval_state.clone();
        Box::pin(async move { request_permission(&st, &reason, &command).await })
    }));

    context.set_tool_definitions(registry.definitions());

    let agent = crate::agent::Agent::new(provider.clone(), registry).with_config(
        crate::agent::AgentConfig {
            max_iterations: config.agent.max_iterations,
            concurrent_tool_execution: config.agent.concurrent_tool_execution,
            max_concurrent_tools: config.agent.max_concurrent_tools,
            approval: config.agent.approval,
            context_budget_tokens: config.agent.context_budget_tokens,
            persist: true,
            source: "acp".to_string(),
            environment_probe: config.agent.environment_probe,
            terminal_backend: config
                .terminal
                .backend
                .clone()
                .unwrap_or_else(|| "local".to_string()),
            ..Default::default()
        },
    );
    let agent = agent
        .with_store(store)
        .with_tool_context(context)
        .with_fallback_specs(&config.model.fallbacks);
    let agent = Arc::new(agent);
    agent.wire_runners();
    Ok(agent)
}

/// hermes edit_approval → ACP `session/request_permission`. Falls back to
/// deny when the client does not answer or cancels.
async fn request_permission(state: &Arc<ServerState>, reason: &str, command: &str) -> bool {
    // Resolve the active session (approvals fire mid-turn; attribute to
    // the running session when unambiguous).
    let session_id = {
        let sessions = state.sessions.lock().unwrap();
        sessions
            .values()
            .find(|session| session.running.load(Ordering::SeqCst))
            .map(|session| session.id.clone())
    };
    let Some(session_id) = session_id else {
        eprintln!("[acp] approval with no active session — denying: {command}");
        return false;
    };
    let params = json!({
        "sessionId": session_id,
        "toolCall": {
            "title": "Approve dangerous command",
            "kind": "execute",
            "rawInput": {"command": command, "reason": reason},
        },
        "options": [
            {"optionId": "allow_once", "kind": "allow_once", "name": "Allow Once"},
            {"optionId": "allow_always", "kind": "allow_always", "name": "Always Allow"},
            {"optionId": "reject_once", "kind": "reject_once", "name": "Reject"},
        ],
    });
    match state
        .request_from_client("session/request_permission", params)
        .await
    {
        Some(response) => {
            let option = response
                .pointer("/outcome/optionId")
                .and_then(Value::as_str)
                .unwrap_or("");
            matches!(option, "allow_once" | "allow_always")
        }
        None => false,
    }
}

// ---------------------------------------------------------------------------
// Prompt content extraction (hermes server.py `_extract_text` /
// `_content_blocks_to_openai_user_content`)
// ---------------------------------------------------------------------------

fn extract_prompt(prompt: &Value) -> (String, Vec<crate::provider::MessageImage>) {
    let mut text_parts: Vec<String> = Vec::new();
    let mut images: Vec<crate::provider::MessageImage> = Vec::new();
    let Some(blocks) = prompt.as_array() else {
        return (String::new(), Vec::new());
    };
    for block in blocks {
        match block.get("type").and_then(Value::as_str).unwrap_or("") {
            "text" => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    if !text.trim().is_empty() {
                        text_parts.push(text.to_string());
                    }
                }
            }
            "image" => {
                if let Some(url) = block.get("url").and_then(Value::as_str) {
                    images.push(crate::provider::MessageImage {
                        url: url.to_string(),
                        media_type: block
                            .get("mimeType")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    });
                } else if let Some(data) = block.get("data").and_then(Value::as_str) {
                    let mime = block
                        .get("mimeType")
                        .and_then(Value::as_str)
                        .unwrap_or("image/png");
                    images.push(crate::provider::MessageImage {
                        url: format!("data:{mime};base64,{data}"),
                        media_type: Some(mime.to_string()),
                    });
                }
            }
            "resource_link" | "resource" => {
                // Surface referenced resources as path pointers the agent
                // can read (hermes resource-link text fallback).
                let uri = block
                    .get("uri")
                    .and_then(Value::as_str)
                    .or_else(|| block.pointer("/resource/uri").and_then(Value::as_str))
                    .unwrap_or("");
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .or_else(|| block.pointer("/resource/name").and_then(Value::as_str))
                    .unwrap_or(uri);
                if !uri.is_empty() {
                    text_parts.push(format!("[Attached resource: {name} — {uri}]"));
                }
            }
            _ => {}
        }
    }
    (text_parts.join("\n"), images)
}

// ---------------------------------------------------------------------------
// Tool-call updates (hermes tools.py build_tool_start / build_tool_complete)
// ---------------------------------------------------------------------------

fn tool_kind(name: &str) -> &'static str {
    match name {
        "read_file" | "search_files" | "session_search" => "read",
        "write_file" | "patch" => "edit",
        "terminal" | "process" | "execute_code" => "execute",
        "web_search" | "web_extract" => "fetch",
        "todo" => "other",
        _ => "other",
    }
}

/// hermes events.py plan update: todo results render as ACP plan entries.
fn plan_update_from_todo_result(result: &Value) -> Option<Value> {
    let text = result.as_str()?;
    let data: Value = serde_json::from_str(text.trim()).ok()?;
    let todos = data.get("todos")?.as_array()?;
    let mut entries = Vec::new();
    for item in todos {
        let content = item
            .get("content")
            .or_else(|| item.get("id"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if content.is_empty() {
            continue;
        }
        let raw_status = item
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("pending");
        let (status, content) = match raw_status {
            "in_progress" => ("in_progress".to_string(), content),
            "completed" => ("completed".to_string(), content),
            "cancelled" => ("completed".to_string(), format!("[cancelled] {content}")),
            _ => ("pending".to_string(), content),
        };
        entries.push(json!({"content": content, "priority": "medium", "status": status}));
    }
    Some(json!({"sessionUpdate": "plan", "entries": entries}))
}

// ---------------------------------------------------------------------------
// Method handlers
// ---------------------------------------------------------------------------

async fn handle_initialize(state: &Arc<ServerState>, id: Value, params: Value) {
    let client_version = params.get("protocolVersion").and_then(Value::as_i64);
    let client_name = params
        .pointer("/clientInfo/name")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    eprintln!(
        "[acp] initialize from {client_name} (protocol v{})",
        client_version.unwrap_or(PROTOCOL_VERSION as i64)
    );
    state
        .respond_success(
            id,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "agentInfo": {
                    "name": "ulnclaw",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "agentCapabilities": {
                    "loadSession": true,
                    "promptCapabilities": {"image": true, "audio": false, "embeddedContext": true},
                },
                "authMethods": [],
            }),
        )
        .await;
}

async fn handle_new_session(state: &Arc<ServerState>, id: Value, params: Value) {
    let cwd = params
        .get("cwd")
        .and_then(Value::as_str)
        .unwrap_or(".")
        .to_string();
    let session_id = format!(
        "{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let session = Arc::new(AcpSession {
        id: session_id.clone(),
        cwd: cwd.clone(),
        history: tokio::sync::Mutex::new(Vec::new()),
        cancelled: AtomicBool::new(false),
        running: AtomicBool::new(false),
    });
    state
        .sessions
        .lock()
        .unwrap()
        .insert(session_id.clone(), session);
    eprintln!("[acp] new session {session_id} (cwd={cwd})");
    state
        .respond_success(id, json!({"sessionId": session_id}))
        .await;
}

/// Replay persisted history on `session/load` (hermes
/// `_replay_session_history`).
async fn handle_load_session(state: &Arc<ServerState>, id: Value, params: Value) {
    let cwd = params
        .get("cwd")
        .and_then(Value::as_str)
        .unwrap_or(".")
        .to_string();
    let session_id = params
        .get("sessionId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if session_id.is_empty() {
        state
            .respond_error(id, -32602, "sessionId is required")
            .await;
        return;
    }
    let store_key = format!("acp-{session_id}");
    let store = match crate::session::sqlite::SqliteSessionStore::open_default() {
        Ok(store) => store,
        Err(e) => {
            state
                .respond_error(id, -32603, &format!("session store unavailable: {e}"))
                .await;
            return;
        }
    };
    let messages = store.load_messages(&store_key).unwrap_or_default();
    if messages.is_empty() && store.get_session_row(&store_key).ok().flatten().is_none() {
        state
            .respond_error(id, -32602, &format!("Session not found: {session_id}"))
            .await;
        return;
    }
    let mut history: Vec<crate::provider::Message> = Vec::new();
    for message in &messages {
        let content = message.content.clone().unwrap_or_default();
        if content.trim().is_empty() {
            continue;
        }
        let update_type = match message.role {
            crate::provider::Role::User => "user_message_chunk",
            crate::provider::Role::Assistant => "agent_message_chunk",
            _ => continue,
        };
        history.push(message.clone());
        state
            .session_update(
                &session_id,
                json!({"sessionUpdate": update_type, "content": {"type": "text", "text": content}}),
            )
            .await;
    }
    let session = Arc::new(AcpSession {
        id: session_id.clone(),
        cwd,
        history: tokio::sync::Mutex::new(history),
        cancelled: AtomicBool::new(false),
        running: AtomicBool::new(false),
    });
    state
        .sessions
        .lock()
        .unwrap()
        .insert(session_id.clone(), session);
    eprintln!("[acp] loaded session {session_id}");
    state
        .respond_success(id, json!({"sessionId": session_id}))
        .await;
}

async fn handle_prompt(state: &Arc<ServerState>, id: Value, params: Value) {
    let session_id = params
        .get("sessionId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let session = state.sessions.lock().unwrap().get(&session_id).cloned();
    let Some(session) = session else {
        state
            .respond_error(id, -32602, &format!("Session not found: {session_id}"))
            .await;
        return;
    };
    let (user_text, images) = extract_prompt(&params.get("prompt").cloned().unwrap_or(json!([])));
    if user_text.trim().is_empty() && images.is_empty() {
        state
            .respond_success(id, json!({"stopReason": "end_turn"}))
            .await;
        return;
    }

    let agent = match state
        .agent
        .get_or_try_init(|| {
            let st = state.clone();
            async move { build_agent(st).await }
        })
        .await
    {
        Ok(agent) => agent.clone(),
        Err(e) => {
            state
                .respond_error(id, -32603, &format!("agent init failed: {e}"))
                .await;
            return;
        }
    };

    session.cancelled.store(false, Ordering::SeqCst);
    session.running.store(true, Ordering::SeqCst);

    // Streaming callbacks → session/update notifications (hermes
    // events.py bridge).
    let tool_call_counter = Arc::new(AtomicU64::new(1));
    let open_tool_calls: Arc<Mutex<HashMap<String, std::collections::VecDeque<String>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let update_state = state.clone();
    let update_session = session_id.clone();
    let callbacks = crate::agent::AgentCallbacks {
        on_stream_delta: Some(Box::new({
            let st = update_state.clone();
            let sid = update_session.clone();
            move |delta: &str| {
                let text = delta.to_string();
                let st = st.clone();
                let sid = sid.clone();
                tokio::spawn(async move {
                    st.session_update(
                        &sid,
                        json!({"sessionUpdate": "agent_message_chunk",
                               "content": {"type": "text", "text": text}}),
                    )
                    .await;
                });
            }
        })),
        on_thinking: Some(Box::new({
            let st = update_state.clone();
            let sid = update_session.clone();
            move || {
                let st = st.clone();
                let sid = sid.clone();
                tokio::spawn(async move {
                    st.session_update(
                        &sid,
                        json!({"sessionUpdate": "agent_thought_chunk",
                               "content": {"type": "text", "text": "thinking…"}}),
                    )
                    .await;
                });
            }
        })),
        on_tool_start: Some(Box::new({
            let st = update_state.clone();
            let sid = update_session.clone();
            let counter = tool_call_counter.clone();
            let open_calls = open_tool_calls.clone();
            move |name: &str, args: &Value| {
                let call_id = format!("call-{}", counter.fetch_add(1, Ordering::SeqCst));
                let title = match name {
                    "terminal" | "process" => args
                        .get("command")
                        .and_then(Value::as_str)
                        .map(|c| format!("{name}: {c}"))
                        .unwrap_or_else(|| name.to_string()),
                    "read_file" | "write_file" | "patch" => args
                        .get("path")
                        .and_then(Value::as_str)
                        .map(|p| format!("{name}: {p}"))
                        .unwrap_or_else(|| name.to_string()),
                    _ => name.to_string(),
                };
                open_calls
                    .lock()
                    .unwrap()
                    .entry(name.to_string())
                    .or_default()
                    .push_back(call_id.clone());
                let kind = tool_kind(name);
                let st = st.clone();
                let sid = sid.clone();
                let args = args.clone();
                tokio::spawn(async move {
                    st.session_update(
                        &sid,
                        json!({"sessionUpdate": "tool_call",
                               "toolCallId": call_id,
                               "title": title,
                               "kind": kind,
                               "status": "in_progress",
                               "rawInput": args}),
                    )
                    .await;
                });
            }
        })),
        on_tool_complete: Some(Box::new({
            let st = update_state.clone();
            let sid = update_session.clone();
            let open_calls = open_tool_calls.clone();
            move |name: &str, result: &Value| {
                // Plan updates from todo results (hermes events.py).
                let plan = if name == "todo" {
                    plan_update_from_todo_result(result)
                } else {
                    None
                };
                // Close the oldest still-open call of this name (hermes
                // tool_call_ids deque semantics).
                let call_id = open_calls
                    .lock()
                    .unwrap()
                    .get_mut(name)
                    .and_then(|queue| queue.pop_front())
                    .unwrap_or_else(|| format!("{name}-latest"));
                let st2 = st.clone();
                let sid2 = sid.clone();
                let result = result.clone();
                tokio::spawn(async move {
                    if let Some(plan) = plan {
                        st2.session_update(&sid2, plan).await;
                    }
                    st2.session_update(
                        &sid2,
                        json!({"sessionUpdate": "tool_call_update",
                               "toolCallId": call_id,
                               "status": "completed",
                               "rawOutput": result}),
                    )
                    .await;
                });
            }
        })),
        on_step: None,
        on_approval_request: None,
        on_activity: None,
    };
    agent.set_callbacks(callbacks).await;

    // Conversation continuity within the editor session + persistence
    // under `acp-<sessionId>`.
    let history = {
        let history = session.history.lock().await;
        if history.is_empty() {
            None
        } else {
            Some(history.clone())
        }
    };
    let store_key = format!("acp-{}", session.id);
    let run_result = agent
        .run_with_session_images(&user_text, images, history, Some(&store_key))
        .await;

    session.running.store(false, Ordering::SeqCst);
    let cancelled = session.cancelled.swap(false, Ordering::SeqCst);

    match run_result {
        Ok(result) => {
            // Carry the full conversation forward for the next turn.
            *session.history.lock().await = result.conversation.clone();
            let stop_reason = if cancelled { "cancelled" } else { "end_turn" };
            state
                .respond_success(id, json!({"stopReason": stop_reason}))
                .await;
        }
        Err(e) => {
            state
                .session_update(
                    &session_id,
                    json!({"sessionUpdate": "agent_message_chunk",
                           "content": {"type": "text", "text": format!("error: {e}")}}),
                )
                .await;
            state
                .respond_success(id, json!({"stopReason": "end_turn"}))
                .await;
        }
    }
}

// ---------------------------------------------------------------------------
// Main loop
// ---------------------------------------------------------------------------

async fn handle_line(state: &Arc<ServerState>, line: &str) {
    let request: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(_) => {
            state
                .respond_error(Value::Null, -32700, "Parse error")
                .await;
            return;
        }
    };

    // Responses to our own outbound requests (permission decisions).
    if request.get("method").is_none() {
        if let Some(request_id) = request.get("id").and_then(Value::as_u64) {
            let responder = state
                .pending_client_requests
                .lock()
                .unwrap()
                .remove(&request_id);
            if let Some(responder) = responder {
                let result = request
                    .get("result")
                    .cloned()
                    .unwrap_or_else(|| json!({"error": request.get("error").cloned()}));
                let _ = responder.send(result);
            }
        }
        return;
    }

    let method = request
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let id = request.get("id").cloned();
    let params = request.get("params").cloned().unwrap_or(json!({}));
    let is_notification = id.is_none();

    match method.as_str() {
        "initialize" => handle_initialize(state, id.unwrap_or(Value::Null), params).await,
        "authenticate" => {
            // No auth methods advertised — nothing to authenticate.
            if !is_notification {
                state
                    .respond_success(id.unwrap_or(Value::Null), json!(null))
                    .await;
            }
        }
        "session/new" => handle_new_session(state, id.unwrap_or(Value::Null), params).await,
        "session/load" => handle_load_session(state, id.unwrap_or(Value::Null), params).await,
        "session/prompt" => handle_prompt(state, id.unwrap_or(Value::Null), params).await,
        "session/cancel" => {
            let session_id = params
                .get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or("");
            if let Some(session) = state.sessions.lock().unwrap().get(session_id) {
                session.cancelled.store(true, Ordering::SeqCst);
                eprintln!("[acp] cancel requested for {session_id}");
            }
            // Notification in ACP — no response.
        }
        "session/set_mode" | "session/set_model" => {
            if !is_notification {
                state
                    .respond_success(id.unwrap_or(Value::Null), json!(null))
                    .await;
            }
        }
        _ => {
            if !is_notification {
                // Liveness probes (ping/health/healthcheck) get the same
                // -32601 treatment as hermes (_BENIGN_PROBE_METHODS).
                state
                    .respond_error(
                        id.unwrap_or(Value::Null),
                        -32601,
                        &format!("Method not found: {method}"),
                    )
                    .await;
            }
        }
    }
}

/// Run the ACP stdio server until EOF (hermes `entry.py` →
/// `acp.Agent.serve`).
pub async fn run_stdio(verbose: bool) -> std::io::Result<()> {
    use tokio::io::AsyncBufReadExt;

    let state = ServerState::new(verbose);
    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();

    while let Some(line) = lines.next_line().await? {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        if verbose {
            eprintln!("[acp] <- {line}");
        }
        handle_line(&state, &line).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_extraction_text_and_images() {
        let (text, images) = extract_prompt(&json!([
            {"type": "text", "text": "hello"},
            {"type": "text", "text": "world"},
            {"type": "image", "data": "QUJD", "mimeType": "image/png"},
            {"type": "image", "url": "https://example.com/x.png"},
            {"type": "resource_link", "uri": "file:///tmp/a.txt", "name": "a.txt"},
        ]));
        assert_eq!(
            text,
            "hello\nworld\n[Attached resource: a.txt — file:///tmp/a.txt]"
        );
        assert_eq!(images.len(), 2);
        assert!(images[0].url.starts_with("data:image/png;base64,"));
        assert_eq!(images[1].url, "https://example.com/x.png");
    }

    #[test]
    fn tool_kind_mapping() {
        assert_eq!(tool_kind("read_file"), "read");
        assert_eq!(tool_kind("write_file"), "edit");
        assert_eq!(tool_kind("terminal"), "execute");
        assert_eq!(tool_kind("web_search"), "fetch");
        assert_eq!(tool_kind("unknown_tool"), "other");
    }

    #[test]
    fn plan_update_from_todo() {
        let result = json!(
            r#"{"todos":[{"content":"step one","status":"completed"},{"content":"step two","status":"pending"},{"content":"gone","status":"cancelled"}]}"#
        );
        let plan = plan_update_from_todo_result(&result).unwrap();
        assert_eq!(plan["sessionUpdate"], "plan");
        let entries = plan["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0]["status"], "completed");
        assert_eq!(entries[2]["content"], "[cancelled] gone");
        assert!(plan_update_from_todo_result(&json!("not json")).is_none());
    }

    #[test]
    fn protocol_version_is_v1() {
        assert_eq!(PROTOCOL_VERSION, 1);
    }
}

//! `/api/ws` — dashboard live socket (hermes desktop parity).
//!
//! The hermes desktop renderer speaks JSON-RPC over this socket: requests
//! are `{id, method, params}` frames answered with `{id, result}` /
//! `{id, error:{message}}`, and server-push events travel as
//! `{method:"event", params:{type, session_id, payload}}`. This bridge
//! implements the renderer's method surface (prompt.submit with live
//! message/tool streaming, session lifecycle, config, approvals/clarify/
//! terminal-read responders, process + pet + wake housekeeping) on top of
//! the ulnclaw gateway, and mirrors the desktop event bus onto the wire.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::gateway::GatewayState;

/// `GET /api/ws` — upgrade to the live event socket.
pub async fn dashboard_ws(
    ws: WebSocketUpgrade,
    Query(params): Query<HashMap<String, String>>,
    State(state): State<Arc<GatewayState>>,
) -> Response {
    let token = params.get("token").cloned();
    if let Some(key) = state.key.as_ref() {
        if token.as_deref() != Some(key.as_str()) {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "unauthorized"})),
            )
                .into_response();
        }
    }
    ws.on_upgrade(move |socket| serve_socket(socket, state))
}

type Sink = Arc<Mutex<futures::stream::SplitSink<WebSocket, Message>>>;

async fn send_frame(sink: &Sink, frame: Value) -> bool {
    sink.lock()
        .await
        .send(Message::Text(frame.to_string().into()))
        .await
        .is_ok()
}

async fn serve_socket(socket: WebSocket, state: Arc<GatewayState>) {
    let (sink, mut stream) = socket.split();
    let sink: Sink = Arc::new(Mutex::new(sink));
    let hello = json!({
        "type": "hello",
        "server": "ulnclaw",
        "version": env!("CARGO_PKG_VERSION"),
    });
    if !send_frame(&sink, hello).await {
        return;
    }
    // hermes parity: the renderer seeds live-sync (and demotes its legacy
    // polls) only after a `gateway.ready` event; advertise change events.
    let ready = json!({
        "method": "event",
        "params": {
            "type": "gateway.ready",
            "payload": {"change_events": true, "server": "ulnclaw"},
        },
    });
    if !send_frame(&sink, ready).await {
        return;
    }
    // Pre-existing sessions never fire a live `sessions.changed`; nudge the
    // renderer to pull the sidebar once on connect so history shows up.
    for change in ["sessions.changed", "cron.changed", "platforms.changed"] {
        let frame = json!({
            "method": "event",
            "params": {"type": change, "payload": {}},
        });
        if !send_frame(&sink, frame).await {
            return;
        }
    }
    let mut bus = crate::desktop_bridge::subscribe();
    let mut keepalive = tokio::time::interval(std::time::Duration::from_secs(15));
    keepalive.tick().await; // first tick is immediate; drop it
    loop {
        tokio::select! {
            event = bus.recv() => {
                let Ok(event) = event else { return };
                let frame = json!({
                    "method": "event",
                    "params": {
                        "type": event.event,
                        "session_id": event.session_id,
                        "payload": event.payload,
                    },
                });
                if !send_frame(&sink, frame).await {
                    return;
                }
            }
            _ = keepalive.tick() => {
                if !send_frame(&sink, json!({"type": "ping"})).await {
                    return;
                }
            }
            inbound = stream.next() => {
                match inbound {
                    None | Some(Ok(Message::Close(_))) | Some(Err(_)) => return,
                    Some(Ok(Message::Ping(data))) => {
                        if sink.lock().await.send(Message::Pong(data)).await.is_err() {
                            return;
                        }
                    }
                    Some(Ok(Message::Text(text))) => {
                        let Ok(frame) = serde_json::from_str::<Value>(&text) else {
                            continue;
                        };
                        let Some(id) = frame.get("id").cloned() else {
                            continue;
                        };
                        let method = frame
                            .get("method")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let params = frame.get("params").cloned().unwrap_or(Value::Null);
                        let sink2 = sink.clone();
                        let state2 = state.clone();
                        tokio::spawn(async move {
                            let reply = match dispatch(state2, &method, params).await {
                                Ok(result) => json!({"id": id, "result": result}),
                                Err(message) => json!({"id": id, "error": {"message": message}}),
                            };
                            send_frame(&sink2, reply).await;
                        });
                    }
                    Some(Ok(_)) => {}
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// In-flight turn registry (busy detection + interrupt)
// ---------------------------------------------------------------------------

struct TurnEntry {
    abort: tokio::task::AbortHandle,
}

fn turns() -> &'static Mutex<HashMap<String, TurnEntry>> {
    static MAP: std::sync::OnceLock<Mutex<HashMap<String, TurnEntry>>> = std::sync::OnceLock::new();
    MAP.get_or_init(Default::default)
}

fn publish(session_id: &str, event: &str, payload: Value) {
    crate::desktop_bridge::publish(session_id, event, &payload);
}

// ---------------------------------------------------------------------------
// Method dispatch
// ---------------------------------------------------------------------------

async fn dispatch(state: Arc<GatewayState>, method: &str, params: Value) -> Result<Value, String> {
    match method {
        "prompt.submit" => prompt_submit(state, params).await,
        "session.create" => session_create(state, params),
        "session.interrupt" => session_interrupt(params),
        "session.resume" => session_resume(state, params),
        "session.close" => session_close(state, params),
        "session.title" => session_title(state, params),
        "config.get" => config_get(),
        "config.set" => config_set(params),
        "reload.env" => reload_env(),
        "reload.mcp" => reload_mcp_ws(state, params).await,
        "approval.respond" => approval_respond(params),
        "clarify.respond" => clarify_respond(params),
        "sudo.respond" => prompt_respond(params, "password"),
        "secret.respond" => prompt_respond(params, "value"),
        "terminal.read.respond" => terminal_read_respond(params),
        "process.list" => Ok(process_list()),
        "process.kill" => process_kill(params),
        "model.options" => model_options(state).await,
        "commands.catalog" => Ok(commands_catalog()),
        "complete.slash" => Ok(complete_slash(params)),
        "complete.path" => Ok(complete_path(params)),
        "slash.exec" => slash_exec(state, params),
        "llm.oneshot" => llm_oneshot(state, params).await,
        "message.react" => message_react(state, params),
        "wake.pause" | "wake.resume" => Ok(json!({"ok": true})),
        "wake.start" => Ok(wake_start()),
        "wake.status" => Ok(wake_status()),
        "wake.stop" => Ok(wake_stop()),
        "pet.generate.status" => Ok(pet_generate_status()),
        "pet.generate" => pet_generate(params).await,
        "pet.hatch" => pet_hatch(params).await,
        "pet.rename" => pet_rename(params),
        "pet.select" => pet_select(params),
        "pet.remove" => pet_remove(params).await,
        "pet.cancel" => pet_cancel(params).await,
        // Portal billing / subscription (fail-open logged-out; no Nous Portal
        // in this build — hermes tui_gateway billing.* parity).
        "billing.state" | "subscription.state" => Ok(portal_logged_out()),
        "usage.bars" => Ok(json!({"ok": true, "available": false})),
        "billing.charge"
        | "billing.charge_status"
        | "billing.step_up"
        | "billing.auto_reload"
        | "subscription.preview"
        | "subscription.change"
        | "subscription.resume"
        | "subscription.upgrade" => Ok(portal_unavailable()),
        // Session surface (hermes tui_gateway methods_session.py parity).
        "session.usage" => session_usage(state, params),
        "session.context_breakdown" => session_context_breakdown(state, params),
        "session.status" => session_status(state, params),
        "session.save" => session_save(state, params),
        "session.branch" => session_branch(state, params),
        "session.compress" => session_compress(state, params).await,
        "session.redirect" => session_redirect(state, params),
        "session.activate" => session_activate(state, params),
        "session.active_list" => session_active_list(params).await,
        // Pet surface (hermes tui_gateway pet.* parity).
        "pet.info" => Ok(pet_info()),
        "pet.info.meta" => Ok(pet_info_meta()),
        "pet.gallery" => pet_gallery(params),
        "pet.thumb" => pet_thumb(params),
        "pet.scale" => pet_scale(params),
        "pet.disable" => pet_disable(),
        "pet.export" => pet_export(params),
        // Browser / command / handoff / preview / setup (hermes parity).
        "browser.manage" => browser_manage(params).await,
        "command.dispatch" => command_dispatch(state, params),
        "handoff.request" => handoff_request(params),
        "handoff.state" => handoff_state(params),
        "handoff.fail" => Ok(json!({"failed": false, "state": ""})),
        "preview.restart" => preview_restart(state, params),
        "setup.status" => Ok(setup_status()),
        "setup.runtime_check" => Ok(setup_runtime_check(&state)),
        other => Err(format!("unknown method: {other}")),
    }
}

async fn prompt_submit(state: Arc<GatewayState>, params: Value) -> Result<Value, String> {
    let session_id = params
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if session_id.is_empty() {
        return Err("session_id is required".into());
    }
    let text = params
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    {
        let map = turns().lock().await;
        if map.contains_key(&session_id) {
            return Err("session busy (4009)".into());
        }
    }
    state
        .store
        .ensure_session(&session_id, "desktop", None, None)
        .map_err(|e| e.to_string())?;

    publish(&session_id, "message.start", json!({}));

    let history = state
        .store
        .load_messages(&session_id)
        .unwrap_or_default()
        .into_iter()
        .filter(|m| m.role != crate::provider::Role::System)
        .collect::<Vec<_>>();
    let history_arg = if history.is_empty() {
        None
    } else {
        Some(history)
    };

    let runner = state.agent.clone();
    let sid_emit = session_id.clone();
    let sid_run = session_id.clone();
    // Enforce the session model lock (session.create / POST
    // /api/sessions/:id/model) on WS turns exactly like the HTTP chat path.
    let override_model = crate::gateway::session_model_override(&state, &session_id);
    let task = tokio::spawn(crate::agent::stream_scope(
        Arc::new(move |event: crate::agent::StreamEvent| match event {
            crate::agent::StreamEvent::Delta(delta) => {
                publish(&sid_emit, "message.delta", json!({"text": delta}));
            }
            crate::agent::StreamEvent::ToolProgress { tool, status } => {
                publish(
                    &sid_emit,
                    "tool.progress",
                    json!({"name": tool, "status": status}),
                );
            }
            crate::agent::StreamEvent::ToolStarted {
                name,
                call_id,
                arguments,
            } => {
                publish(
                    &sid_emit,
                    "tool.start",
                    json!({"name": name, "call_id": call_id, "id": call_id, "arguments": arguments, "title": name}),
                );
            }
            crate::agent::StreamEvent::ToolCompleted { call_id, result } => {
                publish(
                    &sid_emit,
                    "tool.complete",
                    json!({"call_id": call_id, "id": call_id, "result": result}),
                );
            }
        }),
        async move {
            let turn = runner.run_with_session_images(&text, vec![], history_arg, Some(&sid_run));
            match override_model {
                Some(model) => crate::agent::model_override_scope(model, turn).await,
                None => turn.await,
            }
        },
    ));
    let abort = task.abort_handle();
    turns()
        .lock()
        .await
        .insert(session_id.clone(), TurnEntry { abort });

    let sid2 = session_id.clone();
    tokio::spawn(async move {
        let outcome = task.await;
        turns().lock().await.remove(&sid2);
        match outcome {
            Ok(Ok(result)) => {
                publish(
                    &sid2,
                    "message.complete",
                    json!({
                        "text": result.content,
                        "usage": {
                            "input": result.usage.prompt_tokens,
                            "output": result.usage.completion_tokens,
                            "total": result.usage.total_tokens,
                            "calls": result.iterations,
                        },
                    }),
                );
            }
            Ok(Err(err)) => {
                publish(
                    &sid2,
                    "message.complete",
                    json!({"text": "", "status": "error", "error": err.to_string()}),
                );
            }
            Err(_) => {
                publish(
                    &sid2,
                    "message.complete",
                    json!({"text": "", "status": "error", "error": "interrupted", "partial": true}),
                );
            }
        }
        publish(
            &sid2,
            "session.info",
            json!({"session_id": sid2, "running": false}),
        );
        publish(&sid2, "sessions.changed", json!({"session_id": sid2}));
    });

    Ok(json!({"ok": true, "session_id": session_id}))
}

fn session_create(state: Arc<GatewayState>, params: Value) -> Result<Value, String> {
    // hermes tui_gateway session.create parity (methods_session.py): the
    // gateway mints the session id and answers immediately with a lightweight
    // payload so the desktop can paint the composer. No DB row is written for
    // a bare create — prompt.submit's ensure_session persists the row on the
    // first turn (hermes lazy-row contract), so launches and draft tabs that
    // never send a message don't leave empty sessions in the list. A create
    // carrying a model override / explicit cwd / title persists a row so the
    // per-session state survives into the turn path.
    let session_id = uuid::Uuid::new_v4().to_string();

    // Workspace: only an explicit, existing directory wins; otherwise the
    // gateway's launch directory (hermes explicit_cwd semantics).
    let raw_cwd = params
        .get("cwd")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    let expanded = if raw_cwd == "~" {
        std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_default()
    } else if let Some(rest) = raw_cwd.strip_prefix("~/") {
        std::env::var_os("HOME")
            .map(|home| std::path::PathBuf::from(home).join(rest))
            .unwrap_or_default()
    } else {
        std::path::PathBuf::from(&raw_cwd)
    };
    let explicit_cwd = !raw_cwd.is_empty() && expanded.is_dir();
    let cwd = if explicit_cwd {
        expanded
    } else {
        std::env::current_dir().unwrap_or_default()
    };
    let cwd_str = cwd.display().to_string();

    // Per-session model override (desktop composer pick): persisted on the
    // session row so WS turns enforce it via model_override_scope, and
    // reflected in info so the composer doesn't briefly clobber its sticky
    // pick with the global default.
    let create_model = params
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(str::to_string);
    let create_provider = create_model.as_ref().and_then(|_| {
        params
            .get("provider")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_string)
    });
    let title = params
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default()
        .to_string();
    let source = params
        .get("source")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("desktop");

    // Branch/seed contract (hermes session.create): `messages` carries a
    // copied transcript and `parent_session_id` links the new row to its
    // lineage parent. Either one forces an immediately persisted row so the
    // branch shows up in the sidebar before its first turn.
    let seed: Vec<crate::provider::Message> = params
        .get("messages")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    let role = match row.get("role").and_then(Value::as_str)? {
                        "assistant" => crate::provider::Role::Assistant,
                        "system" => crate::provider::Role::System,
                        "tool" => crate::provider::Role::Tool,
                        _ => crate::provider::Role::User,
                    };
                    let content = match row.get("content") {
                        Some(Value::String(text)) => Some(text.clone()),
                        Some(other) => Some(other.to_string()),
                        None => None,
                    };
                    Some(crate::provider::Message {
                        role,
                        content,
                        tool_calls: None,
                        tool_call_id: None,
                        name: None,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let parent_session_id = params
        .get("parent_session_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    if !seed.is_empty() || parent_session_id.is_some() {
        state
            .store
            .create_named_session(
                &session_id,
                source,
                create_model.as_deref(),
                parent_session_id.as_deref(),
            )
            .map_err(|e| e.to_string())?;
        for message in &seed {
            state
                .store
                .append_message(&session_id, message)
                .map_err(|e| e.to_string())?;
        }
        if !title.is_empty() {
            state
                .store
                .set_session_title(&session_id, &title)
                .map_err(|e| e.to_string())?;
        }
    } else if create_model.is_some() || explicit_cwd || !title.is_empty() {
        state
            .store
            .ensure_session(
                &session_id,
                source,
                create_model.as_deref(),
                explicit_cwd.then_some(cwd_str.as_str()),
            )
            .map_err(|e| e.to_string())?;
        if !title.is_empty() {
            state
                .store
                .set_session_title(&session_id, &title)
                .map_err(|e| e.to_string())?;
        }
    }

    let model = create_model.unwrap_or_else(|| state.model_name.clone());
    let provider = create_provider.unwrap_or_else(|| state.provider_name.clone());

    let mut info = json!({
        "model": model,
        "provider": provider,
        "tools": {},
        "skills": {},
        "cwd": cwd_str,
        "branch": git_branch_for_cwd(&cwd),
        "lazy": true,
        "desktop_contract": 5,
        "version": env!("CARGO_PKG_VERSION"),
        "running": false,
    });
    if let Some(effort) = params
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        info["reasoning_effort"] = json!(effort);
    }
    if let Some(fast) = params.get("fast").and_then(Value::as_bool) {
        info["fast"] = json!(fast);
        info["service_tier"] = json!(if fast { "priority" } else { "" });
    }

    let seed_wire: Vec<Value> = seed
        .iter()
        .map(|message| json!({"role": message.role, "content": message.content}))
        .collect();
    Ok(json!({
        "session_id": session_id,
        "stored_session_id": session_id,
        "message_count": seed_wire.len(),
        "messages": seed_wire,
        "info": info,
    }))
}

fn git_branch_for_cwd(cwd: &std::path::Path) -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_default()
}

fn session_interrupt(params: Value) -> Result<Value, String> {
    let session_id = params
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let removed = {
        let mut map = match turns().try_lock() {
            Ok(map) => map,
            Err(_) => return Err("turn registry locked".into()),
        };
        map.remove(session_id)
    };
    let interrupted = removed.is_some();
    if let Some(entry) = removed {
        entry.abort.abort();
    }
    Ok(json!({"ok": true, "interrupted": interrupted}))
}

fn session_resume(state: Arc<GatewayState>, params: Value) -> Result<Value, String> {
    let session_id = params
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if session_id.is_empty() {
        return Err("session_id is required".into());
    }
    let source = params
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or("desktop");
    state
        .store
        .ensure_session(&session_id, source, None, None)
        .map_err(|e| e.to_string())?;
    Ok(json!({"session_id": session_id}))
}

fn session_close(state: Arc<GatewayState>, params: Value) -> Result<Value, String> {
    let session_id = params
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let _ = state.store.end_session(session_id, "closed");
    publish(
        session_id,
        "sessions.changed",
        json!({"session_id": session_id}),
    );
    Ok(json!({"ok": true}))
}

fn session_title(state: Arc<GatewayState>, params: Value) -> Result<Value, String> {
    let session_id = params
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let title = params
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or_default();
    state
        .store
        .set_session_title(session_id, title)
        .map_err(|e| e.to_string())?;
    publish(
        session_id,
        "session.title",
        json!({"session_id": session_id, "title": title}),
    );
    Ok(json!({"ok": true}))
}

fn config_get() -> Result<Value, String> {
    let path = crate::config_cmd::config_path();
    let config = crate::config::UlncLawConfig::load(Some(&path)).unwrap_or_default();
    serde_json::to_value(&config).map_err(|e| e.to_string())
}

fn config_set(params: Value) -> Result<Value, String> {
    // The desktop writes a handful of display/behavior keys; accept and
    // persist the raw patch into config.toml under [desktop].
    let path = crate::config_cmd::config_path();
    let raw = std::fs::read_to_string(&path).unwrap_or_default();
    let mut doc: toml::Value = raw
        .parse()
        .unwrap_or_else(|_| toml::Value::Table(Default::default()));
    let table = doc
        .as_table_mut()
        .ok_or_else(|| "config root is not a table".to_string())?;
    let desktop = table
        .entry("desktop")
        .or_insert_with(|| toml::Value::Table(Default::default()));
    if let (Some(desktop_table), Some(obj)) = (desktop.as_table_mut(), params.as_object()) {
        for (key, value) in obj {
            if let Ok(converted) = json_to_toml(value) {
                desktop_table.insert(key.clone(), converted);
            }
        }
    }
    std::fs::write(&path, doc.to_string()).map_err(|e| e.to_string())?;
    Ok(json!({"ok": true}))
}

fn json_to_toml(value: &Value) -> Result<toml::Value, ()> {
    Ok(match value {
        Value::Null => toml::Value::String(String::new()),
        Value::Bool(b) => toml::Value::Boolean(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                toml::Value::Integer(i)
            } else {
                toml::Value::Float(n.as_f64().unwrap_or(0.0))
            }
        }
        Value::String(s) => toml::Value::String(s.clone()),
        Value::Array(items) => toml::Value::Array(
            items
                .iter()
                .map(json_to_toml)
                .collect::<Result<Vec<_>, _>>()?,
        ),
        Value::Object(map) => {
            let mut table = toml::map::Map::new();
            for (key, value) in map {
                table.insert(key.clone(), json_to_toml(value)?);
            }
            toml::Value::Table(table)
        }
    })
}

fn approval_respond(params: Value) -> Result<Value, String> {
    let session_key = params
        .get("request_id")
        .or_else(|| params.get("session_key"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let choice = params
        .get("choice")
        .or_else(|| params.get("response"))
        .and_then(Value::as_str)
        .unwrap_or("deny");
    let resolved = crate::approval_gateway::resolve(session_key, choice);
    Ok(json!({"ok": resolved}))
}

fn clarify_respond(params: Value) -> Result<Value, String> {
    let clarify_id = params
        .get("request_id")
        .or_else(|| params.get("clarify_id"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let response = params
        .get("response")
        .or_else(|| params.get("text"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let resolved = crate::clarify_gateway::resolve(clarify_id, response);
    Ok(json!({"ok": resolved}))
}

fn terminal_read_respond(params: Value) -> Result<Value, String> {
    let id = params
        .get("request_id")
        .or_else(|| params.get("id"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let answer = params
        .get("text")
        .or_else(|| params.get("answer"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let ok = crate::desktop_bridge::resolve_read(id, Ok(answer));
    Ok(json!({"ok": ok}))
}

fn process_list() -> Value {
    let rows: Vec<Value> = crate::tools::builtin::terminal::list_background_processes()
        .into_iter()
        .map(|info| {
            json!({
                "pid": info.pid,
                "session_id": info.session_id,
                "command": info.command,
                "status": info.status,
                "uptime_seconds": info.uptime_seconds,
                "output_preview": info.output_preview,
            })
        })
        .collect();
    json!(rows)
}

fn process_kill(params: Value) -> Result<Value, String> {
    let process_id = params
        .get("process_id")
        .or_else(|| params.get("id"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let value = crate::tools::builtin::terminal::request_close_terminal("", process_id);
    Ok(value)
}

async fn model_options(state: Arc<GatewayState>) -> Result<Value, String> {
    let response = crate::gateway::model_options_pub(State(state)).await;
    let (parts, body) = response.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .map_err(|e| e.to_string())?;
    if !parts.status.is_success() {
        return Err("model options unavailable".into());
    }
    serde_json::from_slice(&bytes).map_err(|e| e.to_string())
}

fn commands_catalog() -> Value {
    let commands: Vec<Value> = crate::gateway::slash_catalog_pub()
        .into_iter()
        .map(|(name, description)| json!({"name": name, "description": description}))
        .collect();
    json!({"commands": commands})
}

fn complete_slash(params: Value) -> Value {
    let prefix = params
        .get("prefix")
        .or_else(|| params.get("query"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_lowercase();
    let matches: Vec<Value> = crate::gateway::slash_catalog_pub()
        .into_iter()
        .filter(|(name, _)| name.to_lowercase().contains(&prefix))
        .take(20)
        .map(|(name, description)| json!({"name": name, "description": description}))
        .collect();
    json!({"suggestions": matches})
}

fn complete_path(params: Value) -> Value {
    let prefix = params
        .get("prefix")
        .or_else(|| params.get("query"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let (dir_part, file_part) = match prefix.rfind('/') {
        Some(idx) => (&prefix[..idx + 1], &prefix[idx + 1..]),
        None => ("", prefix),
    };
    let base = if dir_part.is_empty() {
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
    } else {
        std::path::PathBuf::from(dir_part)
    };
    let mut suggestions = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&base) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !file_part.is_empty() && !name.to_lowercase().starts_with(&file_part.to_lowercase())
            {
                continue;
            }
            let is_dir = entry.path().is_dir();
            let label = if is_dir {
                format!("{dir_part}{name}/")
            } else {
                format!("{dir_part}{name}")
            };
            suggestions.push(json!({"path": label, "is_dir": is_dir}));
            if suggestions.len() >= 20 {
                break;
            }
        }
    }
    json!({"suggestions": suggestions})
}

async fn llm_oneshot(state: Arc<GatewayState>, params: Value) -> Result<Value, String> {
    let prompt = params
        .get("prompt")
        .or_else(|| params.get("text"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if prompt.is_empty() {
        return Err("prompt is required".into());
    }
    let runner = state.agent.clone();
    let result = runner
        .run_with_session_images(&prompt, vec![], None, None)
        .await
        .map_err(|e| e.to_string())?;
    Ok(json!({"text": result.content}))
}

// ---------------------------------------------------------------------------
// Config env + MCP reload (hermes reload.env / reload.mcp parity)
// ---------------------------------------------------------------------------

fn reload_env() -> Result<Value, String> {
    let applied = crate::config::reload_env();
    Ok(json!({
        "ok": true,
        "applied": applied,
        "output": format!("{applied} env var(s) reloaded from .env"),
    }))
}

async fn reload_mcp_ws(state: Arc<GatewayState>, params: Value) -> Result<Value, String> {
    let confirm = params
        .get("confirm")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !confirm {
        return Ok(json!({
            "warning": "Reloading MCP servers disconnects and reconnects every configured server (live sessions' prompt cache is invalidated). Pass confirm=true to proceed.",
            "output": "MCP reload skipped — confirmation required.",
        }));
    }
    let config = crate::config::UlncLawConfig::load(None).unwrap_or_default();
    let report = state.agent.reload_mcp(&config).await;
    Ok(json!({ "ok": true, "output": crate::mcp::format_reload_report(&report) }))
}

// ---------------------------------------------------------------------------
// Gateway-side slash execution (hermes slash.exec parity, lean)
// ---------------------------------------------------------------------------

fn slash_exec(state: Arc<GatewayState>, params: Value) -> Result<Value, String> {
    let session_id = params
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let command = params
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .trim_start_matches('/')
        .to_string();
    if command.is_empty() {
        return Err("empty command".into());
    }
    let mut parts = command.splitn(2, char::is_whitespace);
    let base = parts.next().unwrap_or_default().to_lowercase();
    let arg = parts.next().unwrap_or_default().trim().to_lowercase();

    match base.as_str() {
        "goal" => {
            let mut manager =
                crate::goals::GoalManager::new(session_id, Some(state.store.clone()), 0);
            let no_goal = || "No active goal. Set one with /goal <text>.".to_string();
            let output = match arg.as_str() {
                "" | "status" => manager.status_line(),
                "pause" => manager
                    .pause("paused via /goal pause")
                    .map(|s| format!("⏸ Goal paused: {}", s.goal))
                    .unwrap_or_else(no_goal),
                "resume" => manager
                    .resume(false)
                    .map(|s| format!("▶ Goal resumed: {}", s.goal))
                    .unwrap_or_else(no_goal),
                "clear" => {
                    manager.clear();
                    "✓ Goal cleared.".to_string()
                }
                other => return Err(format!("unknown goal subcommand: {other}")),
            };
            Ok(json!({ "output": output }))
        }
        other => Err(format!("not a gateway slash command: {other}")),
    }
}

// ---------------------------------------------------------------------------
// Mid-turn prompt capture (hermes tui_gateway sudo/secret.respond parity)
// ---------------------------------------------------------------------------

/// Pending mid-turn prompt requests (sudo password / skill secret capture).
/// Future gateway-side emitters register a request id here and await the
/// oneshot; the desktop overlay answers over sudo.respond / secret.respond.
fn pending_prompts(
) -> &'static std::sync::Mutex<HashMap<String, tokio::sync::oneshot::Sender<String>>> {
    static MAP: std::sync::OnceLock<
        std::sync::Mutex<HashMap<String, tokio::sync::oneshot::Sender<String>>>,
    > = std::sync::OnceLock::new();
    MAP.get_or_init(Default::default)
}

/// Register a mid-turn prompt so a surface can answer it. Returns the receiver
/// the emitter awaits for the user-supplied value.
pub fn request_prompt(request_id: &str) -> tokio::sync::oneshot::Receiver<String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    pending_prompts()
        .lock()
        .unwrap()
        .insert(request_id.to_string(), tx);
    rx
}

fn prompt_respond(params: Value, key: &str) -> Result<Value, String> {
    let request_id = params
        .get("request_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let mut map = pending_prompts().lock().unwrap();
    match map.remove(&request_id) {
        Some(sender) => {
            let value = params
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let _ = sender.send(value);
            Ok(json!({ "status": "ok" }))
        }
        // allow_expired semantics (hermes _respond): a late answer racing a
        // backend timeout is normal — report expired, not an error.
        None if !request_id.is_empty() => Ok(json!({ "status": "expired" })),
        None => Err(format!("no pending {key} request")),
    }
}

// ---------------------------------------------------------------------------
// Message reactions (hermes message.react WS parity)
// ---------------------------------------------------------------------------

fn message_react(state: Arc<GatewayState>, params: Value) -> Result<Value, String> {
    let session_id = params
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if session_id.is_empty() {
        return Err("session_id is required".into());
    }
    let emoji = params
        .get("emoji")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    let author = params
        .get("author")
        .and_then(Value::as_str)
        .filter(|a| !a.trim().is_empty())
        .unwrap_or("user")
        .to_string();

    let store = &state.store;
    let mut target_role = "user".to_string();
    let row_id: i64 = if let Some(id) = params.get("row_id").and_then(Value::as_i64) {
        match store.message_role(&session_id, id) {
            Some(role) => {
                target_role = role;
                id
            }
            None => return Err(format!("Message {id} is not part of this conversation.")),
        }
    } else {
        let role = params
            .get("newest_role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        store
            .latest_message_row_id(&session_id, role, 0, true)
            .ok_or_else(|| "No message to react to yet.".to_string())?
    };

    let emoji_opt = if emoji.is_empty() {
        None
    } else {
        Some(emoji.as_str())
    };
    let Some(reactions) = store.set_message_reaction(&session_id, row_id, emoji_opt, &author)
    else {
        return Err(format!(
            "Message {row_id} is not part of this conversation."
        ));
    };

    publish(
        &session_id,
        "message.reaction",
        json!({ "row_id": row_id, "reactions": reactions, "role": target_role }),
    );
    Ok(json!({ "ok": true, "row_id": row_id, "reactions": reactions }))
}

// ---------------------------------------------------------------------------
// Pet generate / hatch / adopt (hermes tui_gateway pet.* WS parity)
// ---------------------------------------------------------------------------

fn pet_gen_root() -> std::path::PathBuf {
    crate::config::ulnclaw_home().join("pets").join(".gen")
}

fn pet_cancel_flags() -> &'static Mutex<HashMap<String, Arc<std::sync::atomic::AtomicBool>>> {
    static MAP: std::sync::OnceLock<Mutex<HashMap<String, Arc<std::sync::atomic::AtomicBool>>>> =
        std::sync::OnceLock::new();
    MAP.get_or_init(Default::default)
}

fn png_data_uri(img: &image::RgbaImage) -> String {
    use base64::Engine;
    let mut buf = Vec::new();
    if img
        .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
        .is_err()
    {
        return String::new();
    }
    format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(buf)
    )
}

fn pet_generate_status() -> Value {
    match crate::pets_generate::resolve_image_endpoint() {
        Ok(endpoint) => json!({
            "available": true,
            "providers": [{
                "name": endpoint.model,
                "label": format!("OpenAI-compatible images ({})", endpoint.model),
                "default": true,
            }],
        }),
        Err(reason) => json!({ "available": false, "providers": [], "reason": reason }),
    }
}

async fn pet_generate(params: Value) -> Result<Value, String> {
    let prompt = params
        .get("prompt")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    let reference_raw = params
        .get("referenceImage")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if prompt.is_empty() && reference_raw.is_empty() {
        return Err("missing prompt".into());
    }
    let count = params
        .get("count")
        .and_then(Value::as_u64)
        .unwrap_or(4)
        .clamp(1, 4) as usize;
    let style = params
        .get("style")
        .and_then(Value::as_str)
        .map(str::to_string);
    let endpoint = crate::pets_generate::resolve_image_endpoint()
        .map_err(|e| crate::pets_generate::humanize_image_error(&e))?;

    let token = uuid::Uuid::new_v4().to_string()[..12].to_string();
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    pet_cancel_flags()
        .lock()
        .await
        .insert(token.clone(), cancel.clone());
    let stage = pet_gen_root().join(&token);
    std::fs::create_dir_all(&stage).map_err(|e| e.to_string())?;

    let reference: Option<std::path::PathBuf> = if reference_raw.is_empty() {
        None
    } else {
        use base64::Engine;
        let comma = reference_raw
            .find(',')
            .ok_or_else(|| "invalid referenceImage data URL".to_string())?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&reference_raw[comma + 1..])
            .map_err(|e| format!("decode referenceImage: {e}"))?;
        let path = stage.join("reference.png");
        std::fs::write(&path, &bytes).map_err(|e| e.to_string())?;
        Some(path)
    };

    publish(
        "",
        "pet.generate.progress",
        json!({ "token": token, "count": count }),
    );

    let concept = if prompt.is_empty() {
        "a pet based on the reference image".to_string()
    } else {
        prompt
    };
    let token_emit = token.clone();
    let cancel_inner = cancel.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        let collected: std::sync::Mutex<Vec<(usize, String)>> = std::sync::Mutex::new(Vec::new());
        let stage_for_cb = stage.clone();
        let on_draft = |index: usize, img: &image::RgbaImage| {
            let uri = png_data_uri(img);
            let _ = img.save(stage_for_cb.join(format!("draft-{index}.png")));
            collected.lock().unwrap().push((index, uri.clone()));
            publish(
                "",
                "pet.generate.progress",
                json!({ "token": token_emit, "index": index, "dataUri": uri, "count": count }),
            );
        };
        let reference_arg = reference.as_deref();
        let result = if reference_arg.is_some() {
            // Grounded generation: fan out manually so every draft rides the
            // reference image (generate_base_drafts has no reference input).
            let mut images = Vec::new();
            for index in 0..count {
                if cancel_inner.load(std::sync::atomic::Ordering::SeqCst) {
                    break;
                }
                let variation =
                    ["front view", "three-quarter view", "side view", "close-up"][index % 4];
                let prompt =
                    crate::pets_generate::build_base_prompt(&concept, style.as_deref(), variation);
                match crate::pets_generate::generate_image(&endpoint, &prompt, reference_arg, false)
                    .map(|bytes| crate::pets_generate::harden_transparency(&bytes))
                {
                    Ok(img) => {
                        images.push(img.clone());
                        on_draft(index, &img);
                    }
                    Err(error) => tracing::warn!("pet.generate draft {index} failed: {error}"),
                }
            }
            Ok(images)
        } else {
            crate::pets_generate::generate_base_drafts(
                &endpoint,
                &concept,
                count,
                style.as_deref(),
                Some(&on_draft),
                Some(&cancel_inner),
            )
        };
        (result, collected.into_inner().unwrap())
    })
    .await
    .map_err(|e| e.to_string())?;

    pet_cancel_flags().lock().await.remove(&token);
    let (result, mut drafts) = outcome;
    if cancel.load(std::sync::atomic::Ordering::SeqCst) {
        return Err("generation cancelled".into());
    }
    let generated = result.map_err(|e| crate::pets_generate::humanize_image_error(&e))?;
    if drafts.is_empty() && !generated.is_empty() {
        // Callback stream unavailable — fall back to the collected images.
        drafts = generated
            .iter()
            .enumerate()
            .map(|(index, img)| (index, png_data_uri(img)))
            .collect();
    }
    if drafts.is_empty() {
        return Err("generation produced no usable drafts".into());
    }
    drafts.sort_by_key(|(index, _)| *index);
    Ok(json!({
        "ok": true,
        "token": token,
        "drafts": drafts
            .into_iter()
            .map(|(index, data_uri)| json!({ "index": index, "dataUri": data_uri }))
            .collect::<Vec<_>>(),
    }))
}

async fn pet_hatch(params: Value) -> Result<Value, String> {
    let token = params
        .get("token")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    let cancel_token = params
        .get("cancelToken")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    let cancel_token = if cancel_token.is_empty() {
        token.clone()
    } else {
        cancel_token
    };
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if token.is_empty() {
        return Err("missing token".into());
    }
    if name.is_empty() {
        return Err("missing name".into());
    }
    let index = params.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
    let base = pet_gen_root()
        .join(&token)
        .join(format!("draft-{index}.png"));
    if !base.is_file() {
        return Err("draft expired — generate again".into());
    }
    let description = params
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let concept = params
        .get("prompt")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| name.clone());
    let style = params
        .get("style")
        .and_then(Value::as_str)
        .map(str::to_string);

    let home = crate::config::ulnclaw_home();
    let endpoint = crate::pets_generate::resolve_image_endpoint()
        .map_err(|e| crate::pets_generate::humanize_image_error(&e))?;
    let mut slug = crate::pets::slugify(&name);
    let mut bump = 2;
    while crate::pets::load_pet(&home, &slug).is_some() {
        slug = format!("{}-{}", crate::pets::slugify(&name), bump);
        bump += 1;
    }

    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    pet_cancel_flags()
        .lock()
        .await
        .insert(cancel_token.clone(), cancel.clone());

    let outcome = tokio::task::spawn_blocking(move || {
        let base_image = image::open(&base)
            .map_err(|e| format!("read staged draft: {e}"))?
            .to_rgba8();
        let on_progress = |event: &str, detail: &str| {
            let payload = if event == "row" && detail.matches(':').count() == 2 {
                let mut parts = detail.split(':');
                json!({
                    "event": "row",
                    "state": parts.next().unwrap_or_default(),
                    "done": parts.next().unwrap_or_default(),
                    "total": parts.next().unwrap_or_default(),
                })
            } else {
                json!({ "event": event, "detail": detail })
            };
            publish("", "pet.hatch.progress", payload);
        };
        crate::pets_generate::hatch_pet(
            &home,
            &endpoint,
            &base_image,
            &slug,
            &name,
            &description,
            &concept,
            style.as_deref(),
            Some(&on_progress),
            Some(&cancel),
        )
    })
    .await
    .map_err(|e| e.to_string())?;

    pet_cancel_flags().lock().await.remove(&cancel_token);
    let result = outcome.map_err(|e| crate::pets_generate::humanize_image_error(&e))?;
    Ok(json!({
        "ok": true,
        "slug": result.slug,
        "displayName": result.display_name,
        "warnings": result.validation.warnings,
        "pet": {
            "enabled": false,
            "slug": result.slug,
            "displayName": result.display_name,
            "spritesheet": format!("/api/pets/{}/spritesheet", result.slug),
        },
    }))
}

fn pet_rename(params: Value) -> Result<Value, String> {
    let slug = params
        .get("slug")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if slug.is_empty() {
        return Err("missing slug".into());
    }
    if name.is_empty() {
        return Err("missing name".into());
    }
    let home = crate::config::ulnclaw_home();
    let new_slug = crate::pets::rename_pet(&home, &slug, &name)
        .ok_or_else(|| format!("could not rename pet '{slug}'"))?;
    Ok(json!({ "ok": true, "slug": new_slug, "displayName": name }))
}

fn pet_select(params: Value) -> Result<Value, String> {
    let slug = params
        .get("slug")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if slug.is_empty() {
        return Err("missing slug".into());
    }
    let home = crate::config::ulnclaw_home();
    let pet = crate::pets::load_pet(&home, &slug)
        .filter(|p| p.exists())
        .ok_or_else(|| format!("'{slug}' is not installed"))?;
    crate::pets::set_active(&slug).map_err(|e| format!("could not persist active pet: {e}"))?;
    Ok(json!({ "ok": true, "slug": slug, "displayName": pet.display_name }))
}

async fn pet_remove(params: Value) -> Result<Value, String> {
    let slug = params
        .get("slug")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if slug.is_empty() {
        return Err("missing slug".into());
    }
    let home = crate::config::ulnclaw_home();
    crate::pets::clear_active_if(&slug);
    let removed = crate::pets::remove_pet(&home, &slug);
    Ok(json!({ "ok": removed }))
}

async fn pet_cancel(params: Value) -> Result<Value, String> {
    let token = params
        .get("token")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if let Some(flag) = pet_cancel_flags().lock().await.get(&token) {
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    Ok(json!({ "ok": true }))
}

// ---------------------------------------------------------------------------
// Wake word (hermes tui_gateway wake.* WS parity)
//
// The ulnclaw build has no always-on mic/STT runtime, so the wake engine
// reports unavailable; the renderer degrades gracefully (slash output shows
// the hint, settings show the wake word as off).
// ---------------------------------------------------------------------------

const WAKE_UNAVAILABLE_HINT: &str =
    "wake-word listening is not built into this ulnclaw gateway (needs a mic + STT runtime)";

fn wake_status() -> Value {
    json!({
        "listening": false,
        "owned_by_caller": false,
        "owner_surface": Value::Null,
        "enabled": false,
        "available": false,
        "audio_silent": false,
        "phrase": "hey ulnclaw",
        "hint": WAKE_UNAVAILABLE_HINT,
        "input_device": { "status": "missing", "error": "no audio input runtime" },
    })
}

fn wake_start() -> Value {
    json!({
        "started": false,
        "reason": "not_available",
        "hint": WAKE_UNAVAILABLE_HINT,
    })
}

fn wake_stop() -> Value {
    json!({ "stopped": false, "reason": "not_owner", "disabled_persisted": false })
}

// ---------------------------------------------------------------------------
// Portal billing / subscription (hermes tui_gateway billing.* parity)
//
// ulnclaw has no Nous Portal integration: read-only state methods fail open
// as logged-out (the renderer paints the signed-out billing screen), and
// every mutation answers the typed error envelope the billing API converts
// into a refusal toast.
// ---------------------------------------------------------------------------

fn portal_logged_out() -> Value {
    json!({"ok": true, "logged_in": false})
}

fn portal_unavailable() -> Value {
    json!({
        "ok": false,
        "error": "unavailable",
        "message": "Nous Portal billing is not integrated in this ulnclaw gateway build.",
    })
}

// ---------------------------------------------------------------------------
// Session surface (hermes tui_gateway methods_session.py parity)
// ---------------------------------------------------------------------------

fn require_session_id(params: &Value) -> Result<String, String> {
    let session_id = params
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if session_id.is_empty() {
        return Err("session_id is required".into());
    }
    Ok(session_id)
}

fn history_to_wire(messages: &[crate::provider::Message]) -> Vec<Value> {
    messages
        .iter()
        .map(|message| json!({"role": message.role, "content": message.content}))
        .collect()
}

fn session_is_busy(session_id: &str) -> bool {
    turns()
        .try_lock()
        .map(|map| map.contains_key(session_id))
        .unwrap_or(false)
}

/// The `info` block shared by session.activate / session.branch /
/// session.compress (hermes `_session_info`, desktop SessionRuntimeInfo).
fn session_runtime_info(state: &Arc<GatewayState>, session_id: &str) -> Value {
    let row = state.store.get_session_row(session_id).ok().flatten();
    let cwd = row
        .as_ref()
        .and_then(|r| r.cwd.clone())
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| {
            std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        });
    let branch = git_branch_for_cwd(std::path::Path::new(&cwd));
    let model = row
        .as_ref()
        .and_then(|r| r.model.clone())
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| state.model_name.clone());
    json!({
        "model": model,
        "provider": state.provider_name,
        "cwd": cwd,
        "branch": branch,
        "version": env!("CARGO_PKG_VERSION"),
        "desktop_contract": 5,
        "running": session_is_busy(session_id),
    })
}

fn format_epoch(ts: f64) -> String {
    chrono::DateTime::<chrono::Local>::from(
        chrono::DateTime::from_timestamp(ts as i64, 0).unwrap_or_default(),
    )
    .format("%Y-%m-%d %H:%M")
    .to_string()
}

/// `session.usage` — persisted token totals for the session (hermes
/// `_session_usage_snapshot`; the desktop /usage slash renders it).
fn session_usage(state: Arc<GatewayState>, params: Value) -> Result<Value, String> {
    let session_id = require_session_id(&params)?;
    let row = state
        .store
        .get_session_row(&session_id)
        .map_err(|e| e.to_string())?;
    let calls = state
        .store
        .load_message_rows(&session_id)
        .map(|rows| rows.iter().filter(|r| r.role == "assistant").count())
        .unwrap_or(0);
    let (input, output) = row
        .map(|r| (r.input_tokens, r.output_tokens))
        .unwrap_or((0, 0));
    Ok(json!({
        "calls": calls,
        "input": input,
        "output": output,
        "total": input + output,
    }))
}

/// `session.context_breakdown` — context-window fill estimate from the
/// persisted transcript (hermes `compute_session_context_breakdown`,
/// lean: no per-category attribution in this build).
fn session_context_breakdown(state: Arc<GatewayState>, params: Value) -> Result<Value, String> {
    let session_id = require_session_id(&params)?;
    let history: Vec<crate::provider::Message> = state
        .store
        .load_messages(&session_id)
        .unwrap_or_default()
        .into_iter()
        .filter(|m| m.role != crate::provider::Role::System)
        .collect();
    let used = crate::context::ContextCompressor::estimate_tokens(&history);
    let budget = state.agent.context_budget_tokens();
    let percent = if budget > 0 {
        ((used as f64) / (budget as f64) * 100.0).round()
    } else {
        0.0
    };
    let model = state
        .store
        .get_session_row(&session_id)
        .ok()
        .flatten()
        .and_then(|r| r.model)
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| state.model_name.clone());
    Ok(json!({
        "categories": [],
        "context_max": budget,
        "context_percent": percent,
        "context_used": used,
        "estimated_total": used,
        "model": model,
    }))
}

/// `session.status` — cockpit-style text block (hermes parity, /status).
fn session_status(state: Arc<GatewayState>, params: Value) -> Result<Value, String> {
    let session_id = require_session_id(&params)?;
    let row = state
        .store
        .get_session_row(&session_id)
        .map_err(|e| e.to_string())?;
    let running = session_is_busy(&session_id);
    let model = row
        .as_ref()
        .and_then(|r| r.model.clone())
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| state.model_name.clone());
    let mut lines = vec![
        "ulnclaw Gateway Status".to_string(),
        String::new(),
        format!("Session ID: {session_id}"),
        format!("Path: {}", crate::config::ulnclaw_home().display()),
    ];
    if let Some(row) = &row {
        if let Some(title) = row.title.as_deref().filter(|t| !t.is_empty()) {
            lines.push(format!("Title: {title}"));
        }
        lines.push(format!("Model: {model} ({})", state.provider_name));
        lines.push(format!("Created: {}", format_epoch(row.started_at)));
        lines.push(format!(
            "Last Activity: {}",
            format_epoch(row.last_activity_at)
        ));
        lines.push(format!("Tokens: {}", row.input_tokens + row.output_tokens));
    } else {
        lines.push(format!("Model: {model} ({})", state.provider_name));
    }
    lines.push(format!(
        "Agent Running: {}",
        if running { "Yes" } else { "No" }
    ));
    Ok(json!({"output": lines.join("\n")}))
}

/// `session.save` — snapshot the transcript under `<home>/sessions/saved/`
/// (hermes session.save; the desktop renders `Saved transcript to <file>`).
fn session_save(state: Arc<GatewayState>, params: Value) -> Result<Value, String> {
    let session_id = require_session_id(&params)?;
    let saved_dir = crate::config::ulnclaw_home().join("sessions").join("saved");
    std::fs::create_dir_all(&saved_dir).map_err(|e| {
        format!(
            "failed to create save directory {}: {e}",
            saved_dir.display()
        )
    })?;
    let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
    let path = saved_dir.join(format!("ulnclaw_conversation_{timestamp}.json"));
    let messages = state
        .store
        .load_messages(&session_id)
        .map_err(|e| e.to_string())?;
    let row = state.store.get_session_row(&session_id).ok().flatten();
    let model = row
        .as_ref()
        .and_then(|r| r.model.clone())
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| state.model_name.clone());
    let session_start = row
        .as_ref()
        .map(|r| {
            chrono::DateTime::from_timestamp(r.started_at as i64, 0)
                .unwrap_or_default()
                .to_rfc3339()
        })
        .unwrap_or_default();
    let payload = json!({
        "model": model,
        "session_id": session_id,
        "session_start": session_start,
        "system_prompt": "",
        "messages": history_to_wire(&messages),
    });
    let body = serde_json::to_string_pretty(&payload).map_err(|e| e.to_string())?;
    std::fs::write(&path, body).map_err(|e| e.to_string())?;
    Ok(json!({"file": path.display().to_string()}))
}

/// `session.branch` — copy (an optional prefix of) the parent transcript
/// into a new linked session (hermes session.branch; the desktop /branch
/// flow treats the reply as a SessionCreateResponse).
fn session_branch(state: Arc<GatewayState>, params: Value) -> Result<Value, String> {
    let parent_id = require_session_id(&params)?;
    let mut history: Vec<crate::provider::Message> = state
        .store
        .load_messages(&parent_id)
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|m| m.role != crate::provider::Role::System)
        .collect();
    if history.is_empty() {
        return Err("nothing to branch — send a message first".into());
    }
    if let Some(count) = params.get("count").and_then(Value::as_u64) {
        if count > 0 {
            history.truncate(count as usize);
        }
    }
    let parent_row = state.store.get_session_row(&parent_id).ok().flatten();
    let model = parent_row.as_ref().and_then(|r| r.model.clone());
    let new_id = state
        .store
        .create_child_session(&parent_id, "desktop", model.as_deref())
        .map_err(|e| e.to_string())?;
    for message in &history {
        state
            .store
            .append_message(&new_id, message)
            .map_err(|e| e.to_string())?;
    }
    let parent_title = parent_row
        .as_ref()
        .and_then(|r| r.title.clone())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "branch".to_string());
    let title = format!("{parent_title} (branch)");
    state
        .store
        .set_session_title(&new_id, &title)
        .map_err(|e| e.to_string())?;
    let wire = history_to_wire(&history);
    Ok(json!({
        "session_id": new_id,
        "stored_session_id": new_id,
        "title": title,
        "parent": parent_id,
        "message_count": wire.len(),
        "messages": wire,
        "info": session_runtime_info(&state, &new_id),
    }))
}

/// `session.compress` — LLM summarisation of the persisted transcript,
/// mirroring the gateway `/compress` slash (hermes session.compress). The
/// desktop replaces its transcript from `messages` and toasts `summary`.
async fn session_compress(state: Arc<GatewayState>, params: Value) -> Result<Value, String> {
    let session_id = require_session_id(&params)?;
    {
        let map = turns().lock().await;
        if map.contains_key(&session_id) {
            return Err("session busy — interrupt the current turn before compress".into());
        }
    }
    let history: Vec<crate::provider::Message> = state
        .store
        .load_messages(&session_id)
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|m| m.role != crate::provider::Role::System)
        .collect();
    let compressor = crate::context::ContextCompressor::new(state.agent.context_budget_tokens())
        .with_timezone(state.agent.context().config.timezone.clone());
    let before_messages = history.len();
    if before_messages <= compressor.keep_recent + 2 {
        return Ok(json!({
            "status": "aborted",
            "before_messages": before_messages,
            "after_messages": before_messages,
            "before_tokens": 0,
            "after_tokens": 0,
            "summary": {
                "headline": format!(
                    "nothing to compress — only {before_messages} message(s); needs more than {}",
                    compressor.keep_recent + 2
                ),
                "aborted": true,
            },
            "info": session_runtime_info(&state, &session_id),
            "messages": history_to_wire(&history),
        }));
    }
    let before_tokens = crate::context::ContextCompressor::estimate_tokens(&history);
    // Same auxiliary routing as the agent loop's auto-compression.
    let provider = state.agent.provider();
    let compressed = match crate::provider::auxiliary::resolve_aux_task(
        &state.agent.context().config,
        crate::provider::auxiliary::TASK_COMPRESSION,
        provider.clone(),
    ) {
        Ok(aux) => {
            compressor
                .compress_with_model(history.clone(), aux.provider.as_ref(), &aux.model)
                .await
        }
        Err(e) => {
            tracing::warn!(
                "auxiliary compression routing failed: {}; using main provider",
                e
            );
            compressor
                .compress_with_provider(history.clone(), provider.as_ref())
                .await
        }
    };
    let Some(compressed) = compressed else {
        return Err("compression failed or found nothing to summarize".into());
    };
    let after_messages = compressed.len();
    let after_tokens = crate::context::ContextCompressor::estimate_tokens(&compressed);
    state
        .store
        .replace_messages(&session_id, &compressed)
        .map_err(|e| format!("compression failed to persist: {e}"))?;
    publish(
        &session_id,
        "sessions.changed",
        json!({"session_id": session_id}),
    );
    Ok(json!({
        "status": "compressed",
        "before_messages": before_messages,
        "after_messages": after_messages,
        "before_tokens": before_tokens,
        "after_tokens": after_tokens,
        "summary": {
            "headline": format!(
                "compressed session context: {before_messages} → {after_messages} messages"
            ),
            "token_line": format!("~{before_tokens} → ~{after_tokens} tokens"),
            "aborted": false,
        },
        "info": session_runtime_info(&state, &session_id),
        "messages": history_to_wire(&compressed),
    }))
}

/// `session.redirect` — mid-turn correction. The ulnclaw agent has no
/// active-turn redirect, so a busy session gets the text queued as the next
/// turn (hermes build-window fallback); an idle session gets the honest
/// 4010-style error the renderer uses to fall back to a normal send.
fn session_redirect(state: Arc<GatewayState>, params: Value) -> Result<Value, String> {
    let text = params
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if text.is_empty() {
        return Err("text is required".into());
    }
    let session_id = require_session_id(&params)?;
    if !session_is_busy(&session_id) {
        return Err("agent does not support active-turn redirect".into());
    }
    state
        .queued_prompts
        .lock()
        .unwrap()
        .entry(session_id.clone())
        .or_default()
        .push_back(text.clone());
    crate::gateway::spawn_queue_drain(&state, &session_id);
    Ok(json!({"status": "queued", "text": text}))
}

/// `session.activate` — attach the renderer to an existing session without
/// closing the current one (hermes session.activate; SessionResumeResponse
/// shape, messages omitted on request).
fn session_activate(state: Arc<GatewayState>, params: Value) -> Result<Value, String> {
    let session_id = require_session_id(&params)?;
    state
        .store
        .ensure_session(&session_id, "desktop", None, None)
        .map_err(|e| e.to_string())?;
    let omit_messages = params
        .get("omit_messages")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let row = state.store.get_session_row(&session_id).ok().flatten();
    let messages: Vec<Value> = if omit_messages {
        Vec::new()
    } else {
        let history = state.store.load_messages(&session_id).unwrap_or_default();
        history_to_wire(&history)
    };
    let message_count = if omit_messages {
        row.map(|r| r.message_count as usize).unwrap_or(0)
    } else {
        messages.len()
    };
    Ok(json!({
        "resumed": session_id,
        "session_id": session_id,
        "session_key": session_id,
        "running": session_is_busy(&session_id),
        "message_count": message_count,
        "messages": messages,
        "messages_omitted": omit_messages,
        "info": session_runtime_info(&state, &session_id),
    }))
}

/// `session.active_list` — live in-process sessions with a running turn
/// (hermes session.active_list; the sidebar liveness poll).
async fn session_active_list(_params: Value) -> Result<Value, String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let map = turns().lock().await;
    let sessions: Vec<Value> = map
        .keys()
        .map(|id| {
            json!({
                "id": id,
                "session_key": id,
                "status": "working",
                "last_active": now,
            })
        })
        .collect();
    Ok(json!({"sessions": sessions}))
}

// ---------------------------------------------------------------------------
// Pet surface (hermes tui_gateway pet.* parity; fail-open `enabled:false`)
// ---------------------------------------------------------------------------

fn sheet_revision(path: &std::path::Path) -> String {
    match std::fs::metadata(path) {
        Ok(meta) => {
            let mtime_ns = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            format!("{mtime_ns}:{size}", size = meta.len())
        }
        Err(_) => "0:0".to_string(),
    }
}

/// Renderer sprite payload (hermes `_pet_sprite_payload`): spritesheet
/// bytes + frame geometry + state-row taxonomy for the desktop canvas.
fn pet_sprite_payload(pet: &crate::pets::InstalledPet, scale: f64) -> Result<Value, String> {
    use base64::Engine;
    let raw = std::fs::read(&pet.spritesheet).map_err(|e| e.to_string())?;
    let mime = match pet
        .spritesheet
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        _ => "image/webp",
    };
    let sheet = crate::pets::open_sheet(&pet.spritesheet);
    let (frames_by_state, state_rows): (Value, Vec<String>) = match &sheet {
        Some(image) => {
            let counts = crate::pets::state_frame_counts(image);
            let row_count = (image.height() / crate::pets::FRAME_H).max(1) as usize;
            let rows = crate::pets::state_rows_for_grid(Some(row_count as u32))
                .iter()
                .take(row_count)
                .map(|s| s.to_string())
                .collect();
            (json!(counts), rows)
        }
        None => (json!({}), Vec::new()),
    };
    Ok(json!({
        "slug": pet.slug,
        "displayName": pet.display_name,
        "mime": mime,
        "spritesheetBase64": base64::engine::general_purpose::STANDARD.encode(raw),
        "spritesheetRevision": sheet_revision(&pet.spritesheet),
        "frameW": crate::pets::FRAME_W,
        "frameH": crate::pets::FRAME_H,
        "framesPerState": crate::pets::FRAMES_PER_STATE,
        "framesByState": frames_by_state,
        "loopMs": crate::pets::LOOP_MS,
        "scale": scale,
        "stateRows": state_rows,
    }))
}

fn active_pet_selection() -> (bool, Option<crate::pets::InstalledPet>, f64) {
    let config = crate::pets::read_pet_config();
    if !config.enabled {
        return (false, None, config.scale);
    }
    let home = crate::config::ulnclaw_home();
    let slug = if config.slug.is_empty() {
        None
    } else {
        Some(config.slug.as_str())
    };
    (
        true,
        crate::pets::resolve_active_pet(&home, slug),
        config.scale,
    )
}

/// `pet.info` — active pet sprite payload for the desktop canvas.
fn pet_info() -> Value {
    let (enabled, pet, scale) = active_pet_selection();
    if !enabled {
        return json!({"enabled": false});
    }
    let Some(pet) = pet.filter(|p| p.exists()) else {
        return json!({"enabled": false});
    };
    match pet_sprite_payload(&pet, scale) {
        Ok(mut payload) => {
            if let Some(obj) = payload.as_object_mut() {
                obj.insert("enabled".into(), json!(true));
            }
            payload
        }
        Err(_) => json!({"enabled": false}),
    }
}

/// `pet.info.meta` — cheap active-pet metadata (no sprite bytes).
fn pet_info_meta() -> Value {
    let (enabled, pet, scale) = active_pet_selection();
    if !enabled {
        return json!({"enabled": false});
    }
    let Some(pet) = pet.filter(|p| p.exists()) else {
        return json!({"enabled": false});
    };
    json!({
        "enabled": true,
        "slug": pet.slug,
        "displayName": pet.display_name,
        "scale": scale,
        "spritesheetRevision": sheet_revision(&pet.spritesheet),
    })
}

/// `pet.gallery` — installed pets (and, unless `localOnly`, the petdex
/// manifest) for the desktop picker (hermes pet.gallery, fail-open).
fn pet_gallery(params: Value) -> Result<Value, String> {
    let local_only = params
        .get("localOnly")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let config = crate::pets::read_pet_config();
    let home = crate::config::ulnclaw_home();
    let local = crate::pets::installed_pets(&home);
    let mut pets: Vec<Value> = local
        .iter()
        .map(|pet| {
            json!({
                "slug": pet.slug,
                "displayName": pet.display_name,
                "installed": true,
                "generated": pet.generated(),
            })
        })
        .collect();
    if !local_only {
        if let Ok(entries) = crate::pets::fetch_manifest(false) {
            for entry in entries {
                if pets.iter().any(|p| p["slug"] == entry.slug) {
                    continue;
                }
                pets.push(json!({
                    "slug": entry.slug,
                    "displayName": entry.display_name,
                    "installed": false,
                    "spritesheetUrl": entry.spritesheet_url,
                    "curated": true,
                }));
            }
        }
    }
    Ok(json!({"enabled": config.enabled, "active": config.slug, "pets": pets}))
}

/// `pet.thumb` — small idle-frame PNG data URI for the picker preview.
fn pet_thumb(params: Value) -> Result<Value, String> {
    use base64::Engine;
    let slug = params
        .get("slug")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if slug.is_empty() {
        return Err("missing slug".into());
    }
    let home = crate::config::ulnclaw_home();
    let source_url = params
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let Some(data) = crate::pets::thumbnail_png(&home, &slug, source_url) else {
        return Ok(json!({"ok": false, "slug": slug}));
    };
    Ok(json!({
        "ok": true,
        "slug": slug,
        "dataUri": format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(data)
        ),
    }))
}

/// `pet.scale` — persist the desktop slider (clamped to engine bounds).
fn pet_scale(params: Value) -> Result<Value, String> {
    let value = match params.get("scale") {
        Some(Value::Number(number)) => number.to_string(),
        Some(Value::String(text)) => text.clone(),
        _ => return Err("scale is required".into()),
    };
    let scale = crate::pets::set_pet_scale(&value)?;
    Ok(json!({"ok": true, "scale": scale}))
}

/// `pet.disable` — turn the pet off (`display.pet.enabled=false`).
fn pet_disable() -> Result<Value, String> {
    crate::pets::set_enabled(false)?;
    Ok(json!({"ok": true}))
}

/// `pet.export` — zip an installed pet (pet.json + sprite) as base64.
fn pet_export(params: Value) -> Result<Value, String> {
    use base64::Engine;
    let slug = params
        .get("slug")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if slug.is_empty() {
        return Err("missing slug".into());
    }
    let home = crate::config::ulnclaw_home();
    let (filename, data) =
        crate::pets::export_pet(&home, &slug).map_err(|e| format!("pet.export failed: {e}"))?;
    Ok(json!({
        "ok": true,
        "filename": filename,
        "zipBase64": base64::engine::general_purpose::STANDARD.encode(data),
    }))
}

// ---------------------------------------------------------------------------
// Browser / command dispatch / handoff / preview / setup (hermes parity)
// ---------------------------------------------------------------------------

/// `browser.manage` — status/connect/disconnect for the CDP browser tools
/// (hermes browser.manage; mirrors the REST /v1/browser/connect flow).
async fn browser_manage(params: Value) -> Result<Value, String> {
    let action = params
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("status");
    match action {
        "status" => {
            let endpoint = crate::browser::endpoint_with_source();
            Ok(json!({
                "connected": endpoint.is_some(),
                "url": endpoint.map(|(_, url)| url).unwrap_or_default(),
            }))
        }
        "disconnect" => {
            crate::browser::clear_cdp_override();
            Ok(json!({"connected": false, "url": ""}))
        }
        "connect" => {
            let url = params
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string();
            if !url.is_empty() {
                crate::browser::set_cdp_override(&url).map_err(|e| e.to_string())?;
                let resolved = crate::browser::endpoint_with_source()
                    .map(|(_, raw)| raw)
                    .unwrap_or(url);
                return Ok(json!({
                    "connected": true,
                    "url": resolved,
                    "messages": [format!("✓ CDP endpoint set to {resolved}")],
                }));
            }
            let outcome = crate::browser::connect::connect_local_default(9222).await;
            match outcome.url {
                Some(found) => {
                    let _ = crate::browser::set_cdp_override(&found);
                    Ok(json!({
                        "connected": true,
                        "url": found,
                        "messages": outcome.messages,
                    }))
                }
                None => Err(if outcome.messages.is_empty() {
                    "browser connect failed".to_string()
                } else {
                    outcome.messages.join("\n")
                }),
            }
        }
        other => Err(format!("unknown action: {other}")),
    }
}

/// `command.dispatch` — quick/plugin/skill command fallback after
/// slash.exec (hermes command.dispatch). The ulnclaw build routes the
/// gateway-owned `/goal` command and reports everything else with the
/// exact fallback wording the renderer matches on.
fn command_dispatch(state: Arc<GatewayState>, params: Value) -> Result<Value, String> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .trim_start_matches('/')
        .to_string();
    let arg = params
        .get("arg")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if name == "goal" {
        let result = slash_exec(
            state,
            json!({
                "session_id": params.get("session_id").cloned().unwrap_or(Value::Null),
                "command": format!("goal {arg}"),
            }),
        )?;
        return Ok(json!({
            "type": "exec",
            "output": result.get("output").cloned().unwrap_or_default(),
        }));
    }
    Err(format!("not a quick/plugin/skill command: {name}"))
}

/// `handoff.request` — session handoff to a messaging platform needs the
/// hermes gateway's handoff watcher; this build reports it unsupported.
fn handoff_request(params: Value) -> Result<Value, String> {
    let platform = params
        .get("platform")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if platform.is_empty() {
        return Err("platform required".into());
    }
    Err("handoff is not supported — this ulnclaw gateway build has no messaging-platform handoff watcher".into())
}

/// `handoff.state` — poll shape (empty record = no handoff in flight).
fn handoff_state(params: Value) -> Result<Value, String> {
    require_session_id(&params)?;
    Ok(json!({"state": "", "platform": "", "error": ""}))
}

/// `preview.restart` — relaunch the dev server behind a preview URL in a
/// detached background turn (hermes preview.restart).
fn preview_restart(state: Arc<GatewayState>, params: Value) -> Result<Value, String> {
    let url = params
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if url.is_empty() {
        return Err("url required".into());
    }
    let cwd = params
        .get("cwd")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    let context = params
        .get("context")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    let task_id = format!(
        "preview_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..6]
    );
    let console_block = if context.is_empty() {
        String::new()
    } else {
        format!("Preview console:\n{context}\n")
    };
    let prompt = format!(
        "The desktop preview pane cannot load a local server URL.\n\n\
         Preview URL: {url}\n\
         Current working directory: {cwd}\n\n\
         {console_block}\
         Restart exactly the app intended for the Preview URL, not the desktop app itself.\n\
         The Preview URL and port are the target. Preserve that target unless you conclude it is impossible.\n\
         First inspect what process, if any, owns the Preview URL port. If a stale server exists, inspect its cwd and prefer that cwd over the desktop process cwd.\n\
         The Current working directory is only a hint. Do not assume it is the preview app root when the port owner or files indicate another root.\n\
         If the console shows a module-script MIME error for src/main.tsx or similar, a static server is serving source files. Do not restart a dumb static server for that app; start the real dev server/bundler instead.\n\
         Before declaring success, verify the Preview URL responds with the intended app.\n\
         Do not modify files. Do not ask the user unless blocked.\n\
         Prefer existing project scripts or commands when they are clear.\n\
         If a stale process owns the needed port, handle it safely.\n\
         Start long-running servers detached/in the background, then return immediately.\n\
         Keep the final response short: what command/server was started, or why it could not be restarted."
    );
    let agent = state.agent.clone();
    let session = task_id.clone();
    tokio::spawn(async move {
        if let Err(e) = agent
            .run_with_session_images(&prompt, vec![], None, Some(&session))
            .await
        {
            tracing::warn!("preview.restart task {session} failed: {e}");
        }
    });
    Ok(json!({"task_id": task_id}))
}

/// True when any provider auth state is discoverable (hermes
/// `_has_any_provider_configured`): an API key resolves, or the provider
/// is a keyless local runtime.
fn provider_configured(config: &crate::config::UlncLawConfig) -> bool {
    if config.resolve_api_key().is_some() {
        return true;
    }
    matches!(
        config.model.provider.as_str(),
        "ollama" | "llamacpp" | "llama_cpp" | "local" | "moa"
    )
}

/// `setup.status` — onboarding probe (any provider configured?).
fn setup_status() -> Value {
    let config = crate::config::UlncLawConfig::load(None).unwrap_or_default();
    json!({"provider_configured": provider_configured(&config)})
}

/// `setup.runtime_check` — strict check that the gateway's configured
/// model can actually be served (hermes setup.runtime_check).
fn setup_runtime_check(state: &Arc<GatewayState>) -> Value {
    let config = crate::config::UlncLawConfig::load(None).unwrap_or_default();
    if provider_configured(&config) {
        json!({
            "ok": true,
            "provider": state.provider_name,
            "model": state.model_name,
            "source": "gateway",
        })
    } else {
        json!({
            "ok": false,
            "provider": state.provider_name,
            "model": state.model_name,
            "source": "gateway",
            "error": format!("No usable credentials found for {}.", state.provider_name),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Agent;
    use crate::gateway::ApprovalRouter;
    use crate::provider::openai::OpenAiProvider;
    use crate::session::sqlite::SqliteSessionStore;
    use crate::tools::ToolRegistry;

    fn test_state() -> Arc<GatewayState> {
        let temp = tempfile::tempdir().expect("tempdir");
        let store =
            Arc::new(SqliteSessionStore::open(temp.path().join("state.db")).expect("store opens"));
        std::mem::forget(temp);
        let provider = Arc::new(
            OpenAiProvider::builder()
                .endpoint("http://127.0.0.1:9/v1")
                .model("test-model")
                .name("test")
                .build()
                .expect("provider builds"),
        );
        let agent = Agent::new(provider, ToolRegistry::new()).with_store(store);
        GatewayState::new(
            Arc::new(agent),
            "test-model".into(),
            "test".into(),
            Some("sekret".into()),
            ApprovalRouter::new(),
        )
        .expect("state builds")
    }

    #[tokio::test]
    async fn wake_methods_report_unavailable_gracefully() {
        let state = test_state();
        let status = dispatch(state.clone(), "wake.status", json!({}))
            .await
            .unwrap();
        assert_eq!(status["listening"], false);
        assert_eq!(status["available"], false);
        assert!(status["hint"].as_str().unwrap().contains("not built"));

        let start = dispatch(state.clone(), "wake.start", json!({"persist": true}))
            .await
            .unwrap();
        assert_eq!(start["started"], false);
        assert!(start["hint"].as_str().is_some());

        let stop = dispatch(state, "wake.stop", json!({})).await.unwrap();
        assert_eq!(stop["stopped"], false);
        assert_eq!(stop["reason"], "not_owner");
    }

    #[tokio::test]
    async fn pet_generate_status_shape() {
        let state = test_state();
        let status = dispatch(state, "pet.generate.status", json!({}))
            .await
            .unwrap();
        assert!(status.get("available").is_some());
        assert!(status["providers"].is_array());
    }

    #[tokio::test]
    async fn pet_methods_validate_params() {
        let state = test_state();
        assert!(dispatch(state.clone(), "pet.generate", json!({}))
            .await
            .is_err());
        assert!(dispatch(state.clone(), "pet.hatch", json!({"name": "x"}))
            .await
            .is_err());
        assert!(dispatch(state.clone(), "pet.hatch", json!({"token": "t"}))
            .await
            .is_err());
        assert!(dispatch(
            state.clone(),
            "pet.hatch",
            json!({"token": "nope", "index": 0, "name": "x"})
        )
        .await
        .is_err());
        assert!(dispatch(state.clone(), "pet.rename", json!({"slug": "s"}))
            .await
            .is_err());
        assert!(
            dispatch(state.clone(), "pet.select", json!({"slug": "ghost"}))
                .await
                .is_err()
        );

        let removed = dispatch(state.clone(), "pet.remove", json!({"slug": "ghost"}))
            .await
            .unwrap();
        assert_eq!(removed["ok"], false);

        let cancel = dispatch(state, "pet.cancel", json!({"token": "unknown"}))
            .await
            .unwrap();
        assert_eq!(cancel["ok"], true);
    }

    #[tokio::test]
    async fn prompt_respond_follows_hermes_expired_semantics() {
        let state = test_state();
        // No request id at all -> hard error the renderer surfaces.
        let err = dispatch(state.clone(), "sudo.respond", json!({})).await;
        assert!(err.unwrap_err().contains("no pending password request"));

        // Late answer for a known-but-unpending id -> expired, not an error.
        let expired = dispatch(
            state.clone(),
            "sudo.respond",
            json!({"request_id": "r1", "password": "x"}),
        )
        .await
        .unwrap();
        assert_eq!(expired["status"], "expired");

        // A registered prompt is answered over the WS method.
        let rx = request_prompt("r2");
        let ok = dispatch(
            state.clone(),
            "secret.respond",
            json!({"request_id": "r2", "value": "sekret"}),
        )
        .await
        .unwrap();
        assert_eq!(ok["status"], "ok");
        assert_eq!(rx.await.unwrap(), "sekret");

        // Answering twice reports expired (entry consumed).
        let again = dispatch(
            state,
            "secret.respond",
            json!({"request_id": "r2", "value": "x"}),
        )
        .await
        .unwrap();
        assert_eq!(again["status"], "expired");
    }

    #[tokio::test]
    async fn message_react_persists_and_publishes() {
        let state = test_state();
        state
            .store
            .ensure_session("s-react", "desktop", None, None)
            .expect("session");
        state
            .store
            .append_message(
                "s-react",
                &crate::provider::Message {
                    role: crate::provider::Role::User,
                    content: Some("hello".into()),
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                },
            )
            .expect("append");
        let result = dispatch(
            state.clone(),
            "message.react",
            json!({"session_id": "s-react", "emoji": "👍", "author": "user"}),
        )
        .await
        .unwrap();
        assert_eq!(result["ok"], true);
        let row = result["row_id"].as_i64().unwrap();
        let reactions = result["reactions"].as_array().unwrap();
        assert_eq!(reactions.len(), 1);
        assert_eq!(reactions[0]["emoji"], "👍");

        // Toggling the same emoji retracts it.
        let result = dispatch(
            state,
            "message.react",
            json!({"session_id": "s-react", "row_id": row, "emoji": "👍", "author": "user"}),
        )
        .await
        .unwrap();
        assert_eq!(result["reactions"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn slash_exec_goal_lifecycle() {
        let state = test_state();
        state
            .store
            .ensure_session("s-goal", "desktop", None, None)
            .expect("session");

        // No goal yet -> status line says so.
        let status = dispatch(
            state.clone(),
            "slash.exec",
            json!({"session_id": "s-goal", "command": "goal status"}),
        )
        .await
        .unwrap();
        assert!(status["output"]
            .as_str()
            .unwrap()
            .starts_with("No active goal"));

        // Seed a goal through the manager, then pause/resume/clear over WS.
        let mut manager = crate::goals::GoalManager::new("s-goal", Some(state.store.clone()), 5);
        manager.set("ship the desktop", None, None);

        let paused = dispatch(
            state.clone(),
            "slash.exec",
            json!({"session_id": "s-goal", "command": "/goal pause"}),
        )
        .await
        .unwrap();
        assert_eq!(paused["output"], "⏸ Goal paused: ship the desktop");

        let resumed = dispatch(
            state.clone(),
            "slash.exec",
            json!({"session_id": "s-goal", "command": "goal resume"}),
        )
        .await
        .unwrap();
        assert_eq!(resumed["output"], "▶ Goal resumed: ship the desktop");

        let status = dispatch(
            state.clone(),
            "slash.exec",
            json!({"session_id": "s-goal", "command": "goal"}),
        )
        .await
        .unwrap();
        assert!(status["output"]
            .as_str()
            .unwrap()
            .contains("ship the desktop"));

        let cleared = dispatch(
            state.clone(),
            "slash.exec",
            json!({"session_id": "s-goal", "command": "goal clear"}),
        )
        .await
        .unwrap();
        assert_eq!(cleared["output"], "✓ Goal cleared.");

        assert!(dispatch(
            state,
            "slash.exec",
            json!({"session_id": "s-goal", "command": "goal bogus"}),
        )
        .await
        .is_err());
    }

    #[tokio::test]
    async fn reload_mcp_requires_confirm() {
        let state = test_state();
        let skipped = dispatch(state.clone(), "reload.mcp", json!({}))
            .await
            .unwrap();
        assert!(skipped["warning"].as_str().is_some());

        let done = dispatch(state, "reload.mcp", json!({"confirm": true}))
            .await
            .unwrap();
        assert_eq!(done["ok"], true);
        assert!(done["output"]
            .as_str()
            .unwrap()
            .contains("No MCP servers connected"));
    }

    #[tokio::test]
    async fn reload_env_reports_applied_count() {
        let state = test_state();
        let result = dispatch(state, "reload.env", json!({})).await.unwrap();
        assert_eq!(result["ok"], true);
        assert!(result["applied"].is_u64());
    }

    #[tokio::test]
    async fn session_create_desktop_contract() {
        let state = test_state();

        // Desktop composer create: model override + sticky picks persist and
        // reflect back in info (use-session-actions contract).
        let created = dispatch(
            state.clone(),
            "session.create",
            json!({
                "cols": 96,
                "source": "desktop",
                "model": "gpt-test-deluxe",
                "provider": "test",
                "reasoning_effort": "high",
                "fast": true
            }),
        )
        .await
        .unwrap();
        let session_id = created["session_id"]
            .as_str()
            .expect("session_id")
            .to_string();
        assert!(!session_id.is_empty());
        assert_eq!(created["stored_session_id"], created["session_id"]);
        assert_eq!(created["message_count"], 0);
        assert!(created["messages"].as_array().unwrap().is_empty());
        let info = &created["info"];
        assert_eq!(info["model"], "gpt-test-deluxe");
        assert_eq!(info["provider"], "test");
        assert_eq!(info["desktop_contract"], 5);
        assert_eq!(info["lazy"], true);
        assert_eq!(info["reasoning_effort"], "high");
        assert_eq!(info["fast"], true);
        // Override persisted on the row so prompt turns enforce it.
        let row = state
            .store
            .get_session_row(&session_id)
            .unwrap()
            .expect("row persisted for create with model override");
        assert_eq!(row.model.as_deref(), Some("gpt-test-deluxe"));

        // Bare create stays rowless (hermes lazy-row contract: no empty
        // sessions for launches that never type) and falls back to the
        // gateway's global model/provider.
        let bare = dispatch(
            state.clone(),
            "session.create",
            json!({"source": "desktop"}),
        )
        .await
        .unwrap();
        let bare_id = bare["session_id"].as_str().unwrap().to_string();
        assert!(state.store.get_session_row(&bare_id).unwrap().is_none());
        assert_eq!(bare["info"]["model"], "test-model");
        assert_eq!(bare["info"]["provider"], "test");

        // session.close on a rowless draft is a clean no-op (drift abort path).
        let closed = dispatch(
            state.clone(),
            "session.close",
            json!({"session_id": bare_id}),
        )
        .await
        .unwrap();
        assert_eq!(closed["ok"], true);
    }

    /// Serialize tests that repoint ULNCLAW_HOME (process-global env).
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn seed_message(
        state: &Arc<GatewayState>,
        session_id: &str,
        role: crate::provider::Role,
        text: &str,
    ) {
        state
            .store
            .append_message(
                session_id,
                &crate::provider::Message {
                    role,
                    content: Some(text.to_string()),
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                },
            )
            .expect("message appends");
    }

    #[tokio::test]
    async fn billing_methods_fail_open_without_portal() {
        let state = test_state();
        let billing = dispatch(state.clone(), "billing.state", json!({}))
            .await
            .unwrap();
        assert_eq!(billing["ok"], true);
        assert_eq!(billing["logged_in"], false);

        let subscription = dispatch(state.clone(), "subscription.state", json!({}))
            .await
            .unwrap();
        assert_eq!(subscription["ok"], true);
        assert_eq!(subscription["logged_in"], false);

        let usage = dispatch(state.clone(), "usage.bars", json!({}))
            .await
            .unwrap();
        assert_eq!(usage["ok"], true);
        assert_eq!(usage["available"], false);

        for method in [
            "billing.charge",
            "billing.charge_status",
            "billing.step_up",
            "billing.auto_reload",
            "subscription.preview",
            "subscription.change",
            "subscription.resume",
            "subscription.upgrade",
        ] {
            let result = dispatch(state.clone(), method, json!({})).await.unwrap();
            assert_eq!(result["ok"], false, "{method} should be unavailable");
            assert_eq!(
                result["error"], "unavailable",
                "{method} should carry the typed error"
            );
        }
    }

    #[tokio::test]
    async fn session_usage_and_context_breakdown_read_persisted_totals() {
        let state = test_state();
        state
            .store
            .ensure_session("sess-usage", "desktop", None, None)
            .unwrap();
        seed_message(
            &state,
            "sess-usage",
            crate::provider::Role::User,
            "hello there",
        );
        seed_message(
            &state,
            "sess-usage",
            crate::provider::Role::Assistant,
            "hi!",
        );
        state.store.update_usage("sess-usage", 11, 7, 0).unwrap();

        let usage = dispatch(
            state.clone(),
            "session.usage",
            json!({"session_id": "sess-usage"}),
        )
        .await
        .unwrap();
        assert_eq!(usage["input"], 11);
        assert_eq!(usage["output"], 7);
        assert_eq!(usage["total"], 18);
        assert_eq!(usage["calls"], 1);

        let breakdown = dispatch(
            state.clone(),
            "session.context_breakdown",
            json!({"session_id": "sess-usage"}),
        )
        .await
        .unwrap();
        assert_eq!(breakdown["categories"], json!([]));
        assert!(breakdown["context_max"].as_u64().unwrap() > 0);
        assert!(breakdown["context_used"].as_u64().unwrap() > 0);
        assert_eq!(breakdown["model"], "test-model");

        let missing = dispatch(state, "session.usage", json!({})).await;
        assert!(missing.unwrap_err().contains("session_id is required"));
    }

    #[tokio::test]
    async fn session_status_renders_cockpit_text() {
        let state = test_state();
        state
            .store
            .ensure_session("sess-status", "desktop", None, None)
            .unwrap();
        state
            .store
            .set_session_title("sess-status", "Status Probe")
            .unwrap();
        let result = dispatch(
            state,
            "session.status",
            json!({"session_id": "sess-status"}),
        )
        .await
        .unwrap();
        let output = result["output"].as_str().unwrap();
        assert!(output.contains("Session ID: sess-status"));
        assert!(output.contains("Title: Status Probe"));
        assert!(output.contains("Agent Running: No"));
    }

    #[tokio::test]
    async fn session_create_accepts_seed_messages_and_parent() {
        let state = test_state();
        state
            .store
            .ensure_session("parent-1", "desktop", None, None)
            .unwrap();
        let created = dispatch(
            state.clone(),
            "session.create",
            json!({
                "source": "desktop",
                "parent_session_id": "parent-1",
                "messages": [
                    {"role": "user", "content": "first"},
                    {"role": "assistant", "content": "second"},
                ],
            }),
        )
        .await
        .unwrap();
        let new_id = created["session_id"].as_str().unwrap().to_string();
        assert_eq!(created["message_count"], 2);
        assert_eq!(created["messages"][0]["role"], "user");
        assert_eq!(created["messages"][1]["content"], "second");

        let row = state
            .store
            .get_session_row(&new_id)
            .unwrap()
            .expect("seeded create persists");
        assert_eq!(row.parent_session_id.as_deref(), Some("parent-1"));
        let history = state.store.load_messages(&new_id).unwrap();
        assert_eq!(history.len(), 2);
    }

    #[tokio::test]
    async fn session_branch_copies_parent_history() {
        let state = test_state();
        state
            .store
            .ensure_session("branch-parent", "desktop", None, None)
            .unwrap();
        state
            .store
            .set_session_title("branch-parent", "Original")
            .unwrap();
        seed_message(&state, "branch-parent", crate::provider::Role::User, "one");
        seed_message(
            &state,
            "branch-parent",
            crate::provider::Role::Assistant,
            "two",
        );
        seed_message(
            &state,
            "branch-parent",
            crate::provider::Role::User,
            "three",
        );

        let branched = dispatch(
            state.clone(),
            "session.branch",
            json!({"session_id": "branch-parent", "count": 2}),
        )
        .await
        .unwrap();
        let new_id = branched["session_id"].as_str().unwrap().to_string();
        assert_eq!(branched["parent"], "branch-parent");
        assert_eq!(branched["message_count"], 2);
        assert_eq!(branched["title"], "Original (branch)");
        assert_eq!(branched["messages"][1]["content"], "two");

        let row = state
            .store
            .get_session_row(&new_id)
            .unwrap()
            .expect("branch row exists");
        assert_eq!(row.parent_session_id.as_deref(), Some("branch-parent"));
        assert_eq!(state.store.load_messages(&new_id).unwrap().len(), 2);

        // Empty session refuses to branch.
        state
            .store
            .ensure_session("branch-empty", "desktop", None, None)
            .unwrap();
        let err = dispatch(
            state,
            "session.branch",
            json!({"session_id": "branch-empty"}),
        )
        .await
        .unwrap_err();
        assert!(err.contains("nothing to branch"));
    }

    #[tokio::test]
    async fn session_compress_short_history_aborts_without_llm() {
        let state = test_state();
        state
            .store
            .ensure_session("sess-compress", "desktop", None, None)
            .unwrap();
        seed_message(
            &state,
            "sess-compress",
            crate::provider::Role::User,
            "hello",
        );
        seed_message(
            &state,
            "sess-compress",
            crate::provider::Role::Assistant,
            "hi",
        );

        let result = dispatch(
            state,
            "session.compress",
            json!({"session_id": "sess-compress"}),
        )
        .await
        .unwrap();
        assert_eq!(result["status"], "aborted");
        assert_eq!(result["summary"]["aborted"], true);
        assert!(result["messages"].as_array().unwrap().len() == 2);
    }

    #[tokio::test]
    async fn session_redirect_queues_when_busy_and_rejects_when_idle() {
        let state = test_state();
        // Idle session: honest 4010-style refusal.
        let err = dispatch(
            state.clone(),
            "session.redirect",
            json!({"session_id": "sess-redir", "text": "correction"}),
        )
        .await
        .unwrap_err();
        assert!(err.contains("does not support active-turn redirect"));

        // Busy session: the correction is queued for the next turn.
        let task = tokio::spawn(async { std::future::pending::<()>().await });
        turns().lock().await.insert(
            "sess-redir".to_string(),
            TurnEntry {
                abort: task.abort_handle(),
            },
        );
        let result = dispatch(
            state.clone(),
            "session.redirect",
            json!({"session_id": "sess-redir", "text": "correction"}),
        )
        .await
        .unwrap();
        assert_eq!(result["status"], "queued");
        assert_eq!(result["text"], "correction");
        turns().lock().await.remove("sess-redir");
        task.abort();

        // Missing text is a parameter error.
        let err = dispatch(state, "session.redirect", json!({"session_id": "s"}))
            .await
            .unwrap_err();
        assert!(err.contains("text is required"));
    }

    #[tokio::test]
    async fn session_activate_and_active_list_shapes() {
        let state = test_state();
        let activated = dispatch(
            state.clone(),
            "session.activate",
            json!({"session_id": "sess-act", "omit_messages": true}),
        )
        .await
        .unwrap();
        assert_eq!(activated["resumed"], "sess-act");
        assert_eq!(activated["session_key"], "sess-act");
        assert_eq!(activated["messages_omitted"], true);
        assert_eq!(activated["info"]["model"], "test-model");
        assert_eq!(activated["info"]["desktop_contract"], 5);

        let list = dispatch(state.clone(), "session.active_list", json!({}))
            .await
            .unwrap();
        assert_eq!(list["sessions"], json!([]));

        // Activating a session with persisted history returns it unless omitted.
        state
            .store
            .ensure_session("sess-act2", "desktop", None, None)
            .unwrap();
        seed_message(&state, "sess-act2", crate::provider::Role::User, "ping");
        let resumed = dispatch(
            state,
            "session.activate",
            json!({"session_id": "sess-act2"}),
        )
        .await
        .unwrap();
        assert_eq!(resumed["message_count"], 1);
        assert_eq!(resumed["messages"][0]["content"], "ping");
    }

    #[tokio::test]
    async fn session_save_writes_transcript_file() {
        let _guard = ENV_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().expect("tempdir");
        std::env::set_var("ULNCLAW_HOME", temp.path());

        let state = test_state();
        state
            .store
            .ensure_session("sess-save", "desktop", None, None)
            .unwrap();
        seed_message(&state, "sess-save", crate::provider::Role::User, "save me");

        let result = dispatch(state, "session.save", json!({"session_id": "sess-save"}))
            .await
            .unwrap();
        let file = result["file"].as_str().expect("file path");
        let body = std::fs::read_to_string(file).expect("transcript written");
        let parsed: Value = serde_json::from_str(&body).expect("valid json");
        assert_eq!(parsed["session_id"], "sess-save");
        assert_eq!(parsed["messages"][0]["content"], "save me");
        std::env::remove_var("ULNCLAW_HOME");
    }

    #[tokio::test]
    async fn pet_surface_fails_open_without_installed_pets() {
        let _guard = ENV_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().expect("tempdir");
        std::env::set_var("ULNCLAW_HOME", temp.path());

        let state = test_state();
        let info = dispatch(state.clone(), "pet.info", json!({}))
            .await
            .unwrap();
        assert_eq!(info["enabled"], false);

        let meta = dispatch(state.clone(), "pet.info.meta", json!({}))
            .await
            .unwrap();
        assert_eq!(meta["enabled"], false);

        let gallery = dispatch(state.clone(), "pet.gallery", json!({"localOnly": true}))
            .await
            .unwrap();
        assert_eq!(gallery["enabled"], false);
        assert_eq!(gallery["pets"], json!([]));

        let missing = dispatch(state, "pet.thumb", json!({"slug": "ghost"}))
            .await
            .unwrap();
        assert_eq!(missing["ok"], false);
        std::env::remove_var("ULNCLAW_HOME");
    }

    #[tokio::test]
    async fn browser_manage_status_and_disconnect() {
        let state = test_state();
        crate::browser::clear_cdp_override();
        let status = dispatch(state.clone(), "browser.manage", json!({"action": "status"}))
            .await
            .unwrap();
        assert_eq!(status["connected"], false);

        let unknown = dispatch(state.clone(), "browser.manage", json!({"action": "bogus"})).await;
        assert!(unknown.unwrap_err().contains("unknown action"));

        let disconnect = dispatch(state, "browser.manage", json!({"action": "disconnect"}))
            .await
            .unwrap();
        assert_eq!(disconnect["connected"], false);
    }

    #[tokio::test]
    async fn command_dispatch_routes_goal_and_reports_fallback() {
        let state = test_state();
        let goal = dispatch(
            state.clone(),
            "command.dispatch",
            json!({"session_id": "sess-cmd", "name": "goal", "arg": ""}),
        )
        .await
        .unwrap();
        assert_eq!(goal["type"], "exec");
        assert!(goal["output"].as_str().unwrap().contains("No active goal"));

        let err = dispatch(
            state,
            "command.dispatch",
            json!({"session_id": "sess-cmd", "name": "frobnicate", "arg": ""}),
        )
        .await
        .unwrap_err();
        assert!(err.contains("not a quick/plugin/skill command: frobnicate"));
    }

    #[tokio::test]
    async fn handoff_methods_report_unsupported() {
        let state = test_state();
        let err = dispatch(
            state.clone(),
            "handoff.request",
            json!({"session_id": "s", "platform": "telegram"}),
        )
        .await
        .unwrap_err();
        assert!(err.contains("handoff is not supported"));

        let missing = dispatch(state.clone(), "handoff.request", json!({"session_id": "s"})).await;
        assert!(missing.unwrap_err().contains("platform required"));

        let poll = dispatch(state, "handoff.state", json!({"session_id": "s"}))
            .await
            .unwrap();
        assert_eq!(poll["state"], "");
        assert_eq!(poll["platform"], "");
    }

    #[tokio::test]
    async fn preview_restart_requires_url() {
        let state = test_state();
        let err = dispatch(state, "preview.restart", json!({}))
            .await
            .unwrap_err();
        assert!(err.contains("url required"));
    }

    #[tokio::test]
    async fn setup_methods_report_provider_state() {
        let state = test_state();
        let status = dispatch(state.clone(), "setup.status", json!({}))
            .await
            .unwrap();
        assert!(status.get("provider_configured").is_some());

        let check = dispatch(state, "setup.runtime_check", json!({}))
            .await
            .unwrap();
        assert!(check.get("ok").is_some());
        assert_eq!(check["provider"], "test");
        assert_eq!(check["model"], "test-model");
    }
}

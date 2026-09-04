//! Platform tools — Home Assistant, kanban, browser, computer-use, and
//! messaging platform stubs.
//!
//! Home Assistant tools are fully implemented (REST API, gated on
//! HASS_URL + HASS_TOKEN). Kanban implements a local coordination board in
//! SQLite. Browser/computer-use/messaging tools register with hermes-faithful
//! schemas but gate on their backends being configured.

use crate::error::Result;
use crate::tools::{tool, ToolAvailability, ToolContext, ToolRegistry};
use serde_json::json;

pub fn register(registry: &mut ToolRegistry) {
    register_homeassistant(registry);
    register_kanban(registry);
    register_browser(registry);
    register_gated_stubs(registry);
}

// ---------------------------------------------------------------------------
// Home Assistant (ha_list_entities, ha_get_state, ha_list_services, ha_call_service)
// ---------------------------------------------------------------------------

fn hass_configured() -> ToolAvailability {
    if crate::config::get_env_value("HASS_TOKEN").is_some()
        && crate::config::get_env_value("HASS_URL").is_some()
    {
        ToolAvailability::available()
    } else {
        ToolAvailability::unavailable("HASS_URL and HASS_TOKEN must be set")
    }
}

async fn hass_request(
    ctx: &std::sync::Arc<ToolContext>,
    method: reqwest::Method,
    path: &str,
    body: Option<serde_json::Value>,
) -> Result<serde_json::Value> {
    let url = crate::config::get_env_value("HASS_URL")
        .ok_or_else(|| crate::error::AgentError::tool("HASS_URL not set"))?;
    let token = crate::config::get_env_value("HASS_TOKEN")
        .ok_or_else(|| crate::error::AgentError::tool("HASS_TOKEN not set"))?;
    let client = reqwest::Client::new();
    let mut request = client
        .request(
            method,
            format!("{}/api/{}", url.trim_end_matches('/'), path),
        )
        .header("Authorization", format!("Bearer {}", token));
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request
        .send()
        .await
        .map_err(|e| crate::error::AgentError::tool(format!("Home Assistant: {}", e)))?;
    let status = response.status();
    let value: serde_json::Value = response
        .json()
        .await
        .unwrap_or_else(|_| json!({"raw": "no json body"}));
    let _ = ctx;
    if !status.is_success() {
        return Err(crate::error::AgentError::tool(format!(
            "Home Assistant returned {}: {}",
            status, value
        )));
    }
    Ok(value)
}

fn register_homeassistant(registry: &mut ToolRegistry) {
    registry.register(
        tool("ha_list_entities")
            .description("List Home Assistant entity ids, optionally filtered by domain (light, switch, sensor...).")
            .parameters(json!({
                "type": "object",
                "properties": {
                    "domain": {"type": "string", "description": "Optional domain filter, e.g. 'light'"}
                },
                "required": []
            }))
            .handler(|args, ctx| async move {
                let domain = args.get("domain").and_then(|v| v.as_str()).map(String::from);
                match hass_request(&ctx, reqwest::Method::GET, "states", None).await {
                    Ok(states) => {
                        let entities: Vec<serde_json::Value> = states
                            .as_array()
                            .map(|arr| {
                                arr.iter()
                                    .filter(|state| {
                                        domain.as_ref().map(|d| {
                                            state.pointer("/entity_id").and_then(|v| v.as_str())
                                                .map(|id| id.starts_with(&format!("{}.", d)))
                                                .unwrap_or(false)
                                        }).unwrap_or(true)
                                    })
                                    .map(|state| json!({
                                        "entity_id": state.pointer("/entity_id").and_then(|v| v.as_str()),
                                        "state": state.pointer("/state").and_then(|v| v.as_str()),
                                        "friendly_name": state.pointer("/attributes/friendly_name").and_then(|v| v.as_str()),
                                    }))
                                    .collect()
                            })
                            .unwrap_or_default();
                        Ok(json!({"success": true, "entities": entities, "count": entities.len()}))
                    }
                    Err(e) => Ok(json!({"success": false, "error": e.to_string()})),
                }
            })
            .toolset("homeassistant")
            .emoji("🏠")
            .check_fn(hass_configured)
            .build()
            .expect("ha_list_entities builds"),
    );

    registry.register(
        tool("ha_get_state")
            .description("Get the current state and attributes of one Home Assistant entity.")
            .parameters(json!({
                "type": "object",
                "properties": {
                    "entity_id": {"type": "string", "description": "Entity id, e.g. 'light.kitchen'"}
                },
                "required": ["entity_id"]
            }))
            .handler(|args, ctx| async move {
                let Some(entity_id) = args.get("entity_id").and_then(|v| v.as_str()) else {
                    return Ok(json!({"success": false, "error": "ha_get_state: 'entity_id' is required"}));
                };
                match hass_request(&ctx, reqwest::Method::GET, &format!("states/{}", entity_id), None).await {
                    Ok(state) => Ok(json!({"success": true, "entity": state})),
                    Err(e) => Ok(json!({"success": false, "error": e.to_string()})),
                }
            })
            .toolset("homeassistant")
            .emoji("🏠")
            .check_fn(hass_configured)
            .build()
            .expect("ha_get_state builds"),
    );

    registry.register(
        tool("ha_list_services")
            .description("List available Home Assistant services, optionally filtered by domain.")
            .parameters(json!({
                "type": "object",
                "properties": {
                    "domain": {"type": "string", "description": "Optional domain filter, e.g. 'light'"}
                },
                "required": []
            }))
            .handler(|args, ctx| async move {
                let domain = args.get("domain").and_then(|v| v.as_str()).map(String::from);
                match hass_request(&ctx, reqwest::Method::GET, "services", None).await {
                    Ok(services) => {
                        let filtered: Vec<serde_json::Value> = services
                            .as_array()
                            .map(|arr| {
                                arr.iter()
                                    .filter(|svc| {
                                        domain.as_ref().map(|d| {
                                            svc.get("domain").and_then(|v| v.as_str()) == Some(d.as_str())
                                        }).unwrap_or(true)
                                    })
                                    .map(|svc| json!({
                                        "domain": svc.get("domain").and_then(|v| v.as_str()),
                                        "services": svc.get("services").and_then(|v| v.as_object()).map(|o| o.keys().collect::<Vec<_>>()),
                                    }))
                                    .collect()
                            })
                            .unwrap_or_default();
                        Ok(json!({"success": true, "services": filtered}))
                    }
                    Err(e) => Ok(json!({"success": false, "error": e.to_string()})),
                }
            })
            .toolset("homeassistant")
            .emoji("🏠")
            .check_fn(hass_configured)
            .build()
            .expect("ha_list_services builds"),
    );

    registry.register(
        tool("ha_call_service")
            .description("Call a Home Assistant service (e.g. light/turn_on) with entity and data payload.")
            .parameters(json!({
                "type": "object",
                "properties": {
                    "domain": {"type": "string", "description": "Service domain, e.g. 'light'"},
                    "service": {"type": "string", "description": "Service name, e.g. 'turn_on'"},
                    "entity_id": {"type": "string", "description": "Target entity id"},
                    "data": {"type": "object", "description": "Extra service data (brightness, color, ...)"}
                },
                "required": ["domain", "service", "entity_id"]
            }))
            .handler(|args, ctx| async move {
                let domain = args.get("domain").and_then(|v| v.as_str()).unwrap_or("");
                let service = args.get("service").and_then(|v| v.as_str()).unwrap_or("");
                let entity_id = args.get("entity_id").and_then(|v| v.as_str()).unwrap_or("");
                if domain.is_empty() || service.is_empty() || entity_id.is_empty() {
                    return Ok(json!({"success": false, "error": "ha_call_service: domain, service and entity_id are required"}));
                }
                let mut data = args.get("data").cloned().and_then(|v| if v.is_object() { Some(v) } else { None }).unwrap_or_else(|| json!({}));
                data["entity_id"] = json!(entity_id);
                match hass_request(&ctx, reqwest::Method::POST, &format!("services/{}/{}", domain, service), Some(data)).await {
                    Ok(result) => Ok(json!({"success": true, "result": result})),
                    Err(e) => Ok(json!({"success": false, "error": e.to_string()})),
                }
            })
            .toolset("homeassistant")
            .dangerous(true)
            .emoji("🏠")
            .check_fn(hass_configured)
            .build()
            .expect("ha_call_service builds"),
    );
}

// ---------------------------------------------------------------------------
// Kanban — local multi-agent coordination board. The agent tools share the
// same KanbanStore engine (and the same kanban.db) as the `ulnclaw kanban`
// CLI, mirroring hermes where kanban_* tools and `hermes kanban` both ride
// hermes_cli/kanban_db.py. Worker/orchestrator gating follows hermes:
// a process spawned with ULNCLAW_KANBAN_TASK (fallback HERMES_KANBAN_TASK)
// is a worker — create/unblock/link are orchestrator-only.
// ---------------------------------------------------------------------------

fn kanban_engine(ctx: &ToolContext) -> Result<crate::kanban::KanbanStore> {
    crate::kanban::KanbanStore::open(ctx.home.join("kanban.db"))
        .map_err(|e| crate::error::AgentError::tool(format!("open kanban db: {e}")))
}

/// The task this process is working on, if it was spawned as a kanban
/// worker (hermes `HERMES_KANBAN_TASK`).
fn kanban_worker_task() -> Option<String> {
    crate::kanban::worker_task_env()
}

fn kanban_is_worker() -> bool {
    kanban_worker_task().is_some()
}

/// Resolve a task id argument, accepting unique prefixes like the CLI;
/// falls back to the worker's own task when `id` is empty.
fn kanban_resolve_id(store: &crate::kanban::KanbanStore, id: &str) -> Result<Option<String>> {
    let id = id.trim();
    let id = if id.is_empty() {
        match kanban_worker_task() {
            Some(task) => task,
            None => return Ok(None),
        }
    } else {
        id.to_string()
    };
    if store.get_task(&id)?.is_some() {
        return Ok(Some(id));
    }
    store.resolve_task_id(&id)
}

fn kanban_task_json(
    store: &crate::kanban::KanbanStore,
    task: &crate::kanban::Task,
) -> serde_json::Value {
    let parents = store.parents_of(&task.id).unwrap_or_default();
    let children = store.children_of(&task.id).unwrap_or_default();
    json!({
        "id": task.id,
        "title": task.title,
        "body": task.body,
        "status": task.status,
        "assignee": task.assignee,
        "priority": task.priority,
        "created_by": task.created_by,
        "created_at": task.created_at,
        "started_at": task.started_at,
        "completed_at": task.completed_at,
        "result": task.result,
        "parents": parents,
        "children": children,
    })
}

fn kanban_error(e: impl std::fmt::Display) -> serde_json::Value {
    json!({"success": false, "error": e.to_string()})
}

fn register_kanban(registry: &mut ToolRegistry) {
    registry.register(
        tool("kanban_create")
            .description("Create a kanban task on the local coordination board. Orchestrator-only: workers (ULNCLAW_KANBAN_TASK set) cannot create tasks.")
            .parameters(json!({
                "type": "object",
                "properties": {
                    "title": {"type": "string", "description": "Task title"},
                    "body": {"type": "string", "description": "Task description"},
                    "assignee": {"type": "string", "description": "Optional assignee (agent/profile name)"},
                    "parents": {"type": "array", "items": {"type": "string"}, "description": "Optional parent task ids (this task becomes their child)"}
                },
                "required": ["title"]
            }))
            .handler(|args, ctx| async move {
                if kanban_is_worker() {
                    return Ok(kanban_error("kanban_create is orchestrator-only; workers finish with kanban_complete/kanban_block"));
                }
                let title = args.get("title").and_then(|v| v.as_str()).unwrap_or("").trim();
                if title.is_empty() {
                    return Ok(kanban_error("kanban_create: 'title' is required"));
                }
                let store = match kanban_engine(&ctx) {
                    Ok(s) => s,
                    Err(e) => return Ok(kanban_error(e)),
                };
                let body = args.get("body").and_then(|v| v.as_str()).unwrap_or("");
                let assignee = args
                    .get("assignee")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from);
                let task = match store.create_task(&crate::kanban::NewTask {
                    title: title.to_string(),
                    body: body.to_string(),
                    assignee,
                    priority: 0,
                    tenant: None,
                    model: None,
                    created_by: "agent".to_string(),
                    // Wake routing: remember the creator session so the
                    // notifier can resume it on terminal events (hermes
                    // session_id).
                    session_id: Some(ctx.session_id.clone())
                        .filter(|id| !id.trim().is_empty()),
                    ..Default::default()
                }) {
                    Ok(t) => t,
                    Err(e) => return Ok(kanban_error(e)),
                };
                if let Some(parents) = args.get("parents").and_then(|v| v.as_array()) {
                    for parent in parents.iter().filter_map(|v| v.as_str()) {
                        if let Some(parent_id) = store.resolve_task_id(parent).ok().flatten() {
                            store.link_tasks(&parent_id, &task.id).ok();
                        }
                    }
                }
                Ok(json!({"success": true, "task_id": task.id, "status": task.status}))
            })
            .toolset("kanban")
            .emoji("📋")
            .build()
            .expect("kanban_create builds"),
    );

    registry.register(
        tool("kanban_list")
            .description("List kanban tasks on the current board, optionally filtered by status (todo/ready/running/scheduled/blocked/done/archived).")
            .parameters(json!({
                "type": "object",
                "properties": {
                    "status": {"type": "string", "description": "Optional status filter (todo/ready/running/scheduled/blocked/done/archived)"},
                    "limit": {"type": "integer", "description": "Max tasks to return (default 50)"}
                },
                "required": []
            }))
            .handler(|args, ctx| async move {
                let store = match kanban_engine(&ctx) {
                    Ok(s) => s,
                    Err(e) => return Ok(kanban_error(e)),
                };
                let status = args.get("status").and_then(|v| v.as_str()).filter(|s| !s.is_empty());
                let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;
                match store.list_tasks(None, status, None, None, limit.max(1)) {
                    Ok(tasks) => {
                        let rows: Vec<serde_json::Value> = tasks
                            .iter()
                            .map(|t| json!({
                                "id": t.id,
                                "title": t.title,
                                "status": t.status,
                                "assignee": t.assignee,
                                "priority": t.priority,
                            }))
                            .collect();
                        Ok(json!({"success": true, "count": rows.len(), "tasks": rows}))
                    }
                    Err(e) => Ok(kanban_error(e)),
                }
            })
            .toolset("kanban")
            .emoji("📋")
            .build()
            .expect("kanban_list builds"),
    );

    registry.register(
        tool("kanban_show")
            .description("Show one kanban task with comments, attachments, and parent/child links. Workers may omit task_id to see their own task.")
            .parameters(json!({
                "type": "object",
                "properties": {
                    "task_id": {"type": "string", "description": "Task id (or unique prefix); defaults to the worker's own task"}
                },
                "required": []
            }))
            .handler(|args, ctx| async move {
                let store = match kanban_engine(&ctx) {
                    Ok(s) => s,
                    Err(e) => return Ok(kanban_error(e)),
                };
                let id = args.get("task_id").and_then(|v| v.as_str()).unwrap_or("");
                let id = match kanban_resolve_id(&store, id) {
                    Ok(Some(id)) => id,
                    Ok(None) => return Ok(kanban_error("task_id is required (no worker task in env)")),
                    Err(e) => return Ok(kanban_error(e)),
                };
                let task = match store.get_task(&id) {
                    Ok(Some(task)) => task,
                    Ok(None) => return Ok(kanban_error(format!("task '{id}' not found"))),
                    Err(e) => return Ok(kanban_error(e)),
                };
                let comments: Vec<serde_json::Value> = store
                    .comments(&id)
                    .unwrap_or_default()
                    .iter()
                    .map(|c| json!({"author": c.author, "body": c.body, "created_at": c.created_at}))
                    .collect();
                let attachments: Vec<serde_json::Value> = store
                    .attachments(&id)
                    .unwrap_or_default()
                    .iter()
                    .map(|(kind, value)| json!({"kind": kind, "value": value}))
                    .collect();
                let worker_context = store.build_worker_context(&id).unwrap_or_default();
                Ok(json!({
                    "success": true,
                    "task": kanban_task_json(&store, &task),
                    "comments": comments,
                    "attachments": attachments,
                    "worker_context": worker_context,
                }))
            })
            .toolset("kanban")
            .emoji("📋")
            .build()
            .expect("kanban_show builds"),
    );

    registry.register(
        tool("kanban_comment")
            .description("Add a comment to a kanban task. Workers may omit task_id to comment on their own task.")
            .parameters(json!({
                "type": "object",
                "properties": {
                    "task_id": {"type": "string", "description": "Task id (or unique prefix); defaults to the worker's own task"},
                    "body": {"type": "string", "description": "Comment text"}
                },
                "required": ["body"]
            }))
            .handler(|args, ctx| async move {
                let body = args.get("body").and_then(|v| v.as_str()).unwrap_or("").trim();
                if body.is_empty() {
                    return Ok(kanban_error("kanban_comment: 'body' is required"));
                }
                let store = match kanban_engine(&ctx) {
                    Ok(s) => s,
                    Err(e) => return Ok(kanban_error(e)),
                };
                let id = args.get("task_id").and_then(|v| v.as_str()).unwrap_or("");
                let id = match kanban_resolve_id(&store, id) {
                    Ok(Some(id)) => id,
                    Ok(None) => return Ok(kanban_error("task_id is required (no worker task in env)")),
                    Err(e) => return Ok(kanban_error(e)),
                };
                match store.add_comment(&id, "agent", body) {
                    Ok(()) => Ok(json!({"success": true, "task_id": id})),
                    Err(e) => Ok(kanban_error(e)),
                }
            })
            .toolset("kanban")
            .emoji("📋")
            .build()
            .expect("kanban_comment builds"),
    );

    registry.register(
        tool("kanban_heartbeat")
            .description("Signal progress on a kanban task: refreshes the claim TTL and optionally records a progress comment. Workers may omit task_id.")
            .parameters(json!({
                "type": "object",
                "properties": {
                    "task_id": {"type": "string", "description": "Task id (or unique prefix); defaults to the worker's own task"},
                    "progress": {"type": "string", "description": "Short progress note"}
                },
                "required": []
            }))
            .handler(|args, ctx| async move {
                let store = match kanban_engine(&ctx) {
                    Ok(s) => s,
                    Err(e) => return Ok(kanban_error(e)),
                };
                let id = args.get("task_id").and_then(|v| v.as_str()).unwrap_or("");
                let id = match kanban_resolve_id(&store, id) {
                    Ok(Some(id)) => id,
                    Ok(None) => return Ok(kanban_error("task_id is required (no worker task in env)")),
                    Err(e) => return Ok(kanban_error(e)),
                };
                let claimer = crate::kanban::KanbanStore::claimer_id();
                let ttl = crate::kanban::DEFAULT_CLAIM_TTL_SECS;
                // Hermes swarm semantics: heartbeats extend a live claim.
                // Claim on demand so a fresh worker can heartbeat a task it
                // just picked up (todo/ready/scheduled → running).
                let outcome = match store.get_task(&id) {
                    Ok(Some(task)) if task.status == "todo" => {
                        // todo → ready → running (claim) → heartbeat.
                        store
                            .ready_task(&id)
                            .and_then(|t| store.claim_task(&t.id, &claimer, ttl))
                            .and_then(|t| store.heartbeat_task(&t.id, &claimer, ttl))
                    }
                    Ok(Some(task)) if task.status == "ready" => {
                        store
                            .claim_task(&id, &claimer, ttl)
                            .and_then(|t| store.heartbeat_task(&t.id, &claimer, ttl))
                    }
                    Ok(Some(_)) => store.heartbeat_task(&id, &claimer, ttl),
                    Ok(None) => return Ok(kanban_error(format!("task '{id}' not found"))),
                    Err(e) => return Ok(kanban_error(e)),
                };
                match outcome {
                    Ok(task) => {
                        if let Some(progress) = args.get("progress").and_then(|v| v.as_str()) {
                            if !progress.trim().is_empty() {
                                store
                                    .add_comment(&id, "agent", &format!("[heartbeat] {}", progress.trim()))
                                    .ok();
                            }
                        }
                        Ok(json!({"success": true, "task_id": task.id, "status": task.status}))
                    }
                    Err(e) => Ok(kanban_error(e)),
                }
            })
            .toolset("kanban")
            .emoji("📋")
            .build()
            .expect("kanban_heartbeat builds"),
    );

    let terminal = |name: &'static str, done: bool, desc: &'static str| {
        tool(name)
            .description(desc)
            .parameters(json!({
                "type": "object",
                "properties": {
                    "task_id": {"type": "string", "description": "Task id (or unique prefix); defaults to the worker's own task"},
                    "result": {"type": "string", "description": if done { "Summary of what was accomplished" } else { "Why the task is blocked (required)" }},
                    "comment": {"type": "string", "description": "Optional extra comment"},
                    "kind": {"type": "string", "enum": ["dependency", "needs_input", "capability", "transient"], "description": "Block only: typed reason. dependency waits in todo until parents finish; needs_input/capability wait on a human; transient may clear on its own"},
                    "summary": {"type": "string", "description": "Complete only: structured handoff summary for downstream tasks (falls back to result)"},
                    "metadata": {"type": "object", "description": "Complete only: structured facts for the handoff, e.g. {\"changed_files\": [...], \"tests_run\": 12}"},
                    "artifacts": {"type": "array", "items": {"type": "string"}, "description": "Complete only: deliverable file paths. Files inside the scratch workspace are staged into the board's attachments dir so cleanup cannot erase them"},
                    "created_cards": {"type": "array", "items": {"type": "string"}, "description": "Complete only: ids of tasks you created during this run. Each id is verified before completion; phantom ids block the completion"}
                },
                "required": []
            }))
            .handler(move |args, ctx| {
                async move {
                    let store = match kanban_engine(&ctx) {
                        Ok(s) => s,
                        Err(e) => return Ok(kanban_error(e)),
                    };
                    let id = args.get("task_id").and_then(|v| v.as_str()).unwrap_or("");
                    let id = match kanban_resolve_id(&store, id) {
                        Ok(Some(id)) => id,
                        Ok(None) => return Ok(kanban_error("task_id is required (no worker task in env)")),
                        Err(e) => return Ok(kanban_error(e)),
                    };
                    let result = args.get("result").and_then(|v| v.as_str()).map(str::trim);
                    let outcome = if done {
                        // Structured handoff (hermes kanban_complete
                        // summary=/metadata=): summary falls back to
                        // result inside the engine.
                        let summary = args
                            .get("summary")
                            .and_then(|v| v.as_str())
                            .map(str::trim)
                            .filter(|s| !s.is_empty());
                        let metadata = args
                            .get("metadata")
                            .filter(|v| v.is_object());
                        let artifacts: Vec<String> = args
                            .get("artifacts")
                            .and_then(|v| v.as_array())
                            .map(|items| {
                                items
                                    .iter()
                                    .filter_map(|item| {
                                        item.as_str().map(str::trim).filter(|s| !s.is_empty())
                                    })
                                    .map(str::to_string)
                                    .collect()
                            })
                            .unwrap_or_default();
                        let created_cards: Vec<String> = args
                            .get("created_cards")
                            .and_then(|v| v.as_array())
                            .map(|items| {
                                items
                                    .iter()
                                    .filter_map(|item| {
                                        item.as_str().map(str::trim).filter(|s| !s.is_empty())
                                    })
                                    .map(str::to_string)
                                    .collect()
                            })
                            .unwrap_or_default();
                        // Goal-mode judge gate (hermes #38367): a goal-mode
                        // card's completion must carry evidence the
                        // auxiliary judge accepts. Fail-open when no judge
                        // can run.
                        if let Ok(Some(task)) = store.get_task(&id) {
                            if task.goal_mode {
                                let mut goal_parts = vec![task.title.clone()];
                                if !task.body.trim().is_empty() {
                                    goal_parts.push(task.body.clone());
                                }
                                let goal_text = goal_parts.join("\n\n");
                                let text = summary
                                    .or(result)
                                    .unwrap_or("");
                                if let Err(reason) = crate::goals::goal_completion_gate(
                                    &ctx.config,
                                    ctx.provider.clone(),
                                    &goal_text,
                                    text,
                                )
                                .await
                                {
                                    return Ok(kanban_error(format!(
                                        "kanban_complete rejected by goal judge: {reason}.                                          Provide evidence matching the task's acceptance criteria."
                                    )));
                                }
                            }
                        }
                        store.complete_task_with_artifacts(
                            &ctx.home,
                            &id,
                            result.filter(|s| !s.is_empty()),
                            summary,
                            metadata,
                            &artifacts,
                            &created_cards,
                            crate::kanban::worker_run_id_for(&id),
                        )
                    } else {
                        match result.filter(|s| !s.is_empty()) {
                            Some(reason) => {
                                let kind = args
                                    .get("kind")
                                    .and_then(|v| v.as_str())
                                    .map(str::trim)
                                    .filter(|k| !k.is_empty());
                                store.block_task_guarded(
                                    &id,
                                    reason,
                                    kind,
                                    crate::kanban::worker_run_id_for(&id),
                                )
                            }
                            None => return Ok(kanban_error("kanban_block: 'result' (the blocking reason) is required")),
                        }
                    };
                    match outcome {
                        Ok(task) => {
                            if let Some(comment) = args.get("comment").and_then(|v| v.as_str()) {
                                if !comment.trim().is_empty() {
                                    store.add_comment(&id, "agent", comment.trim()).ok();
                                }
                            }
                            Ok(json!({"success": true, "task_id": task.id, "status": task.status}))
                        }
                        Err(e) => Ok(kanban_error(e)),
                    }
                }
            })
            .toolset("kanban")
            .emoji("📋")
            .build()
            .unwrap_or_else(|_| panic!("{name} builds"))
    };
    registry.register(terminal(
        "kanban_complete",
        true,
        "Mark a kanban task done with a result summary. Terminal tool: workers must end with kanban_complete or kanban_block.",
    ));
    registry.register(terminal(
        "kanban_block",
        false,
        "Mark a kanban task blocked with a reason. Terminal tool: workers must end with kanban_complete or kanban_block.",
    ));

    registry.register(
        tool("kanban_review")
            .description("Move your running kanban task to the review column after opening a PR. The dispatcher spawns a review agent that verifies and merges it (or rejects it back to you). Call this instead of kanban_complete when the work needs review before merge.")
            .parameters(json!({
                "type": "object",
                "properties": {
                    "task_id": {"type": "string", "description": "Task id (or unique prefix); defaults to the worker's own task"},
                    "reason": {"type": "string", "description": "Short note, e.g. the PR URL"}
                },
                "required": []
            }))
            .handler(|args, ctx| async move {
                let store = match kanban_engine(&ctx) {
                    Ok(s) => s,
                    Err(e) => return Ok(kanban_error(e)),
                };
                let id = args.get("task_id").and_then(|v| v.as_str()).unwrap_or("");
                let id = match kanban_resolve_id(&store, id) {
                    Ok(Some(id)) => id,
                    Ok(None) => return Ok(kanban_error("task_id is required (no worker task in env)")),
                    Err(e) => return Ok(kanban_error(e)),
                };
                let reason = args
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("ready for review");
                match store.request_review(&id, reason) {
                    Ok(task) => Ok(json!({"success": true, "task_id": task.id, "status": task.status})),
                    Err(e) => Ok(kanban_error(e)),
                }
            })
            .toolset("kanban")
            .emoji("📋")
            .build()
            .expect("kanban_review builds"),
    );

    registry.register(
        tool("kanban_unblock")
            .description("Unblock a kanban task (moves it back to todo). Orchestrator-only.")
            .parameters(json!({
                "type": "object",
                "properties": {
                    "task_id": {"type": "string", "description": "Task id (or unique prefix)"}
                },
                "required": ["task_id"]
            }))
            .handler(|args, ctx| async move {
                if kanban_is_worker() {
                    return Ok(kanban_error("kanban_unblock is orchestrator-only"));
                }
                let store = match kanban_engine(&ctx) {
                    Ok(s) => s,
                    Err(e) => return Ok(kanban_error(e)),
                };
                let id = args.get("task_id").and_then(|v| v.as_str()).unwrap_or("");
                let id = match kanban_resolve_id(&store, id) {
                    Ok(Some(id)) => id,
                    Ok(None) => return Ok(kanban_error("task_id is required")),
                    Err(e) => return Ok(kanban_error(e)),
                };
                match store.unblock_task(&id) {
                    Ok(task) => {
                        Ok(json!({"success": true, "task_id": task.id, "status": task.status}))
                    }
                    Err(e) => Ok(kanban_error(e)),
                }
            })
            .toolset("kanban")
            .emoji("📋")
            .build()
            .expect("kanban_unblock builds"),
    );

    registry.register(
        tool("kanban_link")
            .description("Link a kanban task to a parent task (subtask relationship). Orchestrator-only.")
            .parameters(json!({
                "type": "object",
                "properties": {
                    "task_id": {"type": "string", "description": "Child task id (or unique prefix)"},
                    "parent_id": {"type": "string", "description": "Parent task id (or unique prefix)"}
                },
                "required": ["task_id", "parent_id"]
            }))
            .handler(|args, ctx| async move {
                if kanban_is_worker() {
                    return Ok(kanban_error("kanban_link is orchestrator-only"));
                }
                let store = match kanban_engine(&ctx) {
                    Ok(s) => s,
                    Err(e) => return Ok(kanban_error(e)),
                };
                let child = args.get("task_id").and_then(|v| v.as_str()).unwrap_or("");
                let parent = args.get("parent_id").and_then(|v| v.as_str()).unwrap_or("");
                let child = match kanban_resolve_id(&store, child) {
                    Ok(Some(id)) => id,
                    Ok(None) => return Ok(kanban_error("task_id and parent_id are required")),
                    Err(e) => return Ok(kanban_error(e)),
                };
                let parent = match kanban_resolve_id(&store, parent) {
                    Ok(Some(id)) => id,
                    Ok(None) => return Ok(kanban_error("task_id and parent_id are required")),
                    Err(e) => return Ok(kanban_error(e)),
                };
                match store.link_tasks(&parent, &child) {
                    Ok(()) => Ok(json!({"success": true, "parent_id": parent, "child_id": child})),
                    Err(e) => Ok(kanban_error(e)),
                }
            })
            .toolset("kanban")
            .emoji("📋")
            .build()
            .expect("kanban_link builds"),
    );

    let attach = |name: &'static str,
                  kind: &'static str,
                  value_field: &'static str,
                  desc: &'static str| {
        tool(name)
            .description(desc)
            .parameters(json!({
                "type": "object",
                "properties": {
                    "task_id": {"type": "string", "description": "Task id (or unique prefix); defaults to the worker's own task"},
                    value_field: {"type": "string", "description": "Attachment value"}
                },
                "required": [value_field]
            }))
            .handler(move |args, ctx| {
                let kind = kind;
                let value_field = value_field;
                async move {
                    let value = args.get(&value_field).and_then(|v| v.as_str()).unwrap_or("").trim();
                    if value.is_empty() {
                        return Ok(kanban_error(format!("kanban: '{value_field}' is required")));
                    }
                    let store = match kanban_engine(&ctx) {
                        Ok(s) => s,
                        Err(e) => return Ok(kanban_error(e)),
                    };
                    let id = args.get("task_id").and_then(|v| v.as_str()).unwrap_or("");
                    let id = match kanban_resolve_id(&store, id) {
                        Ok(Some(id)) => id,
                        Ok(None) => return Ok(kanban_error("task_id is required (no worker task in env)")),
                        Err(e) => return Ok(kanban_error(e)),
                    };
                    match store.attach(&id, kind, value) {
                        Ok(()) => Ok(json!({"success": true, "task_id": id})),
                        Err(e) => Ok(kanban_error(e)),
                    }
                }
            })
            .toolset("kanban")
            .emoji("📋")
            .build()
            .unwrap_or_else(|_| panic!("{name} builds"))
    };
    registry.register(attach(
        "kanban_attach",
        "file",
        "path",
        "Attach a local file path to a kanban task.",
    ));
    registry.register(attach(
        "kanban_attach_url",
        "url",
        "url",
        "Attach a URL to a kanban task.",
    ));

    registry.register(
        tool("kanban_attachments")
            .description("List attachments of a kanban task.")
            .parameters(json!({
                "type": "object",
                "properties": {
                    "task_id": {"type": "string", "description": "Task id (or unique prefix); defaults to the worker's own task"}
                },
                "required": []
            }))
            .handler(|args, ctx| async move {
                let store = match kanban_engine(&ctx) {
                    Ok(s) => s,
                    Err(e) => return Ok(kanban_error(e)),
                };
                let id = args.get("task_id").and_then(|v| v.as_str()).unwrap_or("");
                let id = match kanban_resolve_id(&store, id) {
                    Ok(Some(id)) => id,
                    Ok(None) => return Ok(kanban_error("task_id is required (no worker task in env)")),
                    Err(e) => return Ok(kanban_error(e)),
                };
                let attachments: Vec<serde_json::Value> = store
                    .attachments(&id)
                    .unwrap_or_default()
                    .iter()
                    .map(|(kind, value)| json!({"kind": kind, "value": value}))
                    .collect();
                Ok(json!({"success": true, "attachments": attachments}))
            })
            .toolset("kanban")
            .emoji("📋")
            .build()
            .expect("kanban_attachments builds"),
    );
}

// ---------------------------------------------------------------------------
// Browser tools — registered with faithful schemas, gated on CDP endpoint
// ---------------------------------------------------------------------------

fn browser_configured() -> ToolAvailability {
    if crate::browser::camofox::is_camofox_mode() {
        return ToolAvailability::available();
    }
    let raw = crate::config::get_env_value("ULNCLAW_BROWSER_CDP");
    match raw.as_deref() {
        // Explicit endpoint always enabled.
        Some(value) if !crate::browser::is_auto_mode(value) => ToolAvailability::available(),
        // Auto/managed mode (explicit or default): need a local binary.
        _ => {
            if crate::browser::find_browser_binary().is_some() {
                ToolAvailability::available()
            } else {
                ToolAvailability::unavailable(
                    "no browser backend available (install Chrome/Chromium for auto-launch, or set ULNCLAW_BROWSER_CDP to a DevTools endpoint)",
                )
            }
        }
    }
}

/// hermes `_blocked_private_page_action` / snapshot current-page guard:
/// refuse to read or operate on a page sitting on a private address.
async fn private_page_block(
    session: &std::sync::Arc<crate::browser::BrowserSession>,
    guard: bool,
) -> Option<serde_json::Value> {
    if !guard {
        return None;
    }
    let info = session.page_info().await.unwrap_or(json!({}));
    let url = info.get("url").and_then(|v| v.as_str()).unwrap_or("");
    if !url.is_empty() && crate::browser::guard::is_private_url(url) {
        return Some(json!({
            "success": false,
            "error": format!(
                "Blocked: page URL targets a private or internal address ({url}).                  Refusing to read or interact with this page in this browser mode."
            )
        }));
    }
    None
}

fn register_browser(registry: &mut ToolRegistry) {
    let specs: [(&str, &str, serde_json::Value); 12] = [
        (
            "browser_navigate",
            "Navigate the browser to a URL.",
            json!({
                "type": "object",
                "properties": {"url": {"type": "string", "description": "URL to open"}},
                "required": ["url"]
            }),
        ),
        (
            "browser_snapshot",
            "Capture an accessibility snapshot of the current page (element refs for interaction).",
            json!({
                "type": "object", "properties": {}, "required": []
            }),
        ),
        (
            "browser_click",
            "Click an element from the last snapshot.",
            json!({
                "type": "object",
                "properties": {"element": {"type": "string", "description": "Element ref or selector"}},
                "required": ["element"]
            }),
        ),
        (
            "browser_type",
            "Type text into an input element.",
            json!({
                "type": "object",
                "properties": {
                    "element": {"type": "string", "description": "Element ref or selector"},
                    "text": {"type": "string", "description": "Text to type"}
                },
                "required": ["element", "text"]
            }),
        ),
        (
            "browser_scroll",
            "Scroll the page (up/down, pixels).",
            json!({
                "type": "object",
                "properties": {
                    "direction": {"type": "string", "enum": ["up", "down"], "default": "down"},
                    "pixels": {"type": "integer", "default": 800}
                },
                "required": []
            }),
        ),
        (
            "browser_back",
            "Navigate back in history.",
            json!({"type": "object", "properties": {}, "required": []}),
        ),
        (
            "browser_press",
            "Press a keyboard key (Enter, Tab, Escape...).",
            json!({
                "type": "object",
                "properties": {"key": {"type": "string", "description": "Key name"}},
                "required": ["key"]
            }),
        ),
        (
            "browser_get_images",
            "List images on the current page with URLs.",
            json!({"type": "object", "properties": {}, "required": []}),
        ),
        (
            "browser_vision",
            "Screenshot the page and analyze it with the vision model.",
            json!({
                "type": "object",
                "properties": {"prompt": {"type": "string", "description": "What to look for in the screenshot"}},
                "required": []
            }),
        ),
        (
            "browser_console",
            "Evaluate JavaScript in the page console and return the result.",
            json!({
                "type": "object",
                "properties": {"expression": {"type": "string", "description": "JS expression"}},
                "required": ["expression"]
            }),
        ),
        (
            "browser_cdp",
            "Send a raw Chrome DevTools Protocol command.",
            json!({
                "type": "object",
                "properties": {
                    "method": {"type": "string", "description": "CDP method, e.g. Page.captureScreenshot"},
                    "params": {"type": "object", "description": "CDP params"}
                },
                "required": ["method"]
            }),
        ),
        (
            "browser_dialog",
            "Handle a JavaScript dialog (accept/dismiss).",
            json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["accept", "dismiss"], "default": "accept"},
                    "prompt_text": {"type": "string", "description": "Text for prompt dialogs"}
                },
                "required": []
            }),
        ),
    ];

    for (name, description, parameters) in specs {
        registry.register(
            tool(name)
                .description(description)
                .parameters(parameters)
                .handler(move |args, ctx| {
                    let tool_name = name.to_string();
                    async move {
                        use crate::browser::with_session;
                        // Hermes browser SSRF guard: active for non-local endpoints
                        // (or containerized terminals); the metadata floor and the
                        // sensitive-query check fire unconditionally.
                        let endpoint_raw = crate::browser::configured_endpoint_raw();
                        let terminal_local = matches!(
                            ctx.config.terminal.backend.as_deref(),
                            None | Some("local") | Some("")
                        );
                        let guard = crate::browser::guard::guard_active(
                            endpoint_raw.as_deref(),
                            terminal_local,
                        );
                        match tool_name.as_str() {
                            "browser_navigate" => {
                                let url = args.get("url").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
                                if url.is_empty() {
                                    return Ok(json!({"success": false, "error": "url is required"}));
                                }
                                if let Some(error) = crate::browser::guard::blocked_navigate(&url, guard) {
                                    return Ok(json!({"success": false, "error": error}));
                                }
                                if crate::browser::camofox::is_camofox_mode() {
                                    return Ok(crate::browser::camofox::navigate(&ctx.session_id, &url, guard).await);
                                }
                                with_session(move |session| async move {
                                    session.navigate(&url).await?;
                                    let info = session.page_info().await.unwrap_or(json!({}));
                                    // Post-redirect SSRF check (hermes): refuse a redirect
                                    // that landed on the metadata floor (always) or a
                                    // private address (when guarded), and blank the page
                                    // so later snapshots cannot leak the content.
                                    let final_url = info
                                        .get("url")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    if !final_url.is_empty() && final_url != url {
                                        if crate::url_safety::is_always_blocked_url_sync(&final_url) {
                                            let _ = session.navigate("about:blank").await;
                                            return Ok(json!({"success": false, "error": "Blocked: redirect landed on a cloud metadata endpoint"}));
                                        }
                                        if guard && !crate::url_safety::is_safe_url_sync(&final_url) {
                                            let _ = session.navigate("about:blank").await;
                                            return Ok(json!({"success": false, "error": "Blocked: redirect landed on a private/internal address"}));
                                        }
                                    }
                                    Ok(json!({
                                        "success": true,
                                        "url": info.get("url").cloned().unwrap_or(json!("")),
                                        "title": info.get("title").cloned().unwrap_or(json!("")),
                                        "hint": "call browser_snapshot to see interactive elements"
                                    }))
                                })
                                .await
                            }
                            "browser_snapshot" => {
                                let guard = guard;
                                if crate::browser::camofox::is_camofox_mode() {
                                    return Ok(crate::browser::camofox::snapshot(&ctx.session_id, guard).await);
                                }
                                with_session(move |session| async move {
                                    if let Some(blocked) = private_page_block(&session, guard).await {
                                        return Ok(blocked);
                                    }
                                    let (text, refs) = session.snapshot().await?;
                                    Ok(json!({
                                        "success": true,
                                        "snapshot": crate::browser::guard::redact_value(json!(text)),
                                        "elements": refs.len(),
                                        "hint": "interact via browser_click/browser_type with element refs like \"3\""
                                    }))
                                })
                                .await
                            }
                            "browser_click" => {
                                let element = args.get("element").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
                                if element.is_empty() {
                                    return Ok(json!({"success": false, "error": "element is required"}));
                                }
                                if crate::browser::camofox::is_camofox_mode() {
                                    return Ok(crate::browser::camofox::click(&ctx.session_id, &element, guard).await);
                                }
                                with_session(move |session| async move {
                                    if let Some(blocked) = private_page_block(&session, guard).await {
                                        return Ok(blocked);
                                    }
                                    let result = session.click(&element).await?;
                                    Ok(json!({"success": true, "clicked": result.get("clicked").cloned()}))
                                })
                                .await
                            }
                            "browser_type" => {
                                let element = args.get("element").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
                                let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                if element.is_empty() {
                                    return Ok(json!({"success": false, "error": "element is required"}));
                                }
                                if crate::browser::camofox::is_camofox_mode() {
                                    return Ok(crate::browser::camofox::type_text(&ctx.session_id, &element, &text, guard).await);
                                }
                                with_session(move |session| async move {
                                    if let Some(blocked) = private_page_block(&session, guard).await {
                                        return Ok(blocked);
                                    }
                                    session.type_text(&element, &text).await?;
                                    Ok(json!({"success": true, "typed_chars": text.len(), "into": element}))
                                })
                                .await
                            }
                            "browser_scroll" => {
                                let direction = args.get("direction").and_then(|v| v.as_str()).unwrap_or("down").to_string();
                                let pixels = args.get("pixels").and_then(|v| v.as_u64()).unwrap_or(800);
                                if crate::browser::camofox::is_camofox_mode() {
                                    return Ok(crate::browser::camofox::scroll(&ctx.session_id, &direction).await);
                                }
                                with_session(move |session| async move {
                                    if let Some(blocked) = private_page_block(&session, guard).await {
                                        return Ok(blocked);
                                    }
                                    session.scroll(&direction, pixels).await?;
                                    Ok(json!({"success": true, "direction": direction, "pixels": pixels}))
                                })
                                .await
                            }
                            "browser_back" => {
                                let guard = guard;
                                if crate::browser::camofox::is_camofox_mode() {
                                    return Ok(crate::browser::camofox::back(&ctx.session_id).await);
                                }
                                with_session(move |session| async move {
                                    if let Some(blocked) = private_page_block(&session, guard).await {
                                        return Ok(blocked);
                                    }
                                    session.go_back().await?;
                                    let info = session.page_info().await.unwrap_or(json!({}));
                                    Ok(json!({"success": true, "url": info.get("url").cloned()}))
                                })
                                .await
                            }
                            "browser_press" => {
                                let key = args.get("key").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
                                if key.is_empty() {
                                    return Ok(json!({"success": false, "error": "key is required"}));
                                }
                                if crate::browser::camofox::is_camofox_mode() {
                                    return Ok(crate::browser::camofox::press(&ctx.session_id, &key, guard).await);
                                }
                                with_session(move |session| async move {
                                    if let Some(blocked) = private_page_block(&session, guard).await {
                                        return Ok(blocked);
                                    }
                                    session.press(&key).await?;
                                    Ok(json!({"success": true, "pressed": key}))
                                })
                                .await
                            }
                            "browser_get_images" => {
                                let guard = guard;
                                if crate::browser::camofox::is_camofox_mode() {
                                    return Ok(crate::browser::camofox::get_images(&ctx.session_id, guard).await);
                                }
                                with_session(move |session| async move {
                                    if let Some(blocked) = private_page_block(&session, guard).await {
                                        return Ok(blocked);
                                    }
                                    let images = session.get_images().await?;
                                    Ok(json!({"success": true, "images": images}))
                                })
                                .await
                            }
                            "browser_vision" => {
                                let prompt = args
                                    .get("prompt")
                                    .and_then(|v| v.as_str())
                                    .filter(|p| !p.trim().is_empty())
                                    .unwrap_or("Describe what is visible in this screenshot.")
                                    .to_string();
                                let Some(provider) = ctx.provider.clone() else {
                                    return Ok(json!({"success": false, "error": "no vision-capable provider configured"}));
                                };
                                // Auxiliary model routing: [auxiliary.vision] override.
                                let provider = match crate::provider::auxiliary::resolve_aux_task(
                                    &ctx.config,
                                    crate::provider::auxiliary::TASK_VISION,
                                    provider.clone(),
                                ) {
                                    Ok(aux) => aux.provider,
                                    Err(e) => {
                                        tracing::warn!("auxiliary vision routing failed: {}; using main provider", e);
                                        provider
                                    }
                                };
                                if crate::browser::camofox::is_camofox_mode() {
                                    return Ok(crate::browser::camofox::vision(&ctx.session_id, &prompt, provider, guard).await);
                                }
                                with_session(move |session| async move {
                                    if let Some(blocked) = private_page_block(&session, guard).await {
                                        return Ok(blocked);
                                    }
                                    let png = session.screenshot().await?;
                                    let image_url = format!("data:image/png;base64,{}", png);
                                    match provider.analyze_image(&prompt, &image_url).await {
                                        Ok(analysis) => Ok(json!({"success": true, "analysis": analysis})),
                                        Err(e) => Ok(json!({"success": false, "error": format!("vision provider: {}", e)})),
                                    }
                                })
                                .await
                            }
                            "browser_console" => {
                                if crate::browser::camofox::is_camofox_mode() {
                                    return Ok(crate::browser::camofox::console_unavailable());
                                }
                                let expression = args.get("expression").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                if expression.is_empty() {
                                    return Ok(json!({"success": false, "error": "expression is required"}));
                                }
                                if guard {
                                    if let Some(literal) =
                                        crate::browser::guard::expression_targets_private_url(&expression)
                                    {
                                        return Ok(json!({
                                            "success": false,
                                            "error": format!(
                                                "Blocked: expression targets a private or internal address ({literal})."
                                            )
                                        }));
                                    }
                                }
                                with_session(move |session| async move {
                                    if let Some(blocked) = private_page_block(&session, guard).await {
                                        return Ok(blocked);
                                    }
                                    let value = session.evaluate(&expression, None).await?;
                                    Ok(json!({
                                        "success": true,
                                        "result": crate::browser::guard::redact_value(value)
                                    }))
                                })
                                .await
                            }
                            "browser_cdp" => {
                                if crate::browser::camofox::is_camofox_mode() {
                                    return Ok(crate::browser::camofox::unsupported("browser_cdp"));
                                }
                                let method = args.get("method").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
                                let params = args.get("params").cloned().unwrap_or(json!({}));
                                if method.is_empty() {
                                    return Ok(json!({"success": false, "error": "method is required"}));
                                }
                                with_session(move |session| async move {
                                    // hermes `_browser_cdp_private_guard`: raw CDP must not
                                    // become the sibling bypass of the guarded tools.
                                    let current_url = if guard {
                                        session
                                            .page_info()
                                            .await
                                            .ok()
                                            .and_then(|info| {
                                                info.get("url").and_then(|v| v.as_str()).map(String::from)
                                            })
                                    } else {
                                        None
                                    };
                                    if let Some(error) = crate::browser::guard::blocked_cdp(
                                        &method,
                                        &params,
                                        current_url.as_deref(),
                                        guard,
                                    ) {
                                        return Ok(json!({"success": false, "error": error}));
                                    }
                                    let result = session.client().call(&method, params).await?;
                                    Ok(json!({
                                        "success": true,
                                        "result": crate::browser::guard::redact_value(result)
                                    }))
                                })
                                .await
                            }
                            "browser_dialog" => {
                                if crate::browser::camofox::is_camofox_mode() {
                                    return Ok(crate::browser::camofox::unsupported("browser_dialog"));
                                }
                                let accept = args.get("action").and_then(|v| v.as_str()).unwrap_or("accept") != "dismiss";
                                let prompt_text = args.get("prompt_text").and_then(|v| v.as_str()).map(String::from);
                                with_session(move |session| async move {
                                    let result = session.handle_dialog(accept, prompt_text.as_deref()).await?;
                                    Ok(json!({"success": true, "dialog": result}))
                                })
                                .await
                            }
                            _ => Ok(json!({"success": false, "error": format!("unknown browser tool: {}", tool_name)})),
                        }
                    }
                })
                .toolset("browser")
                .emoji("🌐")
                .check_fn(browser_configured)
                .build()
                .unwrap_or_else(|_| panic!("{} builds", name)),
        );
    }
}

// ---------------------------------------------------------------------------
// Gated stubs: computer_use + messaging platforms
// ---------------------------------------------------------------------------

fn register_gated_stubs(registry: &mut ToolRegistry) {
    // computer_use — background desktop control via cua-driver (hermes
    // tools/computer_use: schema.py + tool.py + cua_backend.py).
    registry.register(
        tool("computer_use")
            .description(
                "Drive the desktop in the background via cua-driver — screenshots, mouse, \
                 keyboard, scroll, drag — without stealing the user's cursor or keyboard \
                 focus. Supported on macOS, Windows, and Linux. Preferred workflow: call \
                 with action='capture' (mode='som' gives numbered element overlays), then \
                 click by `element` index for reliability. Pixel coordinates are supported \
                 for models trained on them. Works on any window — hidden, minimized, or \
                 behind another app. Requires cua-driver to be installed.",
            )
            .parameters(crate::computer_use::tool_schema())
            .handler(|args, ctx| async move {
                crate::computer_use::handle_computer_use(args, ctx).await
            })
            .toolset("computer_use")
            .dangerous(true)
            .emoji("\u{1f5b1}\u{fe0f}")
            .check_fn(|| {
                if crate::computer_use::resolve_cua_driver_cmd().is_some() {
                    ToolAvailability::available()
                } else {
                    ToolAvailability::unavailable(
                        "cua-driver not installed (ulnclaw computer-use install)",
                    )
                }
            })
            .build()
            .expect("computer_use builds"),
    );

    // Discord — full REST-API backed tools (hermes tools/discord_tool.py):
    // intent-gated dynamic schema, config allowlist, enriched 403s.
    crate::discord_tool::register(registry);

    // Feishu/Lark documents + drive comments (hermes tools/feishu_doc_tool.py
    // + tools/feishu_drive_tool.py): tenant-token Open API client.
    crate::feishu_doc_tool::register(registry);

    // Spotify — seven Web API tools with PKCE OAuth from auth.json
    // (hermes plugins/spotify).
    crate::spotify_tool::register(registry);

    // Yuanbao group/sticker/DM tools (hermes tools/yuanbao_tools.py):
    // gated on the live adapter session.
    crate::yuanbao_tool::register(registry);

    // send_message — cross-channel messaging via the live platform
    // adapters + channel directory (hermes tools/send_message_tool.py
    // + gateway/channel_directory.py).
    crate::send_message_tool::register(registry);
}

#[cfg(test)]
mod kanban_tests {
    use super::*;
    use std::sync::Arc;

    fn registry() -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        register_kanban(&mut registry);
        registry
    }

    fn context() -> (std::path::PathBuf, Arc<ToolContext>) {
        let dir = std::env::temp_dir().join(format!(
            "ulnclaw-kanban-tools-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = Arc::new(
            ToolContext::new()
                .with_home(&dir)
                .with_session_id("kanban-test"),
        );
        (dir, ctx)
    }

    #[tokio::test]
    async fn kanban_tools_roundtrip_on_shared_engine() {
        let (_dir, ctx) = context();
        let registry = registry();

        // Create two tasks.
        let parent = registry
            .dispatch(
                "kanban_create",
                json!({"title": "Parent goal", "body": "top level"}),
                ctx.clone(),
            )
            .await
            .unwrap();
        assert_eq!(parent["success"], true);
        let parent_id = parent["task_id"].as_str().unwrap().to_string();

        let child = registry
            .dispatch(
                "kanban_create",
                json!({"title": "Child task", "parents": [parent_id]}),
                ctx.clone(),
            )
            .await
            .unwrap();
        assert_eq!(child["success"], true);
        let child_id = child["task_id"].as_str().unwrap().to_string();

        // Parent sees the child; prefix resolution works.
        let shown = registry
            .dispatch(
                "kanban_show",
                json!({"task_id": &parent_id[..8]}),
                ctx.clone(),
            )
            .await
            .unwrap();
        assert_eq!(shown["success"], true);
        assert_eq!(shown["task"]["children"][0], child_id);

        // Comment + heartbeat on the child.
        let comment = registry
            .dispatch(
                "kanban_comment",
                json!({"task_id": child_id, "body": "starting work"}),
                ctx.clone(),
            )
            .await
            .unwrap();
        assert_eq!(comment["success"], true);
        let heartbeat = registry
            .dispatch(
                "kanban_heartbeat",
                json!({"task_id": child_id, "progress": "halfway"}),
                ctx.clone(),
            )
            .await
            .unwrap();
        assert_eq!(heartbeat["success"], true);
        assert_eq!(heartbeat["status"], "running");

        // Block requires a reason, then unblock, then complete.
        let block_missing = registry
            .dispatch("kanban_block", json!({"task_id": child_id}), ctx.clone())
            .await
            .unwrap();
        assert_eq!(block_missing["success"], false);
        let blocked = registry
            .dispatch(
                "kanban_block",
                json!({"task_id": child_id, "result": "need api key"}),
                ctx.clone(),
            )
            .await
            .unwrap();
        assert_eq!(blocked["status"], "blocked");
        let unblocked = registry
            .dispatch("kanban_unblock", json!({"task_id": child_id}), ctx.clone())
            .await
            .unwrap();
        assert_eq!(unblocked["success"], true);
        let completed = registry
            .dispatch(
                "kanban_complete",
                json!({"task_id": child_id, "result": "done"}),
                ctx.clone(),
            )
            .await
            .unwrap();
        assert_eq!(completed["status"], "done");

        // Attachments + list filter.
        let attach = registry
            .dispatch(
                "kanban_attach_url",
                json!({"task_id": child_id, "url": "https://example.com/pr/1"}),
                ctx.clone(),
            )
            .await
            .unwrap();
        assert_eq!(attach["success"], true);
        let listed = registry
            .dispatch("kanban_list", json!({"status": "done"}), ctx.clone())
            .await
            .unwrap();
        assert_eq!(listed["count"], 1);
        assert_eq!(listed["tasks"][0]["id"], child_id);

        // The engine DB (same file the CLI uses) holds the rows.
        let store = crate::kanban::KanbanStore::open(ctx.home.join("kanban.db")).unwrap();
        assert!(store.get_task(&child_id).unwrap().is_some());
        assert_eq!(store.comments(&child_id).unwrap().len(), 2);
        assert_eq!(store.attachments(&child_id).unwrap().len(), 1);
        assert_eq!(store.parents_of(&child_id).unwrap(), vec![parent_id]);
    }
}

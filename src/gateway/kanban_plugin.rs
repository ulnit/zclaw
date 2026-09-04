//! `/api/plugins/kanban/*` — the desktop kanban plugin's REST namespace.
//!
//! The vendored desktop ships the hermes kanban plugin UI
//! (`desktop-electron/src/plugins/kanban/`); its data layer speaks
//! `ctx.rest` scoped to `/api/plugins/kanban` (hermes
//! `plugins/kanban/dashboard/plugin_api.py` mounted under
//! `/api/plugins/<name>`). This module serves that contract on top of
//! the same `KanbanStore` engine the CLI and `/api/kanban/*` use.
//! Payload shapes follow the renderer's `plugins/kanban/types.ts`.

use axum::{
    extract::{Multipart, Path, Query, State},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use crate::kanban::{KanbanStore, NewTask};

use super::GatewayState;

/// Column order = hermes `BOARD_COLUMNS`; the archived lane is appended
/// only when the client asks for it.
const BOARD_COLUMNS: &[&str] = &[
    "triage",
    "todo",
    "scheduled",
    "ready",
    "running",
    "blocked",
    "review",
    "done",
];

fn store() -> Result<KanbanStore, Response> {
    KanbanStore::open_default().map_err(|e| super::server_error(&e.to_string()))
}

fn resolve_task(store: &KanbanStore, id: &str) -> Result<String, Response> {
    if let Ok(Some(task)) = store.get_task(id) {
        return Ok(task.id);
    }
    match store.resolve_task_id(id) {
        Ok(Some(id)) => Ok(id),
        Ok(None) => Err(super::not_found(&format!("task {id} not found"))),
        Err(e) => Err(super::server_error(&e.to_string())),
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct BoardQuery {
    board: Option<String>,
    include_archived: Option<String>,
    tail: Option<u64>,
    #[serde(default)]
    tenant: Option<String>,
}

fn resolve_board(store: &KanbanStore, query: &BoardQuery) -> String {
    query
        .board
        .clone()
        .filter(|b| !b.trim().is_empty())
        .unwrap_or_else(|| store.current_board().unwrap_or_else(|_| "default".into()))
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Card + task JSON assembly (renderer KanbanTask / KanbanTaskFull)
// ---------------------------------------------------------------------------

fn card_json(store: &KanbanStore, task: &crate::kanban::Task, diagnostics: &[Value]) -> Value {
    let comments = store.comments(&task.id).unwrap_or_default().len();
    let parents = store.parents_of(&task.id).unwrap_or_default();
    let children = store.children_of(&task.id).unwrap_or_default();
    let mut done = 0usize;
    for child in &children {
        if let Ok(Some(child_task)) = store.get_task(child) {
            if child_task.status == "done" {
                done += 1;
            }
        }
    }
    let latest_summary = store.latest_summary(&task.id).unwrap_or(None);
    let warnings = if diagnostics.is_empty() {
        Value::Null
    } else {
        let highest = diagnostics
            .iter()
            .filter_map(|d| d["severity"].as_str())
            .min_by_key(|severity| match *severity {
                "critical" => 0,
                "error" => 1,
                _ => 2,
            })
            .map(str::to_string);
        json!({ "count": diagnostics.len(), "highest_severity": highest })
    };
    json!({
        "id": task.id,
        "title": task.title,
        "body": task.body,
        "status": task.status,
        "assignee": task.assignee,
        "priority": task.priority,
        "tenant": task.tenant,
        "created_at": task.created_at,
        "latest_summary": latest_summary,
        "comment_count": comments,
        "link_counts": { "parents": parents.len(), "children": children.len() },
        "progress": if children.is_empty() { Value::Null } else { json!({ "done": done, "total": children.len() }) },
        "warnings": warnings,
        "started_at": task.started_at,
        "worker_pid": task.worker_pid,
        "last_heartbeat_at": task.last_heartbeat_at,
    })
}

fn diagnostics_for(
    store: &KanbanStore,
    config: &crate::config::UlncLawConfig,
    task: &crate::kanban::Task,
) -> Vec<Value> {
    if task.status == "done" || task.status == "archived" {
        return Vec::new();
    }
    serde_json::to_value(crate::kanban_diagnostics::compute_task_diagnostics(
        store, config, task,
    ))
    .unwrap_or_else(|_| Value::Array(Vec::new()))
    .as_array()
    .cloned()
    .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// GET /board — full board grouped by status column
// ---------------------------------------------------------------------------

pub async fn board(Query(query): Query<BoardQuery>) -> Response {
    let store = match store() {
        Ok(s) => s,
        Err(e) => return e,
    };
    let board_slug = resolve_board(&store, &query);
    let include_archived = query.include_archived.as_deref() == Some("true");
    let tasks = match store.list_tasks(Some(&board_slug), None, None, None, 2000) {
        Ok(tasks) => tasks,
        Err(e) => return super::server_error(&e.to_string()),
    };
    let config = crate::config::UlncLawConfig::load(None).unwrap_or_default();

    let mut columns: BTreeMap<usize, Vec<Value>> = BTreeMap::new();
    let mut archived: Vec<Value> = Vec::new();
    let mut tenants: Vec<String> = Vec::new();
    let mut assignees: Vec<String> = Vec::new();

    for task in &tasks {
        if let Some(tenant) = task.tenant.as_deref() {
            if !tenants.iter().any(|t| t == tenant) {
                tenants.push(tenant.to_string());
            }
        }
        if let Some(assignee) = task.assignee.as_deref() {
            if !assignees.iter().any(|a| a == assignee) {
                assignees.push(assignee.to_string());
            }
        }
        if task.status == "archived" {
            if include_archived {
                archived.push(card_json(&store, task, &[]));
            }
            continue;
        }
        if let Some(tenant_filter) = query.tenant.as_deref() {
            if task.tenant.as_deref() != Some(tenant_filter) {
                continue;
            }
        }
        let diagnostics = diagnostics_for(&store, &config, task);
        let card = card_json(&store, task, &diagnostics);
        let index = BOARD_COLUMNS
            .iter()
            .position(|column| *column == task.status.as_str())
            .unwrap_or(1); // unknown statuses land in todo
        columns.entry(index).or_default().push(card);
    }

    let mut column_rows: Vec<Value> = BOARD_COLUMNS
        .iter()
        .enumerate()
        .map(|(index, name)| {
            json!({
                "name": name,
                "tasks": columns.remove(&index).unwrap_or_default(),
            })
        })
        .collect();
    if include_archived {
        column_rows.push(json!({ "name": "archived", "tasks": archived }));
    }

    let latest_event_id = store.last_event_id().unwrap_or(0);
    Json(json!({
        "columns": column_rows,
        "tenants": tenants,
        "assignees": assignees,
        "latest_event_id": latest_event_id,
        "now": now_secs(),
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// Boards: GET /boards, POST /boards, PATCH /boards/:slug
// ---------------------------------------------------------------------------

fn board_meta(store: &KanbanStore, board_row: &crate::kanban::Board, current: &str) -> Value {
    let total = store
        .list_tasks(Some(&board_row.slug), None, None, None, 5000)
        .map(|tasks| tasks.len())
        .unwrap_or(0);
    let workdir = board_row.default_workdir.clone().unwrap_or_default();
    let workspace_kind = if workdir.is_empty() {
        "scratch"
    } else if std::path::Path::new(&workdir).join(".git").exists() {
        "worktree"
    } else {
        "dir"
    };
    json!({
        "slug": board_row.slug,
        "name": board_row.name,
        "description": Value::Null,
        "is_current": board_row.slug == current,
        "total": total,
        "default_workdir": workdir,
        "default_workspace_kind": workspace_kind,
        "project_id": Value::Null,
        "project_name": Value::Null,
    })
}

pub async fn list_boards() -> Response {
    let store = match store() {
        Ok(s) => s,
        Err(e) => return e,
    };
    let current = store.current_board().unwrap_or_else(|_| "default".into());
    let boards = match store.list_boards() {
        Ok(boards) => boards,
        Err(e) => return super::server_error(&e.to_string()),
    };
    let rows: Vec<Value> = boards
        .iter()
        .map(|board_row| board_meta(&store, board_row, &current))
        .collect();
    Json(json!({ "boards": rows, "current": current })).into_response()
}

#[derive(Debug, Deserialize, Default)]
pub struct CreateBoardBody {
    slug: Option<String>,
    name: Option<String>,
    /// Accepted for hermes-contract compatibility; ulnclaw boards carry
    /// no project link yet (Project -> board binding is the CLI's job).
    #[serde(default, rename = "project_id")]
    _project_id: Option<String>,
}

pub async fn create_board(Json(body): Json<CreateBoardBody>) -> Response {
    let store = match store() {
        Ok(s) => s,
        Err(e) => return e,
    };
    let slug = body.slug.unwrap_or_default().trim().to_string();
    if slug.is_empty() {
        return super::bad_request("slug is required", None);
    }
    let name = body.name.unwrap_or_else(|| slug.clone());
    match store.create_board(&slug, Some(&name), None) {
        Ok(()) => Json(json!({
            "board": {
                "slug": slug,
                "name": name,
            }
        }))
        .into_response(),
        Err(e) => super::bad_request(&e.to_string(), None),
    }
}

pub async fn update_board(Path(slug): Path<String>, Json(body): Json<Value>) -> Response {
    let store = match store() {
        Ok(s) => s,
        Err(e) => return e,
    };
    if body.get("name").and_then(Value::as_str).is_some() {
        let name = body["name"].as_str().unwrap_or_default();
        if let Err(e) = store.rename_board(&slug, name) {
            return super::bad_request(&e.to_string(), None);
        }
    }
    if let Some(workdir) = body.get("default_workdir") {
        let value = workdir.as_str().unwrap_or_default().trim();
        let value = if value.is_empty() { None } else { Some(value) };
        if let Err(e) = store.set_board_workdir(&slug, value) {
            return super::bad_request(&e.to_string(), None);
        }
    }
    let boards = match store.list_boards() {
        Ok(boards) => boards,
        Err(e) => return super::server_error(&e.to_string()),
    };
    let current = store.current_board().unwrap_or_else(|_| "default".into());
    match boards.iter().find(|board_row| board_row.slug == slug) {
        Some(board_row) => {
            Json(json!({ "board": board_meta(&store, board_row, &current) })).into_response()
        }
        None => super::not_found(&format!("board {slug} not found")),
    }
}

// ---------------------------------------------------------------------------
// Task detail / log / attachments
// ---------------------------------------------------------------------------

pub async fn task_detail(Path(id): Path<String>, Query(query): Query<BoardQuery>) -> Response {
    let _ = query;
    let store = match store() {
        Ok(s) => s,
        Err(e) => return e,
    };
    let id = match resolve_task(&store, &id) {
        Ok(id) => id,
        Err(e) => return e,
    };
    let task = match store.get_task(&id) {
        Ok(Some(task)) => task,
        Ok(None) => return super::not_found(&format!("task {id} not found")),
        Err(e) => return super::server_error(&e.to_string()),
    };
    let config = crate::config::UlncLawConfig::load(None).unwrap_or_default();
    let diagnostics = diagnostics_for(&store, &config, &task);
    let mut full = card_json(&store, &task, &diagnostics);
    let full_map = full.as_object_mut().expect("card is an object");
    full_map.insert("result".into(), json!(task.result));
    full_map.insert("created_by".into(), json!(task.created_by));
    full_map.insert("model_override".into(), json!(task.model));
    full_map.insert("provider_override".into(), json!(task.provider));
    full_map.insert("reasoning_effort".into(), json!(task.reasoning_effort));
    full_map.insert("completed_at".into(), json!(task.completed_at));
    full_map.insert("diagnostics".into(), Value::Array(diagnostics));
    let last_failure_error = store
        .latest_run(&task.id)
        .ok()
        .flatten()
        .and_then(|run| run.error);
    full_map.insert("last_failure_error".into(), json!(last_failure_error));

    let comments: Vec<Value> = store
        .comments(&task.id)
        .unwrap_or_default()
        .into_iter()
        .map(|comment| {
            json!({
                "id": comment.id,
                "author": comment.author,
                "body": comment.body,
                "created_at": comment.created_at,
            })
        })
        .collect();
    let events: Vec<Value> = store
        .events(&task.id)
        .unwrap_or_default()
        .into_iter()
        .map(|event| {
            json!({
                "id": event.id,
                "kind": event.kind,
                "payload": event.payload,
                "created_at": event.created_at,
            })
        })
        .collect();
    let attachments: Vec<Value> = store
        .attachments_with_ids(&task.id)
        .unwrap_or_default()
        .into_iter()
        .map(|(attachment_id, kind, value)| {
            let mut row = json!({
                "id": attachment_id,
                "filename": value,
                "size": Value::Null,
            });
            if kind == "file" {
                let path =
                    crate::kanban::task_attachments_dir(&crate::config::ulnclaw_home(), &task.id)
                        .join(&value);
                if let Ok(metadata) = std::fs::metadata(&path) {
                    row["size"] = json!(metadata.len());
                }
            }
            row
        })
        .collect();
    let runs: Vec<Value> = store
        .list_runs(&task.id, true, None, None)
        .unwrap_or_default()
        .into_iter()
        .map(|run| {
            json!({
                "id": run.id,
                "profile": run.profile,
                "status": run.status,
                "outcome": run.outcome,
                "summary": run.summary,
                "error": run.error,
                "metadata": run.metadata,
                "worker_pid": run.worker_pid,
                "started_at": run.started_at,
                "ended_at": run.ended_at,
            })
        })
        .collect();

    Json(json!({
        "task": full,
        "comments": comments,
        "events": events,
        "attachments": attachments,
        "links": {
            "parents": store.parents_of(&task.id).unwrap_or_default(),
            "children": store.children_of(&task.id).unwrap_or_default(),
        },
        "runs": runs,
    }))
    .into_response()
}

pub async fn task_log(Path(id): Path<String>, Query(query): Query<BoardQuery>) -> Response {
    let store = match store() {
        Ok(s) => s,
        Err(e) => return e,
    };
    let id = match resolve_task(&store, &id) {
        Ok(id) => id,
        Err(e) => return e,
    };
    let home = crate::config::ulnclaw_home();
    let tail = query.tail.unwrap_or(16 * 1024);
    let path = crate::kanban::worker_log_path(&home, &id);
    let size_bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    match crate::kanban::read_worker_log(&home, &id, Some(tail)) {
        Some(content) => {
            let truncated = (size_bytes as usize) > content.len();
            Json(json!({
                "exists": true,
                "size_bytes": size_bytes,
                "content": content,
                "truncated": truncated,
            }))
            .into_response()
        }
        None => Json(json!({
            "exists": false,
            "size_bytes": 0,
            "content": "",
            "truncated": false,
        }))
        .into_response(),
    }
}

pub async fn list_attachments(Path(id): Path<String>) -> Response {
    let store = match store() {
        Ok(s) => s,
        Err(e) => return e,
    };
    let id = match resolve_task(&store, &id) {
        Ok(id) => id,
        Err(e) => return e,
    };
    let rows: Vec<Value> = store
        .attachments_with_ids(&id)
        .unwrap_or_default()
        .into_iter()
        .map(|(attachment_id, _kind, value)| json!({ "id": attachment_id, "filename": value }))
        .collect();
    Json(json!({ "attachments": rows })).into_response()
}

/// Multipart upload: one file field (any name) per attachment.
pub async fn upload_attachment(Path(id): Path<String>, mut multipart: Multipart) -> Response {
    let store = match store() {
        Ok(s) => s,
        Err(e) => return e,
    };
    let id = match resolve_task(&store, &id) {
        Ok(id) => id,
        Err(e) => return e,
    };
    let home = crate::config::ulnclaw_home();
    let dir = crate::kanban::task_attachments_dir(&home, &id);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return super::server_error(&format!("create attachments dir: {e}"));
    }
    let mut saved: Vec<Value> = Vec::new();
    while let Ok(Some(field)) = multipart.next_field().await {
        let filename = field
            .file_name()
            .map(str::to_string)
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| format!("attachment-{}", now_secs()));
        let safe_name = std::path::Path::new(&filename)
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| format!("attachment-{}", now_secs()));
        let bytes = match field.bytes().await {
            Ok(bytes) => bytes,
            Err(e) => return super::bad_request(&format!("read upload: {e}"), None),
        };
        if let Err(e) = std::fs::write(dir.join(&safe_name), &bytes) {
            return super::server_error(&format!("write attachment: {e}"));
        }
        if let Err(e) = store.attach(&id, "file", &safe_name) {
            return super::server_error(&e.to_string());
        }
        saved.push(json!({ "filename": safe_name, "size": bytes.len() }));
    }
    if saved.is_empty() {
        return super::bad_request("no file field in upload", None);
    }
    Json(json!({ "ok": true, "attachments": saved })).into_response()
}

// ---------------------------------------------------------------------------
// Task writes: create / bulk / patch / delete / comments / reassign / reclaim
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
pub struct CreateTaskBody {
    title: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    assignee: Option<String>,
    #[serde(default)]
    priority: Option<i64>,
    #[serde(default)]
    tenant: Option<String>,
    #[serde(default)]
    skills: Option<Vec<String>>,
    #[serde(default)]
    parents: Option<Vec<String>>,
    #[serde(default)]
    triage: Option<bool>,
    #[serde(default)]
    goal_mode: Option<bool>,
    #[serde(default)]
    model_override: Option<String>,
    #[serde(default)]
    provider_override: Option<String>,
    #[serde(default)]
    reasoning_effort: Option<String>,
    #[serde(default)]
    board: Option<String>,
}

pub async fn create_task(
    State(_state): State<Arc<GatewayState>>,
    Json(body): Json<CreateTaskBody>,
) -> Response {
    let store = match store() {
        Ok(s) => s,
        Err(e) => return e,
    };
    let title = body.title.unwrap_or_default().trim().to_string();
    if title.is_empty() {
        return super::bad_request("title is required", None);
    }
    let new_task = NewTask {
        title,
        body: body.body.unwrap_or_default(),
        assignee: body.assignee.filter(|a| !a.trim().is_empty()),
        priority: body.priority.unwrap_or(0),
        tenant: body.tenant,
        model: body.model_override,
        created_by: "desktop".into(),
        skills: body.skills,
        max_runtime_seconds: None,
        provider: body.provider_override,
        reasoning_effort: body.reasoning_effort,
        idempotency_key: None,
        triage: body.triage.unwrap_or(false),
        goal_mode: body.goal_mode.unwrap_or(false),
        ..Default::default()
    };
    // create_task pins the card to the CURRENT board: hop boards when the
    // client scoped another one, then restore the pointer.
    let requested_board = body.board.filter(|b| !b.trim().is_empty());
    let previous_board = store.current_board().ok();
    if let Some(slug) = &requested_board {
        if Some(slug) != previous_board.as_ref() {
            if let Err(e) = store.switch_board(slug) {
                return super::bad_request(&e.to_string(), None);
            }
        }
    }
    let created = store.create_task(&new_task);
    if let (Some(slug), Some(previous)) = (&requested_board, &previous_board) {
        if Some(slug) != Some(&previous) {
            let _ = store.switch_board(&previous);
        }
    }
    let task = match created {
        Ok(task) => task,
        Err(e) => return super::bad_request(&e.to_string(), None),
    };
    let mut warning: Option<String> = None;
    if let Some(parents) = body.parents {
        for parent in parents {
            let Ok(Some(parent_task)) = store.get_task(&parent) else {
                warning = Some(format!("unknown parent {parent} skipped"));
                continue;
            };
            if let Err(e) = store.link_tasks(&parent_task.id, &task.id) {
                warning = Some(format!("link {parent}: {e}"));
            }
        }
    }
    let config = crate::config::UlncLawConfig::load(None).unwrap_or_default();
    let diagnostics = diagnostics_for(&store, &config, &task);
    Json(json!({
        "task": card_json(&store, &task, &diagnostics),
        "warning": warning,
    }))
    .into_response()
}

#[derive(Debug, Deserialize, Default)]
pub struct BulkTasksBody {
    #[serde(default)]
    ids: Vec<String>,
    #[serde(flatten)]
    patch: Value,
}

pub async fn bulk_tasks(Json(body): Json<BulkTasksBody>) -> Response {
    let mut results: Vec<Value> = Vec::new();
    for id in body.ids {
        let outcome = patch_one_task(&id, &body.patch).await;
        match outcome {
            Ok(()) => results.push(json!({ "id": id, "ok": true })),
            Err(message) => results.push(json!({ "id": id, "ok": false, "error": message })),
        }
    }
    Json(json!({ "results": results })).into_response()
}

pub async fn update_task(Path(id): Path<String>, Json(patch): Json<Value>) -> Response {
    match patch_one_task(&id, &patch).await {
        Ok(()) => Json(json!({ "ok": true, "id": id })).into_response(),
        Err(message) => super::bad_request(&message, None),
    }
}

/// Apply one desktop patch to a task. Mirrors hermes `update_task`:
/// lifecycle primitives for status moves, direct writes for drag-drop
/// columns, override setters, priority/title/body edits.
async fn patch_one_task(id: &str, patch: &Value) -> Result<(), String> {
    let store = KanbanStore::open_default().map_err(|e| e.to_string())?;
    let id = match resolve_task(&store, id) {
        Ok(id) => id,
        Err(_) => return Err(format!("task {id} not found")),
    };
    let task = store
        .get_task(&id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("task {id} not found"))?;

    if let Some(assignee) = patch.get("assignee").and_then(Value::as_str) {
        store
            .assign_task(&id, assignee)
            .map_err(|e| e.to_string())?;
    }

    if let Some(status) = patch.get("status").and_then(Value::as_str) {
        match status {
            "done" => {
                let result = patch.get("result").and_then(Value::as_str);
                store
                    .complete_task(&id, result)
                    .map_err(|e| e.to_string())?;
            }
            "blocked" => {
                let reason = patch
                    .get("block_reason")
                    .and_then(Value::as_str)
                    .unwrap_or("blocked from the desktop board");
                store.block_task(&id, reason).map_err(|e| e.to_string())?;
            }
            "scheduled" => {
                let reason = patch
                    .get("block_reason")
                    .and_then(Value::as_str)
                    .unwrap_or("scheduled from the desktop board");
                store
                    .schedule_task(&id, reason)
                    .map_err(|e| e.to_string())?;
            }
            "ready" => {
                if task.status == "blocked" || task.status == "scheduled" {
                    store.unblock_task(&id).map_err(|e| e.to_string())?;
                } else {
                    store
                        .set_status_direct(&id, "ready")
                        .map_err(|e| e.to_string())?;
                }
            }
            "archived" => {
                store.archive_task(&id).map_err(|e| e.to_string())?;
            }
            "running" => {
                return Err(
                    "Cannot set status to 'running' directly; use the dispatcher/claim path".into(),
                );
            }
            "todo" | "triage" | "review" => {
                store
                    .set_status_direct(&id, status)
                    .map_err(|e| e.to_string())?;
            }
            other => return Err(format!("unknown status: {other}")),
        }
    }

    let clear_model = patch
        .get("clear_model_override")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if clear_model || patch.get("model_override").is_some() {
        let model = patch
            .get("model_override")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|model| !model.is_empty());
        let provider = patch.get("provider_override").and_then(Value::as_str);
        store
            .set_model(&id, model, provider)
            .map_err(|e| e.to_string())?;
    }

    let clear_effort = patch
        .get("clear_reasoning_effort")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if clear_effort || patch.get("reasoning_effort").is_some() {
        let effort = patch.get("reasoning_effort").and_then(Value::as_str);
        store
            .set_reasoning_effort(&id, effort)
            .map_err(|e| e.to_string())?;
    }

    if let Some(priority) = patch.get("priority").and_then(Value::as_i64) {
        store
            .set_priority(&id, priority)
            .map_err(|e| e.to_string())?;
    }

    let title = patch.get("title").and_then(Value::as_str);
    let body = patch.get("body").and_then(Value::as_str);
    if title.is_some() || body.is_some() {
        store
            .edit_task(&id, title, body)
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub async fn delete_task(Path(id): Path<String>) -> Response {
    let store = match store() {
        Ok(s) => s,
        Err(e) => return e,
    };
    let id = match resolve_task(&store, &id) {
        Ok(id) => id,
        Err(e) => return e,
    };
    // The desktop deletes live cards: archive first when needed, then remove.
    if let Ok(Some(task)) = store.get_task(&id) {
        if task.status != "archived" {
            if let Err(e) = store.archive_task(&id) {
                return super::bad_request(&e.to_string(), None);
            }
        }
    }
    match store.delete_archived_task(&id) {
        Ok(true) => Json(json!({ "ok": true, "id": id })).into_response(),
        Ok(false) => super::not_found(&format!("task {id} not found")),
        Err(e) => super::server_error(&e.to_string()),
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct CommentBody {
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    body: Option<String>,
}

pub async fn add_comment(Path(id): Path<String>, Json(body): Json<CommentBody>) -> Response {
    let store = match store() {
        Ok(s) => s,
        Err(e) => return e,
    };
    let id = match resolve_task(&store, &id) {
        Ok(id) => id,
        Err(e) => return e,
    };
    let author = body.author.unwrap_or_else(|| "desktop".into());
    let text = body.body.unwrap_or_default();
    if text.trim().is_empty() {
        return super::bad_request("comment body is required", None);
    }
    match store.add_comment(&id, &author, &text) {
        Ok(()) => Json(json!({ "ok": true, "id": id })).into_response(),
        Err(e) => super::server_error(&e.to_string()),
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct ReassignBody {
    #[serde(default)]
    profile: Option<String>,
    #[serde(default)]
    reclaim_first: Option<bool>,
}

pub async fn reassign(Path(id): Path<String>, Json(body): Json<ReassignBody>) -> Response {
    let store = match store() {
        Ok(s) => s,
        Err(e) => return e,
    };
    let id = match resolve_task(&store, &id) {
        Ok(id) => id,
        Err(e) => return e,
    };
    let profile = body.profile.unwrap_or_default();
    if profile.trim().is_empty() {
        return super::bad_request("profile is required", None);
    }
    match store.reassign_task(&id, &profile, body.reclaim_first.unwrap_or(false), "reassigned from the desktop") {
        Ok(task) => Json(json!({ "ok": true, "task": { "id": task.id, "status": task.status, "assignee": task.assignee } })).into_response(),
        Err(e) => super::bad_request(&e.to_string(), None),
    }
}

pub async fn reclaim(Path(id): Path<String>) -> Response {
    let store = match store() {
        Ok(s) => s,
        Err(e) => return e,
    };
    let id = match resolve_task(&store, &id) {
        Ok(id) => id,
        Err(e) => return e,
    };
    match store.reclaim_task(&id, "reclaimed from the desktop") {
        Ok(task) => Json(json!({ "ok": true, "task": { "id": task.id, "status": task.status } }))
            .into_response(),
        Err(e) => super::bad_request(&e.to_string(), None),
    }
}

// ---------------------------------------------------------------------------
// Estimate (auxiliary model; hermes `_run_estimate` contract — never an
// HTTP error, failures answer `{ok: false, reason}`)
// ---------------------------------------------------------------------------

async fn run_estimate(state: &Arc<GatewayState>, title: &str, body: Option<&str>) -> Value {
    let title = title.trim();
    if title.is_empty() {
        return json!({ "ok": false, "reason": "a title is required to estimate" });
    }
    let config = match crate::config::UlncLawConfig::load(None) {
        Ok(config) => config,
        Err(e) => return json!({ "ok": false, "reason": format!("config: {e}") }),
    };
    let cap_title: String = title.chars().take(400).collect();
    let body_text = body
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .unwrap_or("(none)");
    let cap_body: String = body_text.chars().take(4000).collect();
    let user_msg = format!(
        "Title: {cap_title}\n\nDescription:\n{cap_body}\n\n\
         Estimate this task. Reply with ONLY JSON: \
         {{\"est_tokens\": <int total tokens the worker will likely need>, \
         \"complexity\": \"S\"|\"M\"|\"L\", \"rationale\": \"<one sentence>\"}}"
    );
    let main = state.agent.provider();
    let resolution = match crate::provider::auxiliary::resolve_aux_task(&config, "estimate", main) {
        Ok(resolution) => resolution,
        Err(e) => return json!({ "ok": false, "reason": format!("auxiliary routing: {e}") }),
    };
    let request = crate::provider::ProviderRequest {
        messages: vec![crate::provider::Message {
            role: crate::provider::Role::User,
            content: Some(user_msg),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }],
        tools: Vec::new(),
        model: resolution.model.clone(),
        max_tokens: Some(300),
        temperature: Some(0.2),
        stream: false,
        stop: None,
        images: None,
    };
    // Bounded: the renderer's estimate button must never hang on a dead
    // provider (retry backoff can stretch minutes).
    let response = match tokio::time::timeout(
        std::time::Duration::from_secs(20),
        resolution.provider.chat_completion(request),
    )
    .await
    {
        Ok(Ok(response)) => response,
        Ok(Err(e)) => return json!({ "ok": false, "reason": format!("estimate call failed: {e}") }),
        Err(_) => return json!({ "ok": false, "reason": "estimate timed out" }),
    };
    let text = response.content.unwrap_or_default();
    let parsed: Value = serde_json::from_str(
        text.trim()
            .trim_start_matches("```json")
            .trim_start_matches("```")
            .trim_end_matches("```")
            .trim(),
    )
    .unwrap_or(Value::Null);
    if parsed.is_null() {
        return json!({ "ok": false, "reason": "unparseable estimate reply", "model": resolution.model });
    }
    json!({
        "ok": true,
        "est_tokens": parsed.get("est_tokens").cloned().unwrap_or(Value::Null),
        "complexity": parsed.get("complexity").cloned().unwrap_or(Value::Null),
        "rationale": parsed.get("rationale").cloned().unwrap_or(Value::Null),
        "model": resolution.model,
    })
}

#[derive(Debug, Deserialize, Default)]
pub struct EstimateBody {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    body: Option<String>,
}

pub async fn estimate_text(
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<EstimateBody>,
) -> Response {
    let reply = run_estimate(
        &state,
        &body.title.unwrap_or_default(),
        body.body.as_deref(),
    )
    .await;
    Json(reply).into_response()
}

pub async fn estimate_task(
    State(state): State<Arc<GatewayState>>,
    Path(id): Path<String>,
) -> Response {
    let store = match store() {
        Ok(s) => s,
        Err(e) => return e,
    };
    let id = match resolve_task(&store, &id) {
        Ok(id) => id,
        Err(e) => return e,
    };
    let task = match store.get_task(&id) {
        Ok(Some(task)) => task,
        Ok(None) => return super::not_found(&format!("task {id} not found")),
        Err(e) => return super::server_error(&e.to_string()),
    };
    let reply = run_estimate(&state, &task.title, Some(&task.body)).await;
    Json(reply).into_response()
}

// ---------------------------------------------------------------------------
// Profiles / projects / orchestration / dispatch
// ---------------------------------------------------------------------------

fn profiles_json_path() -> std::path::PathBuf {
    crate::config::ulnclaw_home().join("kanban-profiles.json")
}

fn load_profile_descriptions() -> HashMap<String, String> {
    std::fs::read_to_string(profiles_json_path())
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

pub async fn profiles() -> Response {
    let config = crate::config::UlncLawConfig::load(None).unwrap_or_default();
    let descriptions = load_profile_descriptions();
    let store = store();
    let mut names: Vec<String> = config.profiles.keys().cloned().collect();
    if names.is_empty() {
        names.push("default".into());
    }
    for name in descriptions.keys() {
        if !names.iter().any(|existing| existing == name) {
            names.push(name.clone());
        }
    }
    if let Ok(store) = &store {
        if let Ok(tasks) = store.list_tasks(None, None, None, None, 2000) {
            for task in tasks {
                if let Some(assignee) = task.assignee {
                    if !names.iter().any(|name| *name == assignee) {
                        names.push(assignee);
                    }
                }
            }
        }
    }
    names.sort();
    let rows: Vec<Value> = names
        .iter()
        .map(|name| {
            let description = descriptions.get(name).cloned().unwrap_or_default();
            json!({
                "name": name,
                "is_default": name == "default" || names.len() == 1,
                "description": description,
                "description_auto": description.is_empty(),
            })
        })
        .collect();
    Json(json!({ "profiles": rows })).into_response()
}

pub async fn update_profile(Path(name): Path<String>, Json(body): Json<Value>) -> Response {
    let description = body
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let mut descriptions = load_profile_descriptions();
    if description.trim().is_empty() {
        descriptions.remove(&name);
    } else {
        descriptions.insert(name.clone(), description);
    }
    if let Err(e) = std::fs::write(
        profiles_json_path(),
        serde_json::to_string_pretty(&descriptions).unwrap_or_default(),
    ) {
        return super::server_error(&format!("persist profile description: {e}"));
    }
    Json(json!({ "ok": true, "name": name })).into_response()
}

pub async fn projects() -> Response {
    let conn = match crate::projects_db::connect(None) {
        Ok(conn) => conn,
        Err(e) => return super::server_error(&e.to_string()),
    };
    let rows = match crate::projects_db::list_projects(&conn, false) {
        Ok(rows) => rows,
        Err(e) => return super::server_error(&e.to_string()),
    };
    let projects: Vec<Value> = rows
        .iter()
        .map(|project| {
            json!({
                "id": project.id,
                "slug": project.slug,
                "name": project.name,
                "primary_path": project.primary_path,
                "icon": project.icon,
                "color": project.color,
            })
        })
        .collect();
    Json(json!({ "projects": projects })).into_response()
}

pub async fn orchestration_get() -> Response {
    let config = crate::config::UlncLawConfig::load(None).unwrap_or_default();
    let orchestrator = config
        .kanban
        .orchestrator_profile
        .clone()
        .unwrap_or_default();
    let assignee = config.kanban.default_assignee.clone().unwrap_or_default();
    Json(json!({
        "orchestrator_profile": orchestrator,
        "default_assignee": assignee,
        "auto_decompose": config.kanban.auto_decompose,
        "resolved_orchestrator_profile": if orchestrator.is_empty() { "default" } else { orchestrator.as_str() },
        "resolved_default_assignee": if assignee.is_empty() { "default" } else { assignee.as_str() },
    }))
    .into_response()
}

pub async fn orchestration_put(Json(body): Json<Value>) -> Response {
    for (key, dotted) in [
        ("orchestrator_profile", "kanban.orchestrator_profile"),
        ("default_assignee", "kanban.default_assignee"),
    ] {
        if let Some(value) = body.get(key).and_then(Value::as_str) {
            let result: Result<(), String> = if value.trim().is_empty() {
                crate::config_cmd::unset_config_value(dotted).map(|_| ())
            } else {
                crate::config_cmd::set_config_value(dotted, value.trim(), true).map(|_| ())
            };
            if let Err(e) = result {
                if !e.contains("not set") {
                    return super::server_error(&e);
                }
            }
        }
    }
    if let Some(auto) = body.get("auto_decompose").and_then(Value::as_bool) {
        if let Err(e) =
            crate::config_cmd::set_config_value("kanban.auto_decompose", &auto.to_string(), true)
        {
            return super::server_error(&e);
        }
    }
    orchestration_get().await
}

pub async fn dispatch() -> Response {
    let body = super::kanban::DispatchBody {
        max_spawn: None,
        dry_run: None,
    };
    super::kanban::dispatch(Json(body)).await
}

pub async fn assignees() -> Response {
    let store = match store() {
        Ok(s) => s,
        Err(e) => return e,
    };
    let mut names: Vec<String> = Vec::new();
    if let Ok(tasks) = store.list_tasks(None, None, None, None, 2000) {
        for task in tasks {
            if let Some(assignee) = task.assignee {
                if !names.iter().any(|name| *name == assignee) {
                    names.push(assignee);
                }
            }
        }
    }
    names.sort();
    Json(json!({ "assignees": names })).into_response()
}

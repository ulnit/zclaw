//! Kanban board HTTP API — the desktop widget and any external client talk
//! to these endpoints; they share the same `KanbanStore` engine (and the
//! same `kanban.db`) as the `ulnclaw kanban` CLI and the agent-side
//! `kanban_*` tools (hermes parity: one board, three surfaces).

use axum::{
    extract::{Path, Query},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::kanban::{KanbanStore, NewTask, DEFAULT_CLAIM_TTL_SECS};

fn store() -> Result<KanbanStore, Response> {
    KanbanStore::open_default().map_err(|e| super::server_error(&e.to_string()))
}

/// Resolve a task id or unique prefix; 404s when unknown.
fn resolve(store: &KanbanStore, id: &str) -> Result<String, Response> {
    if let Ok(Some(task)) = store.get_task(id) {
        return Ok(task.id);
    }
    match store.resolve_task_id(id) {
        Ok(Some(id)) => Ok(id),
        Ok(None) => Err(super::not_found(&format!("task {id} not found"))),
        Err(e) => Err(super::server_error(&e.to_string())),
    }
}

fn task_json(store: &KanbanStore, task: &crate::kanban::Task) -> Value {
    let parents = store.parents_of(&task.id).unwrap_or_default();
    let children = store.children_of(&task.id).unwrap_or_default();
    json!({
        "id": task.id,
        "board": task.board,
        "title": task.title,
        "body": task.body,
        "assignee": task.assignee,
        "model": task.model,
        "provider": task.provider,
        "status": task.status,
        "priority": task.priority,
        "created_by": task.created_by,
        "created_at": task.created_at,
        "started_at": task.started_at,
        "completed_at": task.completed_at,
        "result": task.result,
        "claim_lock": task.claim_lock,
        "claim_expires": task.claim_expires,
        "last_heartbeat_at": task.last_heartbeat_at,
        "skills": task.skills,
        "reasoning_effort": task.reasoning_effort,
        "max_runtime_seconds": task.max_runtime_seconds,
        "idempotency_key": task.idempotency_key,
        "parents": parents,
        "children": children,
    })
}

/// `GET /api/kanban/boards` — boards with open/total task counts.
pub async fn list_boards() -> Response {
    let store = match store() {
        Ok(s) => s,
        Err(e) => return e,
    };
    let boards = match store.list_boards() {
        Ok(b) => b,
        Err(e) => return super::server_error(&e.to_string()),
    };
    let counts = store.board_task_counts().unwrap_or_default();
    let current = store.current_board().unwrap_or_default();
    let rows: Vec<Value> = boards
        .iter()
        .map(|b| {
            let (open, total) = counts
                .iter()
                .find(|(slug, _, _)| slug == &b.slug)
                .map(|(_, open, total)| (*open, *total))
                .unwrap_or((0, 0));
            json!({
                "slug": b.slug,
                "name": b.name,
                "default_workdir": b.default_workdir,
                "created_at": b.created_at,
                "current": b.slug == current,
                "open_tasks": open,
                "total_tasks": total,
            })
        })
        .collect();
    Json(json!({"object": "ulnclaw.kanban.board.list", "boards": rows})).into_response()
}

#[derive(Deserialize)]
pub struct CreateBoardBody {
    pub slug: String,
    pub name: Option<String>,
    pub default_workdir: Option<String>,
}

/// `POST /api/kanban/boards` — create a board.
pub async fn create_board(Json(body): Json<CreateBoardBody>) -> Response {
    let store = match store() {
        Ok(s) => s,
        Err(e) => return e,
    };
    match store.create_board(
        &body.slug,
        body.name.as_deref(),
        body.default_workdir.as_deref(),
    ) {
        Ok(board) => {
            Json(json!({"object": "ulnclaw.kanban.board", "board": board})).into_response()
        }
        Err(e) => super::bad_request(&e.to_string(), None),
    }
}

/// `POST /api/kanban/boards/:slug/switch` — set the current board.
pub async fn switch_board(Path(slug): Path<String>) -> Response {
    let store = match store() {
        Ok(s) => s,
        Err(e) => return e,
    };
    match store.switch_board(&slug) {
        Ok(()) => Json(json!({"ok": true, "current_board": slug})).into_response(),
        Err(e) => super::bad_request(&e.to_string(), None),
    }
}

#[derive(Deserialize, Default)]
pub struct ListTasksQuery {
    pub status: Option<String>,
    pub assignee: Option<String>,
    pub board: Option<String>,
    /// Workflow-template filter (hermes list --workflow-template-id).
    pub workflow_template_id: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Deserialize)]
pub struct RenameBoardBody {
    pub name: String,
}

/// `POST /api/kanban/boards/:slug/rename` — set a board's display
/// name (hermes `boards rename`).
pub async fn rename_board(Path(slug): Path<String>, Json(body): Json<RenameBoardBody>) -> Response {
    let store = match store() {
        Ok(s) => s,
        Err(e) => return e,
    };
    match store.rename_board(&slug, &body.name) {
        Ok(()) => {
            Json(json!({ "ok": true, "slug": slug, "name": body.name.trim() })).into_response()
        }
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("not found") {
                return super::not_found(&msg);
            }
            super::bad_request(&msg, None)
        }
    }
}

/// `DELETE /api/kanban/boards/:slug` — remove a board (hermes
/// `boards rm`): the default board is protected and boards with
/// non-archived tasks refuse removal.
pub async fn remove_board(Path(slug): Path<String>) -> Response {
    let store = match store() {
        Ok(s) => s,
        Err(e) => return e,
    };
    match store.remove_board(&slug) {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("not found") {
                return super::not_found(&msg);
            }
            super::bad_request(&msg, None)
        }
    }
}

#[derive(Deserialize)]
pub struct SetBoardWorkdirBody {
    pub workdir: Option<String>,
}

/// `POST /api/kanban/boards/:slug/workdir` — set or clear a board's
/// default working directory (hermes `boards set-workdir`). An empty
/// or absent `workdir` clears it; otherwise the path (a leading `~`
/// expands to the home directory) must resolve to an existing
/// directory, stored canonicalized.
pub async fn set_board_workdir(
    Path(slug): Path<String>,
    Json(body): Json<SetBoardWorkdirBody>,
) -> Response {
    let store = match store() {
        Ok(s) => s,
        Err(e) => return e,
    };
    let requested = body
        .workdir
        .as_deref()
        .map(str::trim)
        .filter(|w| !w.is_empty());
    let resolved: Option<String> = match requested {
        None => None,
        Some(raw) => {
            let expanded = if raw == "~" {
                match dirs::home_dir() {
                    Some(home) => home,
                    None => std::path::PathBuf::from(raw),
                }
            } else if let Some(rest) = raw.strip_prefix("~/") {
                match dirs::home_dir() {
                    Some(home) => home.join(rest),
                    None => std::path::PathBuf::from(raw),
                }
            } else {
                std::path::PathBuf::from(raw)
            };
            match std::fs::canonicalize(&expanded) {
                Ok(path) if path.is_dir() => Some(path.to_string_lossy().into_owned()),
                _ => {
                    return super::bad_request(
                        &format!("kanban: workdir {raw} is not an existing directory"),
                        None,
                    )
                }
            }
        }
    };
    match store.set_board_workdir(&slug, resolved.as_deref()) {
        Ok(()) => {
            Json(json!({ "ok": true, "slug": slug, "default_workdir": resolved })).into_response()
        }
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("not found") {
                return super::not_found(&msg);
            }
            super::bad_request(&msg, None)
        }
    }
}

/// `GET /api/kanban/tasks` — tasks on a board (default: current board).
pub async fn list_tasks(Query(query): Query<ListTasksQuery>) -> Response {
    let store = match store() {
        Ok(s) => s,
        Err(e) => return e,
    };
    let limit = query.limit.unwrap_or(200).max(1);
    let tasks = match store.list_tasks(
        query.board.as_deref(),
        query.status.as_deref().filter(|s| !s.is_empty()),
        query.assignee.as_deref().filter(|s| !s.is_empty()),
        query
            .workflow_template_id
            .as_deref()
            .filter(|s| !s.is_empty()),
        limit,
    ) {
        Ok(t) => t,
        Err(e) => return super::server_error(&e.to_string()),
    };
    let rows: Vec<Value> = tasks.iter().map(|t| task_json(&store, t)).collect();
    Json(json!({
        "object": "ulnclaw.kanban.task.list",
        "board": store.current_board().unwrap_or_default(),
        "count": rows.len(),
        "tasks": rows,
    }))
    .into_response()
}

#[derive(Deserialize)]
pub struct CreateTaskBody {
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub parents: Option<Vec<String>>,
    #[serde(default)]
    pub priority: Option<i64>,
    /// Per-task model override (hermes model_override).
    #[serde(default)]
    pub model: Option<String>,
    /// Provider the model belongs to (hermes provider_override);
    /// requires `model`.
    #[serde(default)]
    pub provider: Option<String>,
    /// Per-task reasoning-effort pin (hermes reasoning_effort):
    /// none|minimal|low|medium|high|xhigh|max|ultra; empty/absent =
    /// inherit the worker profile's setting.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// Link to a first-class Project id or slug (hermes create
    /// --project): anchors the worktree under the project's primary
    /// repo with a deterministic branch.
    #[serde(default)]
    pub project: Option<String>,
    /// Skills force-loaded into the dispatcher worker prompt.
    #[serde(default)]
    pub skills: Option<Vec<String>>,
    /// Per-attempt runtime cap in seconds (dispatcher-enforced).
    #[serde(default)]
    pub max_runtime_seconds: Option<i64>,
    /// Dedup key — an existing non-archived task with the key is returned.
    #[serde(default)]
    pub idempotency_key: Option<String>,
    /// Park in the triage column for the specifier/decomposer.
    #[serde(default)]
    pub triage: Option<bool>,
    /// Circuit-breaker threshold: block on the Nth failed attempt
    /// (hermes max_retries; must be >= 1).
    #[serde(default)]
    pub max_retries: Option<i64>,
    /// Park the card directly in a column (hermes create
    /// --initial-status): `running` (default flow) | `blocked`.
    #[serde(default)]
    pub initial_status: Option<String>,
    /// Workflow template hook (hermes workflow_template_id).
    #[serde(default)]
    pub workflow_template_id: Option<String>,
    /// Workflow step hook (hermes current_step_key).
    #[serde(default)]
    pub current_step_key: Option<String>,
    /// Goal-loop worker (hermes create --goal).
    #[serde(default)]
    pub goal_mode: Option<bool>,
    /// Goal-loop turn budget (hermes create --goal-max-turns; must be
    /// >= 1 when set).
    #[serde(default)]
    pub goal_max_turns: Option<i64>,
    /// Workspace: `scratch` | `worktree` | `worktree:<path>` |
    /// `dir:<path>` (hermes create --workspace).
    #[serde(default)]
    pub workspace: Option<String>,
    /// Worktree branch name (hermes create --branch; requires a
    /// worktree workspace).
    #[serde(default)]
    pub branch: Option<String>,
    /// Creator session id for wake routing (hermes create_task
    /// session_id); the notifier wakes this session on terminal events.
    #[serde(default)]
    pub session_id: Option<String>,
}

/// `POST /api/kanban/tasks` — create a task on the current board.
pub async fn create_task(Json(body): Json<CreateTaskBody>) -> Response {
    if body.title.trim().is_empty() {
        return super::bad_request("title is required", None);
    }
    if let Some(max_retries) = body.max_retries {
        if max_retries < 1 {
            return super::bad_request(
                &format!(
                    "max_retries must be >= 1 (got {max_retries}); use 1 to trip on the first failure"
                ),
                None,
            );
        }
    }
    if let Some(turns) = body.goal_max_turns {
        if turns < 1 {
            return super::bad_request(&format!("goal_max_turns must be >= 1 (got {turns})"), None);
        }
    }
    if body
        .provider
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_some()
        && body
            .model
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .is_none()
    {
        return super::bad_request("provider requires model", None);
    }
    if let Some(status) = body
        .initial_status
        .as_deref()
        .map(str::trim)
        .filter(|status| !status.is_empty())
    {
        if !crate::kanban::VALID_INITIAL_STATUSES.contains(&status) {
            return super::bad_request(
                &format!("initial_status must be one of running, blocked (got '{status}')"),
                None,
            );
        }
    }
    // Workspace flags (hermes _cmd_create): parse + validate, then
    // fall back to the [kanban] worktrees default when unset.
    let (mut workspace_kind, workspace_path) = match body.workspace.as_deref() {
        Some(raw) => match crate::kanban::parse_workspace_flag(raw) {
            Ok(parsed) => parsed,
            Err(e) => return super::bad_request(&e, None),
        },
        None => ("scratch".to_string(), None),
    };
    let branch_name = match body.branch.as_deref() {
        Some(raw) => match crate::kanban::parse_branch_flag(raw) {
            Ok(branch) => Some(branch),
            Err(e) => return super::bad_request(&e, None),
        },
        None => None,
    };
    if branch_name.is_some() && workspace_kind != "worktree" {
        return super::bad_request("--branch is only valid with --workspace worktree", None);
    }
    if body.workspace.is_none()
        && crate::config::UlncLawConfig::load(None)
            .map(|c| c.kanban.worktrees)
            .unwrap_or(false)
    {
        workspace_kind = "worktree".to_string();
    }
    let store = match store() {
        Ok(s) => s,
        Err(e) => return e,
    };
    let task = match store.create_task(&NewTask {
        title: body.title.trim().to_string(),
        body: body.body.unwrap_or_default(),
        assignee: body.assignee.filter(|a| !a.trim().is_empty()),
        priority: body.priority.unwrap_or(0),
        tenant: None,
        model: body
            .model
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        provider: body
            .provider
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        reasoning_effort: body
            .reasoning_effort
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        project_id: body
            .project
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        created_by: "gateway".to_string(),
        skills: body.skills.filter(|s| !s.is_empty()),
        max_runtime_seconds: body.max_runtime_seconds,
        idempotency_key: body.idempotency_key.filter(|k| !k.trim().is_empty()),
        triage: body.triage.unwrap_or(false),
        max_retries: body.max_retries,
        initial_status: body.initial_status,
        workflow_template_id: body.workflow_template_id.filter(|s| !s.trim().is_empty()),
        current_step_key: body.current_step_key.filter(|s| !s.trim().is_empty()),
        goal_mode: body.goal_mode.unwrap_or(false),
        goal_max_turns: body.goal_max_turns,
        workspace_kind: Some(workspace_kind),
        workspace_path,
        branch_name,
        session_id: body.session_id.filter(|id| !id.trim().is_empty()),
    }) {
        Ok(t) => t,
        Err(e) => return super::bad_request(&e.to_string(), None),
    };
    for parent in body.parents.unwrap_or_default() {
        if let Ok(Some(parent_id)) = store.resolve_task_id(&parent) {
            store.link_tasks(&parent_id, &task.id).ok();
        }
    }
    Json(json!({
        "object": "ulnclaw.kanban.task",
        "task": task_json(&store, &task),
    }))
    .into_response()
}

/// `GET /api/kanban/tasks/:id` — task with comments + attachments.
pub async fn get_task(Path(id): Path<String>) -> Response {
    let store = match store() {
        Ok(s) => s,
        Err(e) => return e,
    };
    let id = match resolve(&store, &id) {
        Ok(id) => id,
        Err(e) => return e,
    };
    let task = match store.get_task(&id) {
        Ok(Some(t)) => t,
        Ok(None) => return super::not_found(&format!("task {id} not found")),
        Err(e) => return super::server_error(&e.to_string()),
    };
    let comments: Vec<Value> = store
        .comments(&id)
        .unwrap_or_default()
        .iter()
        .map(|c| {
            json!({
                "id": c.id,
                "author": c.author,
                "body": c.body,
                "created_at": c.created_at,
            })
        })
        .collect();
    let attachments: Vec<Value> = store
        .attachments_with_ids(&id)
        .unwrap_or_default()
        .iter()
        .map(|(aid, kind, value)| json!({"id": aid, "kind": kind, "value": value}))
        .collect();
    let events: Vec<Value> = store
        .events(&id)
        .unwrap_or_default()
        .iter()
        .map(|e| {
            json!({
                "kind": e.kind,
                "payload": e.payload,
                "created_at": e.created_at,
            })
        })
        .collect();
    Json(json!({
        "object": "ulnclaw.kanban.task",
        "task": task_json(&store, &task),
        "comments": comments,
        "attachments": attachments,
        "events": events,
    }))
    .into_response()
}

#[derive(Deserialize, Default)]
pub struct CompleteBody {
    #[serde(default)]
    pub result: Option<String>,
    /// Structured handoff summary for downstream tasks (hermes
    /// complete summary=).
    #[serde(default)]
    pub summary: Option<String>,
    /// JSON object of structured facts stored on the closing run
    /// (hermes complete metadata=).
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
    /// Deliverable file paths; scratch-workspace files are staged into
    /// the attachments dir (hermes kanban_complete artifacts=).
    #[serde(default)]
    pub artifacts: Option<Vec<String>>,
    /// Task ids this worker claims to have created; verified before
    /// completion, phantom ids block it (hermes created_cards).
    #[serde(default)]
    pub created_cards: Option<Vec<String>>,
    /// Run id the caller was spawned under; completions from a
    /// reclaimed attempt are refused (hermes expected_run_id).
    #[serde(default)]
    pub expected_run_id: Option<i64>,
}

/// `POST /api/kanban/tasks/:id/complete` — mark done.
pub async fn complete_task(Path(id): Path<String>, Json(body): Json<CompleteBody>) -> Response {
    let store = match store() {
        Ok(s) => s,
        Err(e) => return e,
    };
    let id = match resolve(&store, &id) {
        Ok(id) => id,
        Err(e) => return e,
    };
    let metadata = match body.metadata.as_ref() {
        Some(value) if !value.is_object() => {
            return super::bad_request("metadata must be a JSON object", None)
        }
        other => other,
    };
    let artifacts: Vec<String> = body
        .artifacts
        .unwrap_or_default()
        .into_iter()
        .filter_map(|raw| {
            let trimmed = raw.trim().to_string();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        })
        .collect();
    let created_cards: Vec<String> = body
        .created_cards
        .unwrap_or_default()
        .into_iter()
        .filter_map(|raw| {
            let trimmed = raw.trim().to_string();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        })
        .collect();
    let home = crate::config::ulnclaw_home();
    match store.complete_task_with_artifacts(
        &home,
        &id,
        body.result.as_deref().filter(|s| !s.trim().is_empty()),
        body.summary
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty()),
        metadata,
        &artifacts,
        &created_cards,
        body.expected_run_id,
    ) {
        Ok(task) => {
            Json(json!({"object": "ulnclaw.kanban.task", "task": task_json(&store, &task)}))
                .into_response()
        }
        Err(e) => super::bad_request(&e.to_string(), None),
    }
}

#[derive(Deserialize)]
pub struct BlockBody {
    pub reason: String,
    /// Typed block kind (hermes block --kind): dependency |
    /// needs_input | capability | transient.
    #[serde(default)]
    pub kind: Option<String>,
    /// Run id the caller was spawned under (hermes expected_run_id).
    #[serde(default)]
    pub expected_run_id: Option<i64>,
}

/// `POST /api/kanban/tasks/:id/block` — mark blocked (reason required).
pub async fn block_task(Path(id): Path<String>, Json(body): Json<BlockBody>) -> Response {
    if body.reason.trim().is_empty() {
        return super::bad_request("reason is required", None);
    }
    let store = match store() {
        Ok(s) => s,
        Err(e) => return e,
    };
    let id = match resolve(&store, &id) {
        Ok(id) => id,
        Err(e) => return e,
    };
    match store.block_task_guarded(
        &id,
        body.reason.trim(),
        body.kind
            .as_deref()
            .map(str::trim)
            .filter(|kind| !kind.is_empty()),
        body.expected_run_id,
    ) {
        Ok(task) => {
            Json(json!({"object": "ulnclaw.kanban.task", "task": task_json(&store, &task)}))
                .into_response()
        }
        Err(e) => super::bad_request(&e.to_string(), None),
    }
}

/// `POST /api/kanban/tasks/:id/unblock` — back to todo.
pub async fn unblock_task(Path(id): Path<String>) -> Response {
    let store = match store() {
        Ok(s) => s,
        Err(e) => return e,
    };
    let id = match resolve(&store, &id) {
        Ok(id) => id,
        Err(e) => return e,
    };
    match store.unblock_task(&id) {
        Ok(task) => {
            Json(json!({"object": "ulnclaw.kanban.task", "task": task_json(&store, &task)}))
                .into_response()
        }
        Err(e) => super::bad_request(&e.to_string(), None),
    }
}

#[derive(Deserialize, Default)]
pub struct ScheduleBody {
    /// Why the task is parked in the scheduled column; doubles as a
    /// comment (hermes kanban schedule).
    #[serde(default)]
    pub reason: Option<String>,
}

/// `POST /api/kanban/tasks/:id/schedule` — park a task in the
/// scheduled column: waiting on time, not human input (hermes
/// `kanban schedule`).
pub async fn schedule_task(Path(id): Path<String>, Json(body): Json<ScheduleBody>) -> Response {
    let store = match store() {
        Ok(s) => s,
        Err(e) => return e,
    };
    let id = match resolve(&store, &id) {
        Ok(id) => id,
        Err(e) => return e,
    };
    match store.schedule_task(&id, body.reason.as_deref().unwrap_or("")) {
        Ok(task) => Json(json!({
            "object": "ulnclaw.kanban.task",
            "task": task_json(&store, &task),
        }))
        .into_response(),
        Err(e) => super::bad_request(&e.to_string(), None),
    }
}

#[derive(Deserialize)]
pub struct ReassignBody {
    /// New assignee; "none" or empty clears the assignment.
    pub assignee: String,
    /// Reclaim a running task before reassigning (hermes
    /// kanban reassign --reclaim).
    #[serde(default)]
    pub reclaim_first: Option<bool>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// `POST /api/kanban/tasks/:id/reassign` — move a task to a different
/// assignee (hermes `kanban reassign`); "none" unassigns.
pub async fn reassign_task(Path(id): Path<String>, Json(body): Json<ReassignBody>) -> Response {
    let store = match store() {
        Ok(s) => s,
        Err(e) => return e,
    };
    let id = match resolve(&store, &id) {
        Ok(id) => id,
        Err(e) => return e,
    };
    match store.reassign_task(
        &id,
        &body.assignee,
        body.reclaim_first.unwrap_or(false),
        body.reason.as_deref().unwrap_or(""),
    ) {
        Ok(task) => Json(json!({
            "object": "ulnclaw.kanban.task",
            "task": task_json(&store, &task),
        }))
        .into_response(),
        Err(e) => super::bad_request(&e.to_string(), None),
    }
}

#[derive(Deserialize, Default)]
pub struct PromoteBody {
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub force: Option<bool>,
}

/// `POST /api/kanban/tasks/:id/promote` — move a todo/blocked task
/// straight to ready (hermes `kanban promote`). `force` skips the
/// dependency check.
pub async fn promote_task(Path(id): Path<String>, Json(body): Json<PromoteBody>) -> Response {
    let store = match store() {
        Ok(s) => s,
        Err(e) => return e,
    };
    let id = match resolve(&store, &id) {
        Ok(id) => id,
        Err(e) => return e,
    };
    match store.promote_task(
        &id,
        body.reason.as_deref().unwrap_or(""),
        body.force.unwrap_or(false),
    ) {
        Ok(task) => Json(json!({
            "object": "ulnclaw.kanban.task",
            "task": task_json(&store, &task),
        }))
        .into_response(),
        Err(e) => super::bad_request(&e.to_string(), None),
    }
}

#[derive(Deserialize, Default)]
pub struct ReclaimBody {
    #[serde(default)]
    pub reason: Option<String>,
}

/// `POST /api/kanban/tasks/:id/reclaim` — release an active worker
/// claim on a running task without completing it; back to ready for a
/// fresh worker (hermes `kanban reclaim`).
pub async fn reclaim_task(Path(id): Path<String>, Json(body): Json<ReclaimBody>) -> Response {
    let store = match store() {
        Ok(s) => s,
        Err(e) => return e,
    };
    let id = match resolve(&store, &id) {
        Ok(id) => id,
        Err(e) => return e,
    };
    match store.reclaim_task(&id, body.reason.as_deref().unwrap_or("")) {
        Ok(task) => Json(json!({
            "object": "ulnclaw.kanban.task",
            "task": task_json(&store, &task),
        }))
        .into_response(),
        Err(e) => super::bad_request(&e.to_string(), None),
    }
}

#[derive(Deserialize)]
pub struct AssignBody {
    pub assignee: String,
}

/// `POST /api/kanban/tasks/:id/assign` — set a task's assignee
/// without reclaiming an active claim (hermes `kanban assign`).
pub async fn assign_task(Path(id): Path<String>, Json(body): Json<AssignBody>) -> Response {
    let store = match store() {
        Ok(s) => s,
        Err(e) => return e,
    };
    let id = match resolve(&store, &id) {
        Ok(id) => id,
        Err(e) => return e,
    };
    match store.assign_task(&id, &body.assignee) {
        Ok(task) => Json(json!({
            "object": "ulnclaw.kanban.task",
            "task": task_json(&store, &task),
        }))
        .into_response(),
        Err(e) => super::bad_request(&e.to_string(), None),
    }
}

#[derive(Deserialize, Default)]
pub struct TaskRunsQuery {
    #[serde(default)]
    pub include_active: Option<bool>,
    #[serde(default)]
    pub state_type: Option<String>,
    #[serde(default)]
    pub state_name: Option<String>,
}

/// `GET /api/kanban/tasks/:id/runs` — run history for a task (hermes
/// `kanban runs`): newest first, optionally filtered by status/outcome.
pub async fn task_runs(Path(id): Path<String>, Query(query): Query<TaskRunsQuery>) -> Response {
    let store = match store() {
        Ok(s) => s,
        Err(e) => return e,
    };
    let id = match resolve(&store, &id) {
        Ok(id) => id,
        Err(e) => return e,
    };
    match store.list_runs(
        &id,
        query.include_active.unwrap_or(true),
        query.state_type.as_deref().filter(|v| !v.is_empty()),
        query.state_name.as_deref().filter(|v| !v.is_empty()),
    ) {
        Ok(runs) => Json(json!({
            "object": "ulnclaw.kanban.task.run.list",
            "task_id": id,
            "runs": runs,
        }))
        .into_response(),
        Err(e) => super::bad_request(&e.to_string(), None),
    }
}

/// `GET /api/kanban/stats` — per-status + per-assignee counts plus
/// oldest-ready age of the current board (hermes `kanban stats`).
pub async fn board_stats() -> Response {
    let store = match store() {
        Ok(s) => s,
        Err(e) => return e,
    };
    match store.board_stats() {
        Ok(stats) => Json(json!({
            "object": "ulnclaw.kanban.board.stats",
            "by_status": stats.by_status.iter().map(|(status, count)| {
                json!({ "status": status, "count": count })
            }).collect::<Vec<_>>(),
            "by_assignee": stats.by_assignee.iter().map(|(assignee, rows)| {
                json!({
                    "assignee": assignee,
                    "statuses": rows.iter().map(|(status, count)| {
                        json!({ "status": status, "count": count })
                    }).collect::<Vec<_>>(),
                })
            }).collect::<Vec<_>>(),
            "oldest_ready_age_seconds": stats.oldest_ready_age_seconds,
            "now": stats.now,
        }))
        .into_response(),
        Err(e) => super::server_error(&e.to_string()),
    }
}

#[derive(Deserialize)]
pub struct AttachBody {
    /// Attachment kind; defaults to "file" (hermes kanban attach).
    #[serde(default = "default_attach_kind")]
    pub kind: String,
    /// Attachment value: an existing file path for kind "file", any
    /// reference string otherwise (link, pr, ...).
    pub value: String,
}

fn default_attach_kind() -> String {
    "file".to_string()
}

/// `POST /api/kanban/tasks/:id/attach` — attach a file (validated to
/// exist, stored absolute) or an arbitrary reference to a task
/// (hermes `kanban attach`).
pub async fn attach_task(Path(id): Path<String>, Json(body): Json<AttachBody>) -> Response {
    let kind = body.kind.trim().to_string();
    let value = body.value.trim().to_string();
    if kind.is_empty() || value.is_empty() {
        return super::bad_request("kind and value are required", None);
    }
    if kind == "file" {
        let path = std::path::Path::new(&value);
        if !path.is_file() {
            return super::bad_request(&format!("{value} is not a file"), None);
        }
    }
    let store = match store() {
        Ok(s) => s,
        Err(e) => return e,
    };
    let id = match resolve(&store, &id) {
        Ok(id) => id,
        Err(e) => return e,
    };
    let stored = if kind == "file" {
        std::fs::canonicalize(&value)
            .map(|p| p.display().to_string())
            .unwrap_or(value.clone())
    } else {
        value.clone()
    };
    match store.attach(&id, &kind, &stored) {
        Ok(()) => Json(json!({ "ok": true, "kind": kind, "value": stored })).into_response(),
        Err(e) => super::bad_request(&e.to_string(), None),
    }
}

/// `DELETE /api/kanban/attachments/:aid` — delete an attachment by id
/// (hermes `kanban attach-rm`).
pub async fn remove_attachment(Path(aid): Path<i64>) -> Response {
    let store = match store() {
        Ok(s) => s,
        Err(e) => return e,
    };
    match store.remove_attachment(aid) {
        Ok(true) => Json(json!({ "ok": true })).into_response(),
        Ok(false) => super::not_found(&format!("attachment {aid} not found")),
        Err(e) => super::server_error(&e.to_string()),
    }
}

/// `POST /api/kanban/tasks/:id/archive` — park a task out of the
/// active board (hermes archive). In-flight runs close as reclaimed
/// and archived parents immediately unblock their children.
pub async fn archive_task(Path(id): Path<String>) -> Response {
    let store = match store() {
        Ok(s) => s,
        Err(e) => return e,
    };
    let id = match resolve(&store, &id) {
        Ok(id) => id,
        Err(e) => return e,
    };
    match store.archive_task(&id) {
        Ok(task) => Json(json!({
            "object": "ulnclaw.kanban.task",
            "task": task_json(&store, &task),
        }))
        .into_response(),
        Err(e) => super::bad_request(&e.to_string(), None),
    }
}

/// `DELETE /api/kanban/tasks/:id` — permanently purge an archived
/// task and every related row (hermes `kanban archive --rm`). Safety
/// guard: only archived tasks can be deleted.
pub async fn delete_task(Path(id): Path<String>) -> Response {
    let store = match store() {
        Ok(s) => s,
        Err(e) => return e,
    };
    let id = match resolve(&store, &id) {
        Ok(id) => id,
        Err(e) => return e,
    };
    match store.delete_archived_task(&id) {
        Ok(true) => Json(json!({ "ok": true })).into_response(),
        Ok(false) => super::bad_request("task must be archived before deletion", None),
        Err(e) => super::server_error(&e.to_string()),
    }
}

#[derive(Deserialize, Default)]
pub struct EditTaskBody {
    /// New title; blank is rejected, omitted keeps the current one.
    #[serde(default)]
    pub title: Option<String>,
    /// New body; omitted keeps the current one (empty string clears).
    #[serde(default)]
    pub body: Option<String>,
}

/// `POST /api/kanban/tasks/:id/edit` — rewrite a task's title/body
/// (hermes `kanban edit`). Archived tasks refuse the edit.
pub async fn edit_task(Path(id): Path<String>, Json(body): Json<EditTaskBody>) -> Response {
    let store = match store() {
        Ok(s) => s,
        Err(e) => return e,
    };
    let id = match resolve(&store, &id) {
        Ok(id) => id,
        Err(e) => return e,
    };
    // Pass the title through raw: the store rejects blank titles with
    // a precise error and trims on write; omitted keeps the current.
    match store.edit_task(&id, body.title.as_deref(), body.body.as_deref()) {
        Ok(task) => Json(json!({
            "object": "ulnclaw.kanban.task",
            "task": task_json(&store, &task),
        }))
        .into_response(),
        Err(e) => super::bad_request(&e.to_string(), None),
    }
}

#[derive(Deserialize, Default)]
pub struct SetModelBody {
    /// Model to pin; null / empty clears the override (and the
    /// provider with it). Takes effect on the next dispatch.
    #[serde(default)]
    pub model: Option<String>,
    /// Optional provider pin; requires a model (hermes contract).
    #[serde(default)]
    pub provider: Option<String>,
}

/// `POST /api/kanban/tasks/:id/set-model` — pin (or clear) the
/// per-task model/provider override (hermes `kanban set-model
/// [--provider P]`). Takes effect on the next dispatch.
pub async fn set_model(Path(id): Path<String>, Json(body): Json<SetModelBody>) -> Response {
    let store = match store() {
        Ok(s) => s,
        Err(e) => return e,
    };
    let id = match resolve(&store, &id) {
        Ok(id) => id,
        Err(e) => return e,
    };
    let model = body
        .model
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let provider = body
        .provider
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    // resolve() already 404'd unknown ids; what remains (terminal
    // task, provider-without-model) is a caller error -> 400.
    match store.set_model(&id, model, provider) {
        Ok(task) => Json(json!({
            "object": "ulnclaw.kanban.task",
            "task": task_json(&store, &task),
        }))
        .into_response(),
        Err(e) => super::bad_request(&e.to_string(), None),
    }
}

#[derive(Deserialize, Default)]
pub struct SetReasoningBody {
    /// Reasoning effort level (none|minimal|low|medium|high|xhigh|
    /// max|ultra); null / empty clears the pin so the worker falls
    /// back to its profile's own `agent.reasoning_effort`.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

/// `POST /api/kanban/tasks/:id/set-reasoning` — pin (or clear) the
/// per-task reasoning effort (hermes `reasoning_effort`). Takes
/// effect on the next dispatch, so it is settable on a running task.
pub async fn set_reasoning(Path(id): Path<String>, Json(body): Json<SetReasoningBody>) -> Response {
    let store = match store() {
        Ok(s) => s,
        Err(e) => return e,
    };
    let id = match resolve(&store, &id) {
        Ok(id) => id,
        Err(e) => return e,
    };
    let effort = body
        .reasoning_effort
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let task = match store.set_reasoning_effort(&id, effort) {
        Ok(t) => t,
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("reasoning_effort must be one of") || msg.contains("archived task") {
                return super::bad_request(&msg, None);
            }
            return super::server_error(&msg);
        }
    };
    Json(json!({
        "object": "ulnclaw.kanban.task",
        "task": task_json(&store, &task),
    }))
    .into_response()
}

#[derive(Deserialize)]
pub struct CommentBody {
    pub body: String,
    #[serde(default)]
    pub author: Option<String>,
}

/// `POST /api/kanban/tasks/:id/comment` — add a comment.
pub async fn comment_task(Path(id): Path<String>, Json(body): Json<CommentBody>) -> Response {
    if body.body.trim().is_empty() {
        return super::bad_request("body is required", None);
    }
    let store = match store() {
        Ok(s) => s,
        Err(e) => return e,
    };
    let id = match resolve(&store, &id) {
        Ok(id) => id,
        Err(e) => return e,
    };
    let author = body
        .author
        .as_deref()
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .unwrap_or("desktop");
    match store.add_comment(&id, author, body.body.trim()) {
        Ok(()) => Json(json!({"ok": true, "task_id": id})).into_response(),
        Err(e) => super::bad_request(&e.to_string(), None),
    }
}

#[derive(Deserialize)]
pub struct LinkBody {
    pub parent_id: String,
}

/// `POST /api/kanban/tasks/:id/link` — link child → parent.
pub async fn link_task(Path(id): Path<String>, Json(body): Json<LinkBody>) -> Response {
    let store = match store() {
        Ok(s) => s,
        Err(e) => return e,
    };
    let child = match resolve(&store, &id) {
        Ok(id) => id,
        Err(e) => return e,
    };
    let parent = match resolve(&store, &body.parent_id) {
        Ok(id) => id,
        Err(e) => return e,
    };
    match store.link_tasks(&parent, &child) {
        Ok(()) => Json(json!({"ok": true, "parent_id": parent, "child_id": child})).into_response(),
        Err(e) => super::bad_request(&e.to_string(), None),
    }
}

/// `POST /api/kanban/tasks/:id/unlink` — remove a parent link from
/// this (child) task (hermes unlink; idempotent store-side).
pub async fn unlink_task(Path(id): Path<String>, Json(body): Json<LinkBody>) -> Response {
    let store = match store() {
        Ok(s) => s,
        Err(e) => return e,
    };
    let child = match resolve(&store, &id) {
        Ok(id) => id,
        Err(e) => return e,
    };
    let parent = match resolve(&store, &body.parent_id) {
        Ok(id) => id,
        Err(e) => return e,
    };
    match store.unlink_tasks(&parent, &child) {
        Ok(()) => Json(json!({"ok": true, "parent_id": parent, "child_id": child})).into_response(),
        Err(e) => super::bad_request(&e.to_string(), None),
    }
}

#[derive(Deserialize, Default)]
pub struct DispatchBody {
    #[serde(default)]
    pub max_spawn: Option<usize>,
    #[serde(default)]
    pub dry_run: Option<bool>,
}

/// `POST /api/kanban/dispatch` — one dispatcher tick (hermes
/// `kanban dispatch`): reclaim stale claims, promote parent-done todos,
/// spawn detached `ulnclaw run` workers for ready tasks.
pub async fn dispatch(Json(body): Json<DispatchBody>) -> Response {
    let store = match store() {
        Ok(s) => s,
        Err(e) => return e,
    };
    let home = crate::config::ulnclaw_home();
    let config = crate::config::UlncLawConfig::load(None).unwrap_or_default();
    let max_spawn = body.max_spawn.unwrap_or(config.kanban.max_spawn).max(1);
    let dry_run = body.dry_run.unwrap_or(false);
    let use_worktrees = config.kanban.worktrees;
    let stale_timeout = config.kanban.stale_timeout_seconds;
    let known_profiles: std::collections::HashSet<String> =
        config.profiles.keys().cloned().collect();
    // Spawning child processes is blocking work — keep it off the axum task.
    let outcome = tokio::task::spawn_blocking(move || {
        store.dispatch_once(
            &home,
            use_worktrees,
            |task, workspace| crate::kanban::dispatch_spawn(&home, task, workspace),
            Some(max_spawn),
            dry_run,
            2,
            stale_timeout,
            Some(&known_profiles),
            config.kanban.max_in_progress_per_profile,
            config.kanban.max_in_progress,
        )
    })
    .await;
    match outcome {
        Ok(Ok(result)) => Json(json!({
            "object": "ulnclaw.kanban.dispatch",
            "dry_run": dry_run,
            "skipped_locked": result.skipped_locked,
            "reclaimed": result.reclaimed,
            "promoted": result.promoted,
            "spawned": result.spawned,
            "would_spawn": result.would_spawn,
            "respawn_guarded": result.respawn_guarded,
            "skipped_capped": result.skipped_capped,
            "skipped_unassigned": result.skipped_unassigned,
            "skipped_nonspawnable": result.skipped_nonspawnable,
            "skipped_per_profile_capped": result.skipped_per_profile_capped,
            "spawn_failed": result.spawn_failed,
            "auto_blocked": result.auto_blocked,
        }))
        .into_response(),
        Ok(Err(e)) => super::server_error(&e.to_string()),
        Err(e) => super::server_error(&e.to_string()),
    }
}

/// `POST /api/kanban/tasks/:id/claim` — claim a task for work (desktop
/// "start" button); uses the same TTL semantics as the CLI workers.
pub async fn claim_task(Path(id): Path<String>) -> Response {
    let store = match store() {
        Ok(s) => s,
        Err(e) => return e,
    };
    let id = match resolve(&store, &id) {
        Ok(id) => id,
        Err(e) => return e,
    };
    let claimer = KanbanStore::claimer_id();
    match store.claim_task(&id, &claimer, DEFAULT_CLAIM_TTL_SECS) {
        Ok(task) => {
            Json(json!({"object": "ulnclaw.kanban.task", "task": task_json(&store, &task)}))
                .into_response()
        }
        Err(e) => super::bad_request(&e.to_string(), None),
    }
}

/// `GET /api/kanban/tasks/:id/diagnostics` — structured distress
/// signals for one task (hermes `kanban diagnostics <id>` parity,
/// P678).
pub async fn task_diagnostics(Path(id): Path<String>) -> Response {
    let store = match store() {
        Ok(s) => s,
        Err(e) => return e,
    };
    let id = match resolve(&store, &id) {
        Ok(id) => id,
        Err(e) => return e,
    };
    let task = match store.get_task(&id) {
        Ok(Some(task)) => task,
        Ok(None) => return super::not_found(&format!("task {id} not found")),
        Err(e) => return super::server_error(&e.to_string()),
    };
    let config = crate::config::UlncLawConfig::load(None).unwrap_or_default();
    let diagnostics = crate::kanban_diagnostics::compute_task_diagnostics(&store, &config, &task);
    Json(json!({
        "object": "ulnclaw.kanban.task.diagnostics",
        "task_id": id,
        "diagnostics": diagnostics,
    }))
    .into_response()
}

/// `GET /api/kanban/diagnostics` — board-wide distress-signal scan:
/// every open task with at least one active diagnostic (hermes
/// board-load diagnostics, P678).
pub async fn board_diagnostics() -> Response {
    let store = match store() {
        Ok(s) => s,
        Err(e) => return e,
    };
    let tasks = match store.list_tasks(None, None, None, None, 500) {
        Ok(tasks) => tasks,
        Err(e) => return super::server_error(&e.to_string()),
    };
    let config = crate::config::UlncLawConfig::load(None).unwrap_or_default();
    let mut rows: Vec<Value> = Vec::new();
    for task in tasks {
        if task.status == "done" || task.status == "archived" {
            continue;
        }
        let diagnostics =
            crate::kanban_diagnostics::compute_task_diagnostics(&store, &config, &task);
        if diagnostics.is_empty() {
            continue;
        }
        rows.push(json!({
            "id": task.id,
            "title": task.title,
            "status": task.status,
            "diagnostics": diagnostics,
        }));
    }
    Json(json!({
        "object": "ulnclaw.kanban.board.diagnostics",
        "flagged": rows.len(),
        "tasks": rows,
    }))
    .into_response()
}

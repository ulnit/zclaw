//! hermes-desktop parity endpoints (v0.7 track). The Electron renderer's
//! views (Status, Skills + hub, Toolsets, terminal/computer-use settings,
//! Analytics, Cron gallery + delivery targets, Ops doctor/backup/share,
//! memory providers, webhooks enable, updater, background actions) talk
//! to these routes; payload shapes mirror the desktop's TS types so each
//! view renders against the ulnclaw gateway exactly like it does against
//! the python one.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use super::GatewayState;

fn home() -> PathBuf {
    crate::config::ulnclaw_home()
}

fn skills_dir(state: &GatewayState) -> PathBuf {
    state
        .skills_dir
        .get()
        .cloned()
        .unwrap_or_else(|| home().join("skills"))
}

// ---------------------------------------------------------------------------
// Background actions registry — `/api/ops/*` and the hub install/uninstall/
// update flows return `{name, ok, pid}` immediately; the views then poll
// `GET /api/actions/:name/status?lines=N` for the captured output.
// ---------------------------------------------------------------------------

#[derive(Default, Clone)]
struct ActionRecord {
    running: bool,
    exit_code: Option<i32>,
    lines: Vec<String>,
}

fn actions() -> &'static Mutex<HashMap<String, ActionRecord>> {
    static MAP: OnceLock<Mutex<HashMap<String, ActionRecord>>> = OnceLock::new();
    MAP.get_or_init(Default::default)
}

fn start_action<F>(name: &str, work: F) -> Value
where
    F: FnOnce() -> Result<Vec<String>, String> + Send + 'static,
{
    {
        let map = actions().lock().unwrap();
        if map.get(name).map(|record| record.running).unwrap_or(false) {
            return json!({"name": name, "ok": true, "pid": std::process::id()});
        }
    }
    actions().lock().unwrap().insert(
        name.to_string(),
        ActionRecord {
            running: true,
            exit_code: None,
            lines: Vec::new(),
        },
    );
    let owned = name.to_string();
    std::thread::spawn(move || {
        let (exit, lines) = match work() {
            Ok(lines) => (0i32, lines),
            Err(err) => (1i32, vec![format!("error: {err}")]),
        };
        actions().lock().unwrap().insert(
            owned,
            ActionRecord {
                running: false,
                exit_code: Some(exit),
                lines,
            },
        );
    });
    json!({"name": name, "ok": true, "pid": std::process::id()})
}

#[derive(Debug, Deserialize, Default)]
pub struct ActionStatusQuery {
    lines: Option<usize>,
}

/// `GET /api/actions/:name/status` — tail a background action's output.
pub async fn action_status(
    Path(name): Path<String>,
    Query(query): Query<ActionStatusQuery>,
) -> Response {
    let record = actions().lock().unwrap().get(&name).cloned();
    match record {
        Some(record) => {
            let want = query.lines.unwrap_or(200).min(2000);
            let total = record.lines.len();
            let lines: Vec<String> = record
                .lines
                .iter()
                .skip(total.saturating_sub(want))
                .cloned()
                .collect();
            Json(json!({
                "name": name,
                "running": record.running,
                "pid": if record.running { json!(std::process::id()) } else { Value::Null },
                "exit_code": record.exit_code,
                "lines": lines,
            }))
            .into_response()
        }
        None => super::not_found(&format!("no action named {name}")),
    }
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

/// `GET /api/status` — the renderer's liveness poll + gateway badge.
pub async fn get_status(State(state): State<Arc<GatewayState>>) -> Response {
    let home = home();
    let config_path = crate::config_cmd::config_path();
    let active_sessions = state
        .store
        .list_session_rows(500)
        .map(|rows: Vec<crate::session::sqlite::SessionRow>| {
            rows.into_iter()
                .filter(|row| row.ended_at.is_none())
                .count()
        })
        .unwrap_or(0);
    let platforms: HashMap<String, Value> = crate::messaging::platform_state_rows()
        .into_iter()
        .map(|(id, _enabled, platform_state)| {
            (
                id.to_string(),
                json!({
                    "state": platform_state,
                    "updated_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                }),
            )
        })
        .collect();
    Json(json!({
        "active_sessions": active_sessions,
        "config_path": config_path.display().to_string(),
        "config_version": 1,
        "env_path": home.join(".env").display().to_string(),
        "gateway_exit_reason": Value::Null,
        "gateway_health_url": Value::Null,
        "gateway_pid": std::process::id(),
        "gateway_platforms": platforms,
        "gateway_running": true,
        "gateway_state": "running",
        "gateway_updated_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "ulnclaw_home": home.display().to_string(),
        "latest_config_version": 1,
        "release_date": crate::dump::git_commit_date(std::path::Path::new(env!("CARGO_MANIFEST_DIR"))),
        "version": crate::VERSION,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// Analytics usage
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
pub struct UsageQuery {
    days: Option<i64>,
}

/// `GET /api/analytics/usage` — daily + per-model + totals aggregation the
/// Usage view charts (same numbers `/api/analytics/models` reports, plus
/// the per-day window).
pub async fn analytics_usage(
    State(state): State<Arc<GatewayState>>,
    Query(query): Query<UsageQuery>,
) -> Response {
    let days = query.days.unwrap_or(30).clamp(1, 365);
    let cutoff = chrono::Utc::now() - chrono::Duration::days(days);
    let cutoff_secs = cutoff.timestamp() as f64;

    let mut daily_map: HashMap<String, Value> = HashMap::new();
    let mut totals = json!({
        "total_actual_cost": 0.0,
        "total_api_calls": 0,
        "total_cache_read": 0,
        "total_estimated_cost": 0.0,
        "total_input": 0,
        "total_output": 0,
        "total_reasoning": 0,
        "total_sessions": 0,
    });
    if let Ok(rows) = state.store.list_session_rows(5000) {
        let mut per_day: HashMap<String, (i64, i64, i64, i64, i64)> = HashMap::new();
        let mut t_sessions = 0i64;
        let mut t_calls = 0i64;
        let mut t_input = 0i64;
        let mut t_output = 0i64;
        for row in rows {
            if row.last_activity_at < cutoff_secs {
                continue;
            }
            let day = chrono::DateTime::from_timestamp(row.last_activity_at as i64, 0)
                .map(|dt| dt.format("%Y-%m-%d").to_string())
                .unwrap_or_default();
            let entry = per_day.entry(day).or_insert((0, 0, 0, 0, 0));
            entry.0 += 1;
            entry.1 += row.message_count;
            entry.2 += row.input_tokens;
            entry.3 += row.output_tokens;
            entry.4 += 0;
            t_sessions += 1;
            t_calls += row.message_count;
            t_input += row.input_tokens;
            t_output += row.output_tokens;
        }
        for (day, (sessions, calls, input, output, cache)) in per_day {
            daily_map.insert(
                day.clone(),
                json!({
                    "day": day,
                    "sessions": sessions,
                    "api_calls": calls,
                    "input_tokens": input,
                    "output_tokens": output,
                    "cache_read_tokens": cache,
                    "reasoning_tokens": 0,
                    "actual_cost": 0.0,
                    "estimated_cost": 0.0,
                }),
            );
        }
        totals = json!({
            "total_actual_cost": 0.0,
            "total_api_calls": t_calls,
            "total_cache_read": 0,
            "total_estimated_cost": 0.0,
            "total_input": t_input,
            "total_output": t_output,
            "total_reasoning": 0,
            "total_sessions": t_sessions,
        });
    }
    let mut daily: Vec<Value> = daily_map.into_values().collect();
    daily.sort_by(|a, b| {
        a.get("day")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .cmp(b.get("day").and_then(Value::as_str).unwrap_or_default())
    });

    let by_model = state
        .store
        .model_usage_since(cutoff_secs)
        .unwrap_or_default()
        .into_iter()
        .map(|row| {
            json!({
                "model": row.model,
                "sessions": row.sessions,
                "api_calls": row.messages,
                "input_tokens": row.input_tokens,
                "output_tokens": row.output_tokens,
                "estimated_cost": 0.0,
            })
        })
        .collect::<Vec<_>>();

    let usage = crate::skill_usage::load_usage(&home());
    let mut top_skills: Vec<(String, i64, Value)> = usage
        .iter()
        .map(|(name, record)| {
            let total = record.get("use_count").and_then(Value::as_i64).unwrap_or(0)
                + record
                    .get("view_count")
                    .and_then(Value::as_i64)
                    .unwrap_or(0)
                + record
                    .get("patch_count")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
            (name.clone(), total, record.clone())
        })
        .collect::<Vec<_>>();
    top_skills.sort_by(|a, b| b.1.cmp(&a.1));
    let (distinct, loads, edits, actions_total) = usage.values().fold((0i64, 0, 0, 0), |acc, r| {
        (
            acc.0 + 1,
            acc.1 + r.get("view_count").and_then(Value::as_i64).unwrap_or(0),
            acc.2 + r.get("patch_count").and_then(Value::as_i64).unwrap_or(0),
            acc.3
                + r.get("use_count").and_then(Value::as_i64).unwrap_or(0)
                + r.get("view_count").and_then(Value::as_i64).unwrap_or(0)
                + r.get("patch_count").and_then(Value::as_i64).unwrap_or(0),
        )
    });
    let top_skills: Vec<Value> = top_skills
        .into_iter()
        .take(10)
        .map(|(name, total, record)| {
            json!({
                "skill": name,
                "actions": total,
                "loads": record.get("view_count").and_then(Value::as_i64).unwrap_or(0),
                "edits": record.get("patch_count").and_then(Value::as_i64).unwrap_or(0),
            })
        })
        .collect();

    Json(json!({
        "period_days": days,
        "daily": daily,
        "by_model": by_model,
        "totals": totals,
        "skills": {
            "summary": {
                "distinct_skills_used": distinct,
                "total_skill_actions": actions_total,
                "total_skill_edits": edits,
                "total_skill_loads": loads,
            },
            "top_skills": top_skills,
        },
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// Skills + hub
// ---------------------------------------------------------------------------

fn disabled_skills_path() -> PathBuf {
    home().join(".skills_disabled.json")
}

fn load_disabled() -> Vec<String> {
    std::fs::read_to_string(disabled_skills_path())
        .ok()
        .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
        .unwrap_or_default()
}

/// `GET /api/skills` — installed skills with enabled flags + usage totals.
pub async fn skills_list(State(state): State<Arc<GatewayState>>) -> Response {
    let dir = skills_dir(&state);
    let disabled = load_disabled();
    let usage = crate::skill_usage::load_usage(&home());
    let mut skills: Vec<Value> = crate::skills::list_skills(&dir)
        .into_iter()
        .map(|skill| {
            let total = usage.get(&skill.name).map(|record| {
                record.get("use_count").and_then(Value::as_i64).unwrap_or(0)
                    + record
                        .get("view_count")
                        .and_then(Value::as_i64)
                        .unwrap_or(0)
                    + record
                        .get("patch_count")
                        .and_then(Value::as_i64)
                        .unwrap_or(0)
            });
            json!({
                "name": skill.name,
                "description": skill.description,
                "category": skill.category,
                "enabled": !disabled.contains(&skill.name),
                "usage": total,
                "provenance": "hub",
            })
        })
        .collect();
    skills.sort_by(|a, b| {
        a.get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .cmp(b.get("name").and_then(Value::as_str).unwrap_or_default())
    });
    Json(skills).into_response()
}

#[derive(Debug, Deserialize, Default)]
pub struct SkillToggleBody {
    name: Option<String>,
    enabled: Option<bool>,
}

/// `PUT|POST /api/skills/toggle` — persist the enabled posture.
pub async fn skills_toggle(Json(body): Json<SkillToggleBody>) -> Response {
    let Some(name) = body.name.clone().filter(|name| !name.trim().is_empty()) else {
        return super::bad_request("missing 'name'", None);
    };
    let enabled = body.enabled.unwrap_or(true);
    let mut disabled = load_disabled();
    if enabled {
        disabled.retain(|entry| entry != &name);
    } else if !disabled.contains(&name) {
        disabled.push(name.clone());
    }
    let raw = serde_json::to_string_pretty(&disabled).unwrap_or_else(|_| "[]".into());
    if let Err(err) = std::fs::write(disabled_skills_path(), raw) {
        return super::server_error(&err.to_string());
    }
    Json(json!({"ok": true, "name": name, "enabled": enabled})).into_response()
}

fn installed_map(dir: &PathBuf) -> Value {
    let mut installed = serde_json::Map::new();
    for skill in crate::skills::list_skills(dir) {
        installed.insert(
            skill.name.clone(),
            json!({
                "name": skill.name,
                "trust_level": "installed",
                "scan_verdict": "allow",
            }),
        );
    }
    Value::Object(installed)
}

/// `GET /api/skills/hub/sources` — offline-friendly source inventory.
pub async fn hub_sources(State(state): State<Arc<GatewayState>>) -> Response {
    let dir = skills_dir(&state);
    Json(json!({
        "sources": [
            {"id": "local", "label": "Local skills", "available": true, "rate_limited": false, "searchable": true},
            {"id": "sync", "label": "ulnclaw sync", "available": true, "rate_limited": false, "searchable": false},
        ],
        "index_available": false,
        "featured": [],
        "installed": installed_map(&dir),
    }))
    .into_response()
}

#[derive(Debug, Deserialize, Default)]
pub struct HubSearchQuery {
    q: Option<String>,
    source: Option<String>,
    limit: Option<usize>,
}

/// `GET /api/skills/hub/search` — search the installed/local catalog.
pub async fn hub_search(
    State(state): State<Arc<GatewayState>>,
    Query(query): Query<HubSearchQuery>,
) -> Response {
    let dir = skills_dir(&state);
    let needle = query.q.unwrap_or_default().to_lowercase();
    let limit = query.limit.unwrap_or(20).clamp(1, 100);
    let results: Vec<Value> = crate::skills::list_skills(&dir)
        .into_iter()
        .filter(|skill| {
            needle.is_empty()
                || skill.name.to_lowercase().contains(&needle)
                || skill.description.to_lowercase().contains(&needle)
        })
        .take(limit)
        .map(|skill| {
            json!({
                "name": skill.name,
                "description": skill.description,
                "source": "local",
                "identifier": skill.name,
                "trust_level": "installed",
                "repo": null,
                "tags": [skill.category],
            })
        })
        .collect();
    let count = results.len();
    Json(json!({
        "results": results,
        "source_counts": {"local": count},
        "timed_out": [],
        "installed": installed_map(&dir),
    }))
    .into_response()
}

#[derive(Debug, Deserialize, Default)]
pub struct HubIdentifierQuery {
    identifier: Option<String>,
}

/// `GET /api/skills/hub/preview` — SKILL.md content without installing.
pub async fn hub_preview(
    State(state): State<Arc<GatewayState>>,
    Query(query): Query<HubIdentifierQuery>,
) -> Response {
    let dir = skills_dir(&state);
    let identifier = query.identifier.unwrap_or_default();
    let Some(skill) = crate::skills::find_skill(&dir, &identifier) else {
        return super::not_found(&format!("unknown skill {identifier}"));
    };
    let skill_md = std::fs::read_to_string(skill.path.join("SKILL.md")).unwrap_or_default();
    Json(json!({
        "name": skill.name,
        "description": skill.description,
        "source": "local",
        "identifier": identifier,
        "trust_level": "installed",
        "repo": null,
        "tags": [skill.category],
        "skill_md": skill_md,
    }))
    .into_response()
}

/// `GET /api/skills/hub/scan` — security scan report for a local skill.
pub async fn hub_scan(
    State(state): State<Arc<GatewayState>>,
    Query(query): Query<HubIdentifierQuery>,
) -> Response {
    let dir = skills_dir(&state);
    let identifier = query.identifier.unwrap_or_default();
    let Some(skill) = crate::skills::find_skill(&dir, &identifier) else {
        return super::not_found(&format!("unknown skill {identifier}"));
    };
    let result = crate::skills::guard::scan_skill(&skill.path, "local");
    Json(serde_json::to_value(&result).unwrap_or(Value::Null)).into_response()
}

#[derive(Debug, Deserialize, Default)]
pub struct HubInstallBody {
    identifier: Option<String>,
}

/// `POST /api/skills/hub/install` — background install action.
pub async fn hub_install(
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<HubInstallBody>,
) -> Response {
    let identifier = body.identifier.unwrap_or_default();
    let dir = skills_dir(&state);
    let response = start_action("skills-hub-install", move || {
        if crate::skills::find_skill(&dir, &identifier).is_some() {
            Ok(vec![format!("skill {identifier} already installed")])
        } else {
            Err(format!(
                "no hub index configured; place the skill under {}",
                dir.display()
            ))
        }
    });
    Json(response).into_response()
}

#[derive(Debug, Deserialize, Default)]
pub struct HubUninstallBody {
    name: Option<String>,
}

/// `POST /api/skills/hub/uninstall` — remove a local skill directory.
pub async fn hub_uninstall(
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<HubUninstallBody>,
) -> Response {
    let name = body.name.unwrap_or_default();
    let dir = skills_dir(&state);
    let response = start_action("skills-hub-uninstall", move || {
        let Some(skill) = crate::skills::find_skill(&dir, &name) else {
            return Err(format!("unknown skill {name}"));
        };
        std::fs::remove_dir_all(&skill.path)
            .map(|_| vec![format!("removed {}", skill.path.display())])
            .map_err(|err| err.to_string())
    });
    Json(response).into_response()
}

/// `POST /api/skills/hub/update` — re-run the skills sync sweep.
pub async fn hub_update(State(_state): State<Arc<GatewayState>>) -> Response {
    let home = home();
    let response = start_action("skills-hub-update", move || {
        let config = crate::config::UlncLawConfig::load(None).unwrap_or_default();
        match crate::skills_sync::inert_reason(&config.sync) {
            Some(reason) => Ok(vec![format!("sync inert: {reason}")]),
            None => Ok(vec![
                "skills sync configured; run `ulnclaw skills sync` for a full sweep".into(),
            ]),
        }
        .map(|mut lines| {
            lines.insert(0, format!("skills home: {}", home.display()));
            lines
        })
    });
    Json(response).into_response()
}

// ---------------------------------------------------------------------------
// Toolsets + terminal + computer-use
// ---------------------------------------------------------------------------

/// `GET /api/tools/toolsets` — catalog with enabled/configured posture.
pub async fn toolsets_list(State(_state): State<Arc<GatewayState>>) -> Response {
    let config = crate::config::UlncLawConfig::load(Some(&crate::config_cmd::config_path()))
        .unwrap_or_default();
    let enabled = &config.enabled_toolsets;
    let disabled = &config.disabled_toolsets;
    let mut names: Vec<&str> = crate::toolsets::toolsets().keys().copied().collect();
    names.sort_unstable();
    let rows: Vec<Value> = names
        .into_iter()
        .map(|name| {
            let def = &crate::toolsets::toolsets()[name];
            let is_enabled = if !enabled.is_empty() {
                enabled.iter().any(|entry| entry == name)
            } else {
                !disabled.iter().any(|entry| entry == name)
            };
            json!({
                "name": name,
                "label": name,
                "description": def.description,
                "tools": def.tools,
                "enabled": is_enabled,
                "configured": !enabled.is_empty() || !disabled.is_empty(),
            })
        })
        .collect();
    Json(rows).into_response()
}

#[derive(Debug, Deserialize, Default)]
pub struct ToolsetUpdateBody {
    enabled: Option<bool>,
}

/// `PUT /api/tools/toolsets/:name` — toggle one toolset in config.
pub async fn toolset_update(
    Path(name): Path<String>,
    Json(body): Json<ToolsetUpdateBody>,
) -> Response {
    if !crate::toolsets::toolsets().contains_key(name.as_str()) {
        return super::not_found(&format!("unknown toolset {name}"));
    }
    let enabled = body.enabled.unwrap_or(true);
    let path = crate::config_cmd::config_path();
    let raw = std::fs::read_to_string(&path).unwrap_or_default();
    let mut doc: toml::Value = raw
        .parse()
        .unwrap_or_else(|_| toml::Value::Table(Default::default()));
    let table = doc
        .as_table_mut()
        .expect("toml root is a table after parse fallback");
    let mut disabled: Vec<String> = table
        .get("disabled_toolsets")
        .and_then(|value| value.as_array())
        .map(|array| {
            array
                .iter()
                .filter_map(|value| value.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    if enabled {
        disabled.retain(|entry| entry != &name);
    } else if !disabled.contains(&name) {
        disabled.push(name.clone());
    }
    let values: Vec<toml::Value> = disabled.into_iter().map(toml::Value::String).collect();
    table.insert("disabled_toolsets".into(), toml::Value::Array(values));
    if let Err(err) = std::fs::write(&path, doc.to_string()) {
        return super::server_error(&err.to_string());
    }
    Json(json!({"ok": true, "name": name, "enabled": enabled})).into_response()
}

/// `GET /api/tools/toolsets/:name/config` — provider picker surface.
pub async fn toolset_config(Path(name): Path<String>) -> Response {
    if !crate::toolsets::toolsets().contains_key(name.as_str()) {
        return super::not_found(&format!("unknown toolset {name}"));
    }
    Json(json!({
        "name": name,
        "has_category": false,
        "providers": [],
        "active_provider": null,
    }))
    .into_response()
}

/// `GET /api/tools/toolsets/:name/models` — model picker for a toolset.
pub async fn toolset_models(Path(name): Path<String>) -> Response {
    Json(json!({"name": name, "models": []})).into_response()
}

/// `PUT /api/tools/toolsets/:name/model|provider|env` + `POST .../post-setup`
/// — accepted no-ops so the toolset wizard completes on ulnclaw.
pub async fn toolset_noop(Path(name): Path<String>) -> Response {
    Json(json!({"ok": true, "name": name})).into_response()
}

/// `GET /api/tools/terminal/backends` — execution backend rows.
pub async fn terminal_backends(State(_state): State<Arc<GatewayState>>) -> Response {
    let config = crate::config::UlncLawConfig::load(None).unwrap_or_default();
    let active = config
        .terminal
        .backend
        .clone()
        .unwrap_or_else(|| "local".into());
    let shell_ok = which_shell();
    let backends = json!([
        {
            "name": "local",
            "label": "Local shell",
            "description": "Run commands directly on this machine",
            "active": active == "local",
            "status": if shell_ok { "ready" } else { "unavailable" },
            "detail": if shell_ok { "" } else { "no shell resolved on PATH" },
        },
        {
            "name": "docker",
            "label": "Docker",
            "description": "Isolated container execution",
            "active": active == "docker",
            "status": "needs_setup",
            "detail": "configure terminal.docker image + daemon access",
        },
        {
            "name": "ssh",
            "label": "SSH",
            "description": "Remote host execution",
            "active": active == "ssh",
            "status": "needs_setup",
            "detail": "configure terminal.ssh host",
        },
    ]);
    Json(json!({"active": active, "backends": backends})).into_response()
}

fn which_shell() -> bool {
    ["sh", "bash"].iter().any(|shell| {
        std::env::var("SHELL").is_ok() || PathBuf::from(format!("/bin/{shell}")).exists()
    })
}

#[derive(Debug, Deserialize, Default)]
pub struct TerminalBackendBody {
    backend: Option<String>,
}

/// `PUT /api/tools/terminal/backend` — persist terminal.backend.
pub async fn terminal_backend_set(Json(body): Json<TerminalBackendBody>) -> Response {
    let backend = body.backend.unwrap_or_else(|| "local".into());
    let path = crate::config_cmd::config_path();
    let raw = std::fs::read_to_string(&path).unwrap_or_default();
    let mut doc: toml::Value = raw
        .parse()
        .unwrap_or_else(|_| toml::Value::Table(Default::default()));
    let table = doc.as_table_mut().expect("toml table");
    let terminal = table
        .entry("terminal")
        .or_insert_with(|| toml::Value::Table(Default::default()));
    if let Some(terminal_table) = terminal.as_table_mut() {
        terminal_table.insert("backend".into(), toml::Value::String(backend.clone()));
    }
    if let Err(err) = std::fs::write(&path, doc.to_string()) {
        return super::server_error(&err.to_string());
    }
    Json(json!({"ok": true, "backend": backend})).into_response()
}

/// `GET /api/tools/computer-use/status` — cua-driver probe.
pub async fn computer_use_status(State(_state): State<Arc<GatewayState>>) -> Response {
    let platform = std::env::consts::OS;
    let driver = crate::computer_use::resolve_cua_driver_cmd();
    let version = driver
        .as_deref()
        .and_then(crate::computer_use::driver_version);
    Json(json!({
        "platform": platform,
        "platform_supported": platform == "macos" || platform == "windows" || platform == "linux",
        "installed": driver.is_some(),
        "version": version.map(|version| format!("cua-driver {version}")),
        "ready": driver.is_some(),
        "can_grant": platform == "macos",
        "checks": [],
        "accessibility": null,
        "screen_recording": null,
        "screen_recording_capturable": null,
        "source": null,
        "error": null,
    }))
    .into_response()
}

/// `POST /api/tools/computer-use/permissions/grant` — spawn
/// `cua-driver permissions grant` as a background action (hermes
/// `grant_computer_use_permissions` parity). macOS-only: the driver
/// launches CuaDriver via LaunchServices so the TCC dialog is attributed
/// to com.trycua.driver, then waits for approval. The frontend polls
/// `GET /api/actions/computer-use-grant/status` and re-reads
/// `/api/tools/computer-use/status` once it exits. Off macOS there is no
/// TCC model to grant, so the route answers 400 there.
pub async fn computer_use_grant(State(_state): State<Arc<GatewayState>>) -> Response {
    if std::env::consts::OS != "macos" {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "Computer Use permission grants are a macOS concept."
            })),
        )
            .into_response();
    }
    let outcome = start_action("computer-use-grant", || {
        let Some(driver) = crate::computer_use::resolve_cua_driver_cmd() else {
            return Err("cua-driver: not installed. Run: ulnclaw computer-use install".into());
        };
        let config = crate::config::UlncLawConfig::load(None).unwrap_or_default();
        let mut cmd = std::process::Command::new(&driver);
        cmd.args(["permissions", "grant"])
            .stdin(std::process::Stdio::null());
        for (key, value) in crate::computer_use::cua_driver_child_env(&config.computer_use) {
            cmd.env(key, value);
        }
        let output = cmd
            .output()
            .map_err(|err| format!("cua-driver permissions grant failed: {err}"))?;
        let mut lines = Vec::new();
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            lines.push(line.to_string());
        }
        for line in String::from_utf8_lossy(&output.stderr).lines() {
            lines.push(line.to_string());
        }
        if output.status.success() {
            Ok(lines)
        } else {
            Err(format!(
                "cua-driver permissions grant exited with code {}",
                output.status.code().unwrap_or(-1)
            ))
        }
    });
    Json(outcome).into_response()
}

// ---------------------------------------------------------------------------
// Cron blueprints + delivery targets
// ---------------------------------------------------------------------------

/// `GET /api/cron/blueprints` — blueprint-capable installed skills as form
/// schemas (ulnclaw's catalog is skill-driven, hermes' is recipe-driven).
pub async fn cron_blueprints(State(state): State<Arc<GatewayState>>) -> Response {
    let dir = skills_dir(&state);
    let mut blueprints = Vec::new();
    for skill in crate::skills::list_skills(&dir) {
        if let Some(spec) =
            crate::skills::blueprint::blueprint_spec_for_installed(&dir, &skill.name)
        {
            blueprints.push(json!({
                "key": skill.name,
                "title": skill.name,
                "description": skill.description,
                "category": "skills",
                "tags": [skill.category],
                "fields": [
                    {"name": "schedule", "type": "text", "label": "Schedule", "default": spec.schedule, "options": [], "optional": false, "strict": false, "help": "cron expression or @every duration"},
                    {"name": "deliver", "type": "text", "label": "Deliver", "default": spec.deliver, "options": [], "optional": true, "strict": false, "help": "platform/chat target"},
                    {"name": "prompt", "type": "text", "label": "Prompt", "default": spec.prompt, "options": [], "optional": true, "strict": false, "help": "task prompt override"},
                ],
                "command": "",
                "appUrl": "",
            }));
        }
    }
    Json(json!({"blueprints": blueprints})).into_response()
}

#[derive(Debug, Deserialize, Default)]
pub struct BlueprintInstantiateBody {
    blueprint: Option<String>,
    #[serde(default)]
    values: HashMap<String, String>,
}

/// `POST /api/cron/blueprints/instantiate` — fill slots and create the job.
pub async fn cron_blueprints_instantiate(
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<BlueprintInstantiateBody>,
) -> Response {
    let dir = skills_dir(&state);
    let key = body.blueprint.unwrap_or_default();
    let Some(mut spec) = crate::skills::blueprint::blueprint_spec_for_installed(&dir, &key) else {
        return super::not_found(&format!("unknown blueprint {key}"));
    };
    if let Some(schedule) = body
        .values
        .get("schedule")
        .filter(|value| !value.trim().is_empty())
    {
        spec.schedule = schedule.clone();
    }
    if let Some(deliver) = body
        .values
        .get("deliver")
        .filter(|value| !value.trim().is_empty())
    {
        spec.deliver = deliver.clone();
    }
    if let Some(prompt) = body
        .values
        .get("prompt")
        .filter(|value| !value.trim().is_empty())
    {
        spec.prompt = Some(prompt.clone());
    }
    let job = match crate::skills::blueprint::blueprint_to_job(&spec, None) {
        Ok(job) => job,
        Err(err) => return super::bad_request(&err, None),
    };
    super::create_job(
        State(state),
        Json(super::CreateJobRequest {
            name: Some(job.name),
            schedule: Some(job.schedule),
            prompt: Some(job.prompt),
            deliver: job.deliver.map(Value::String),
            skills: Some(job.skills),
            ..Default::default()
        }),
    )
    .await
}

/// `GET /api/cron/delivery-targets` — platforms with configured home chats.
pub async fn cron_delivery_targets(State(_state): State<Arc<GatewayState>>) -> Response {
    let config = crate::config::UlncLawConfig::load(None).unwrap_or_default();
    let m = &config.messaging;
    let platforms: Vec<(&str, bool)> = vec![
        ("telegram", m.telegram.enabled),
        ("discord", m.discord.enabled),
        ("slack", m.slack.enabled),
        ("signal", m.signal.enabled),
        ("email", m.email.enabled),
        ("mattermost", m.mattermost.enabled),
        ("matrix", m.matrix.enabled),
        ("dingtalk", m.dingtalk.enabled),
        ("wecom", m.wecom.enabled),
        ("feishu", m.feishu.enabled),
        ("weixin", m.weixin.enabled),
        ("qq", m.qq.enabled),
    ];
    let env_on_disk = crate::config::load_env_file(&home().join(".env"));
    let targets: Vec<Value> = platforms
        .into_iter()
        .filter(|(_, enabled)| *enabled)
        .map(|(id, _)| {
            let var = format!("{}_HOME_CHANNEL", id.to_uppercase());
            let home_value = crate::cron::delivery::home_target_chat_id(id)
                .or_else(|| crate::config::get_env_value(&var))
                .or_else(|| env_on_disk.get(&var).cloned())
                .filter(|value| !value.trim().is_empty());
            json!({
                "id": id,
                "name": id,
                "home_env_var": var,
                "home_target_set": home_value.is_some(),
            })
        })
        .collect();
    Json(json!({"targets": targets})).into_response()
}

// ---------------------------------------------------------------------------
// Ops: doctor / backup / debug-share
// ---------------------------------------------------------------------------

/// `POST /api/ops/doctor` — background doctor run, poll via /api/actions.
pub async fn ops_doctor(State(_state): State<Arc<GatewayState>>) -> Response {
    let response = start_action("doctor", || {
        let config = crate::config::UlncLawConfig::load(None).unwrap_or_default();
        let opts = crate::doctor::DoctorOptions {
            fix: false,
            online: false,
            json: false,
        };
        let report = crate::doctor::run_doctor(&config, &opts);
        let rendered = report.render();
        Ok(rendered.lines().map(str::to_string).collect())
    });
    Json(response).into_response()
}

/// `POST /api/ops/backup` — zip the home dir (synchronous; fast enough).
pub async fn ops_backup(State(_state): State<Arc<GatewayState>>) -> Response {
    let home = home();
    let result =
        tokio::task::spawn_blocking(move || crate::backup::create_backup(&home, None)).await;
    match result {
        Ok(Ok(summary)) => {
            let archive = summary.out_path.display().to_string();
            let mut lines = crate::backup::format_backup_summary(&summary)
                .lines()
                .map(str::to_string)
                .collect::<Vec<_>>();
            lines.insert(0, format!("archive: {archive}"));
            actions().lock().unwrap().insert(
                "backup".into(),
                ActionRecord {
                    running: false,
                    exit_code: Some(0),
                    lines,
                },
            );
            Json(json!({
                "name": "backup",
                "ok": true,
                "pid": std::process::id(),
                "archive": archive,
            }))
            .into_response()
        }
        Ok(Err(err)) => {
            Json(json!({"name": "backup", "ok": false, "pid": null, "error": err})).into_response()
        }
        Err(err) => super::server_error(&err.to_string()),
    }
}

/// `POST /api/ops/debug-share` — write a redacted dump locally (no paste
/// service in ulnclaw; the file path is returned as the share url).
pub async fn ops_debug_share(State(_state): State<Arc<GatewayState>>) -> Response {
    let config = crate::config::UlncLawConfig::load(None).unwrap_or_default();
    let dump = crate::dump::build_dump(&config, None, false);
    let dumps = home().join("dumps");
    if let Err(err) = std::fs::create_dir_all(&dumps) {
        return super::server_error(&err.to_string());
    }
    let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let path = dumps.join(format!("ulnclaw-dump-{stamp}.txt"));
    if let Err(err) = std::fs::write(&path, dump) {
        return super::server_error(&err.to_string());
    }
    Json(json!({
        "ok": true,
        "urls": {"dump": path.display().to_string()},
        "failures": {},
        "redacted": true,
        "auto_delete_seconds": null,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// Memory providers
// ---------------------------------------------------------------------------

/// `GET /api/memory/providers/:provider/config?surface=declared`.
pub async fn memory_provider_config(Path(provider): Path<String>) -> Response {
    Json(json!({
        "name": provider,
        "label": provider,
        "docs_url": "",
        "fields": [],
    }))
    .into_response()
}

/// `GET /api/memory/providers/:provider/oauth/status`.
pub async fn memory_provider_oauth_status(Path(_provider): Path<String>) -> Response {
    Json(json!({
        "auth": null,
        "connected": false,
        "detail": "no oauth memory providers configured",
        "state": "idle",
    }))
    .into_response()
}

/// `POST /api/memory/providers/:provider/oauth/start`.
pub async fn memory_provider_oauth_start(Path(_provider): Path<String>) -> Response {
    Json(json!({"ok": false, "error": "oauth memory providers are not configured in this build"}))
        .into_response()
}

// ---------------------------------------------------------------------------
// Webhooks + updater
// ---------------------------------------------------------------------------

/// `POST /api/webhooks/enable` — the webhook platform is config-driven in
/// ulnclaw; report enabled so the Webhooks page can manage subscriptions.
pub async fn webhooks_enable(State(_state): State<Arc<GatewayState>>) -> Response {
    Json(json!({
        "ok": true,
        "enabled": true,
        "platform": "webhook",
        "needs_restart": false,
        "restart_started": false,
        "restart_pid": null,
    }))
    .into_response()
}

/// `GET /api/ulnclaw/update/check` — desktop-driven updater; the gateway
/// side reports the running version and no server-pushed update.
pub async fn update_check(State(_state): State<Arc<GatewayState>>) -> Response {
    Json(json!({
        "update_available": false,
        "current_version": crate::VERSION,
        "latest_version": crate::VERSION,
        "channel": "stable",
    }))
    .into_response()
}

/// `POST /api/ulnclaw/update` — no-op action (the desktop shell owns updates).
pub async fn update_run(State(_state): State<Arc<GatewayState>>) -> Response {
    Json(json!({"name": "update", "ok": false, "pid": null})).into_response()
}

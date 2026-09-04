//! Kanban triage pipeline — `specify` + `decompose` (ports of hermes
//! `hermes_cli/kanban_specify.py` + `hermes_cli/kanban_decompose.py`,
//! v2026.8.3).
//!
//! A task parked in the `triage` column is a rough idea. `specify` asks
//! the auxiliary LLM for a tightened `{title, body}` spec and promotes
//! triage→todo; `decompose` asks for a small dependency graph of child
//! tasks routed to profiles and fans it out atomically (root stays alive
//! as the parent-of-all-children wake-up card). Both are one-shot, lenient
//! parses that never raise on expected failure modes — outcomes carry
//! `ok=false` + reason so `--all` sweeps continue past individual
//! failures.

use std::sync::Arc;

use serde_json::Value;

use crate::config::UlncLawConfig;
use crate::error::AgentError;
use crate::kanban::{DecomposeChild, KanbanStore};
use crate::provider::{Message, Provider, ProviderRequest, Role};

/// Auxiliary task key for the specifier (hermes `auxiliary.triage_specifier`).
pub const TASK_TRIAGE_SPECIFIER: &str = "triage_specifier";
/// Auxiliary task key for the decomposer (hermes `auxiliary.kanban_decomposer`).
pub const TASK_KANBAN_DECOMPOSER: &str = "kanban_decomposer";

fn specify_max_tokens() -> u32 {
    let from_env = std::env::var("ULNCLAW_KANBAN_SPECIFY_MAX_TOKENS")
        .ok()
        .or_else(|| std::env::var("HERMES_KANBAN_SPECIFY_MAX_TOKENS").ok())
        .and_then(|raw| raw.parse::<u32>().ok())
        .unwrap_or(6000);
    from_env.max(1500)
}

// =========================================================================
// Prompts (hermes kanban_specify._SYSTEM_PROMPT / kanban_decompose
// ._SYSTEM_PROMPT, board name adapted)
// =========================================================================

const SPECIFY_SYSTEM_PROMPT: &str = r#"You are the Kanban triage specifier for the ulnclaw board.
A user dropped a rough idea into the Triage column. Your job is to turn it
into a concrete, actionable task spec that an autonomous worker can pick up
and execute without further clarification.

Output a single JSON object with exactly two keys:

  {
    "title": "<tightened task title, <= 80 chars, imperative voice>",
    "body":  "<multi-line spec, see structure below>"
  }

The body MUST include these sections, each prefixed with a bold markdown
heading, in this order:

  **Goal** — one sentence, user-facing outcome.
  **Approach** — 2-5 bullets on how a worker should tackle it.
  **Acceptance criteria** — checklist of concrete, verifiable conditions.
  **Out of scope** — short list of things NOT to touch (omit if nothing
      obvious; never invent scope creep).

Rules:
  - Keep the tightened title close in meaning to the original idea — do
    NOT invent a different project.
  - If the original idea is already detailed, preserve its substance and
    just reformat into the sections above.
  - Never add invented requirements the user didn't hint at.
  - No preamble, no closing remarks, no code fences around the JSON.
  - Output only the JSON object and nothing else.
"#;

const DECOMPOSE_SYSTEM_PROMPT: &str = r#"You are the Kanban decomposer for the ulnclaw board.

A user dropped a rough idea into the Triage column. Your job is to break it
into a small graph of concrete child tasks and route each one to the best-
matching profile from the available roster.

You will be given:
  - The original task title and body
  - The list of available profiles (each with name + description)
  - The fallback "default_assignee" used when no profile fits

Output a single JSON object with this exact shape:

  {
    "fanout": true,
    "rationale": "<one sentence on why this decomposition>",
    "tasks": [
      {
        "title": "<concrete task title, imperative voice, <= 80 chars>",
        "body":  "<detailed spec for the worker on this child task>",
        "assignee": "<profile name from the roster, or null for default>",
        "parents": [<int>, ...]
      },
      ...
    ]
  }

Rules:
  - "parents" is a list of INDICES (0-based) into this same "tasks" list,
    expressing actual data dependencies. Tasks with no parents run in
    PARALLEL. Tasks with parents wait until every parent completes.
  - Prefer parallelism. If two tasks can be done independently, give
    them no parents so the dispatcher fans them out at once.
  - Use 2-6 tasks for normal work. Don't create 20 tiny tasks. Don't
    cram everything into 1 task.
  - Pick assignees from the roster by matching the task to the profile's
    DESCRIPTION (not just the name). When nothing matches well, use null
    and the system will route to the default_assignee.
  - Each child task body is what a fresh worker will read with no other
    context — be specific about goal, approach, and acceptance criteria.

When the task is genuinely a single unit of work (no useful decomposition),
return:

  {
    "fanout": false,
    "rationale": "<one sentence>",
    "title": "<tightened title>",
    "body":  "<concrete spec for a single worker>",
    "assignee": "<profile name from the roster, or null for default>"
  }

In that case the task stays as one work item, just with a tightened spec and
a concrete assignee. If no profile fits, use null and the system will route to
the default_assignee.

No preamble, no closing remarks, no code fences. Output only the JSON object.
"#;

// =========================================================================
// Shared helpers
// =========================================================================

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut out: String = text.chars().take(limit.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Lenient JSON extraction — tolerates fenced code blocks and
/// leading/trailing whitespace; greedy first-`{`/last-`}` slice.
fn extract_json_blob(raw: &str) -> Option<Value> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let stripped = trimmed
        .trim_start_matches("```json")
        .trim_start_matches("```JSON")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    let first = stripped.find('{')?;
    let last = stripped.rfind('}')?;
    if last <= first {
        return None;
    }
    let candidate = &stripped[first..last + 1];
    let value: Value = serde_json::from_str(candidate).ok()?;
    if value.is_object() {
        Some(value)
    } else {
        None
    }
}

fn profile_author(fallback: &str) -> String {
    std::env::var("ULNCLAW_PROFILE")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            std::env::var("HERMES_PROFILE")
                .ok()
                .filter(|v| !v.trim().is_empty())
        })
        .or_else(|| std::env::var("USER").ok().filter(|v| !v.trim().is_empty()))
        .unwrap_or_else(|| fallback.to_string())
}

fn system_message(content: &str) -> Message {
    Message {
        role: Role::System,
        content: Some(content.to_string()),
        tool_calls: None,
        tool_call_id: None,
        name: None,
    }
}

fn user_message(content: String) -> Message {
    Message {
        role: Role::User,
        content: Some(content),
        tool_calls: None,
        tool_call_id: None,
        name: None,
    }
}

async fn call_aux(
    config: &UlncLawConfig,
    main_provider: Arc<dyn Provider>,
    task: &str,
    user_prompt: String,
    system_prompt: &str,
    max_tokens: u32,
) -> std::result::Result<String, AgentError> {
    let resolution = crate::provider::auxiliary::resolve_aux_task(config, task, main_provider)?;
    let request = ProviderRequest {
        messages: vec![system_message(system_prompt), user_message(user_prompt)],
        tools: Vec::new(),
        model: resolution.model,
        max_tokens: Some(max_tokens),
        temperature: Some(0.3),
        stream: false,
        stop: None,

        images: None,
    };
    let response = resolution.provider.chat_completion(request).await?;
    Ok(response.content.unwrap_or_default())
}

/// Profile roster for the decomposer prompt (hermes `_build_roster`).
/// ulnclaw profiles carry no description yet, so entries say so.
fn build_roster(config: &UlncLawConfig) -> (Vec<(String, String)>, Vec<String>) {
    let mut names: Vec<String> = config.profiles.keys().cloned().collect();
    names.sort();
    let roster: Vec<(String, String)> = names
        .iter()
        .map(|name| {
            (
                name.clone(),
                format!("(no description; profile named '{name}')"),
            )
        })
        .collect();
    (roster, names)
}

fn format_roster(roster: &[(String, String)]) -> String {
    if roster.is_empty() {
        return "  (no profiles installed — decomposer cannot route work)".to_string();
    }
    roster
        .iter()
        .map(|(name, description)| format!("  - {name} ⚠ undescribed: {description}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn known_assignee(config: &UlncLawConfig, name: &str) -> bool {
    name == "default" || config.profiles.contains_key(name)
}

/// Resolve the fallback assignee for unroutable children (hermes
/// `_resolve_default_assignee`).
pub fn resolve_default_assignee(config: &UlncLawConfig) -> String {
    if let Some(explicit) = config
        .kanban
        .default_assignee
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if known_assignee(config, explicit) {
            return explicit.to_string();
        }
    }
    "default".to_string()
}

/// Resolve the orchestrator profile owning the root after fan-out (hermes
/// `_resolve_orchestrator_profile`).
pub fn resolve_orchestrator_profile(config: &UlncLawConfig) -> String {
    if let Some(explicit) = config
        .kanban
        .orchestrator_profile
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if known_assignee(config, explicit) {
            return explicit.to_string();
        }
    }
    "default".to_string()
}

fn normalize_assignee_choice(
    choice: Option<&str>,
    default_assignee: &str,
    config: &UlncLawConfig,
) -> String {
    match choice.map(str::trim).filter(|s| !s.is_empty()) {
        Some(name) if known_assignee(config, name) => name.to_string(),
        _ => default_assignee.to_string(),
    }
}

// =========================================================================
// Outcomes
// =========================================================================

/// Result of specifying a single triage task (hermes `SpecifyOutcome`).
#[derive(Debug, Clone)]
pub struct SpecifyOutcome {
    pub task_id: String,
    pub ok: bool,
    pub reason: String,
    pub new_title: Option<String>,
}

/// Result of decomposing a single triage task (hermes `DecomposeOutcome`).
#[derive(Debug, Clone)]
pub struct DecomposeOutcome {
    pub task_id: String,
    pub ok: bool,
    pub reason: String,
    pub fanout: bool,
    pub child_ids: Option<Vec<String>>,
    pub new_title: Option<String>,
}

/// Task ids currently in the triage column (hermes `list_triage_ids`).
pub fn list_triage_ids(store: &KanbanStore) -> crate::error::Result<Vec<String>> {
    Ok(store
        .list_tasks(None, Some("triage"), None, None, 1000)?
        .into_iter()
        .map(|task| task.id)
        .collect())
}

// =========================================================================
// specify (hermes specify_task)
// =========================================================================

/// Specify a single triage task and promote it to `todo`. Never returns an
/// `Err` for expected failure modes (not in triage, aux unconfigured, API
/// error, malformed response) — those surface as `ok=false` so `--all`
/// sweeps keep going.
pub async fn specify_task(
    store: &KanbanStore,
    config: &UlncLawConfig,
    main_provider: Arc<dyn Provider>,
    task_id: &str,
    author: Option<&str>,
) -> SpecifyOutcome {
    let outcome = |ok: bool, reason: &str| SpecifyOutcome {
        task_id: task_id.to_string(),
        ok,
        reason: reason.to_string(),
        new_title: None,
    };
    let task = match store.get_task(task_id) {
        Ok(Some(task)) => task,
        Ok(None) => return outcome(false, "unknown task id"),
        Err(_) => return outcome(false, "unknown task id"),
    };
    if task.status != "triage" {
        return outcome(
            false,
            &format!("task is not in triage (status={})", task.status),
        );
    }

    let user_prompt = format!(
        "Task id: {}\nCurrent title: {}\nCurrent body:\n{}\n",
        task.id,
        truncate(&task.title, 400),
        truncate(
            if task.body.is_empty() {
                "(no body)"
            } else {
                &task.body
            },
            4000
        ),
    );
    let raw = match call_aux(
        config,
        main_provider,
        TASK_TRIAGE_SPECIFIER,
        user_prompt,
        SPECIFY_SYSTEM_PROMPT,
        specify_max_tokens(),
    )
    .await
    {
        Ok(raw) => raw,
        Err(e) => {
            tracing::info!("specify: API call failed for {task_id} ({e}) — skipping");
            return outcome(false, "LLM error: transport/config");
        }
    };
    let raw = raw.trim().to_string();

    let parsed = extract_json_blob(&raw);
    let (new_title, new_body): (Option<String>, Option<String>) = match parsed {
        None => {
            if raw.is_empty() {
                return outcome(false, "LLM returned an empty response");
            }
            // Fall back: whole reply is the body, title untouched.
            (None, Some(raw.clone()))
        }
        Some(value) => {
            let title = value
                .get("title")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_string);
            let body = value
                .get("body")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|b| !b.is_empty())
                .map(str::to_string);
            if title.is_none() && body.is_none() {
                return outcome(false, "LLM response missing title and body");
            }
            (title, body)
        }
    };

    let author = author
        .map(str::to_string)
        .unwrap_or_else(|| profile_author("specifier"));
    match store.specify_triage_task(
        task_id,
        new_title.as_deref(),
        new_body.as_deref(),
        None,
        &author,
    ) {
        Ok(true) => SpecifyOutcome {
            task_id: task_id.to_string(),
            ok: true,
            reason: "specified".to_string(),
            new_title,
        },
        Ok(false) => outcome(false, "task moved out of triage before promotion"),
        Err(_) => outcome(false, "task moved out of triage before promotion"),
    }
}

// =========================================================================
// decompose (hermes decompose_task)
// =========================================================================

/// Decompose a triage task into a graph of child tasks. Same fail-soft
/// contract as [`specify_task`].
pub async fn decompose_task(
    store: &KanbanStore,
    config: &UlncLawConfig,
    main_provider: Arc<dyn Provider>,
    task_id: &str,
    author: Option<&str>,
) -> DecomposeOutcome {
    let outcome = |ok: bool, reason: &str| DecomposeOutcome {
        task_id: task_id.to_string(),
        ok,
        reason: reason.to_string(),
        fanout: false,
        child_ids: None,
        new_title: None,
    };
    let task = match store.get_task(task_id) {
        Ok(Some(task)) => task,
        _ => return outcome(false, "unknown task id"),
    };
    if task.status != "triage" {
        return outcome(
            false,
            &format!("task is not in triage (status={})", task.status),
        );
    }

    let orchestrator = resolve_orchestrator_profile(config);
    let default_assignee = resolve_default_assignee(config);
    let auto_promote = config.kanban.auto_promote_children;
    let (roster, _valid_names) = build_roster(config);

    let user_prompt = format!(
        "Task id: {}\nTitle: {}\nBody:\n{}\n\nAvailable profiles (assignees you may pick from):\n{}\n\nDefault assignee (used when no profile fits a task): {}\n",
        task.id,
        truncate(&task.title, 400),
        truncate(if task.body.is_empty() { "(no body)" } else { &task.body }, 4000),
        format_roster(&roster),
        default_assignee,
    );
    let raw = match call_aux(
        config,
        main_provider,
        TASK_KANBAN_DECOMPOSER,
        user_prompt,
        DECOMPOSE_SYSTEM_PROMPT,
        4000,
    )
    .await
    {
        Ok(raw) => raw,
        Err(e) => {
            tracing::info!("decompose: API call failed for {task_id} ({e})");
            return outcome(false, "LLM error: transport/config");
        }
    };
    let Some(parsed) = extract_json_blob(&raw) else {
        return outcome(false, "LLM returned malformed JSON");
    };
    let audit_author = author
        .map(str::to_string)
        .unwrap_or_else(|| profile_author("decomposer"));
    let fanout = parsed
        .get("fanout")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if !fanout {
        // Single-task fallback: same effect as specify (+ assignee routing).
        let title = parsed
            .get("title")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string);
        let body = parsed
            .get("body")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|b| !b.is_empty())
            .map(str::to_string);
        let assignee = if task.assignee.as_deref().map_or(true, str::is_empty) {
            Some(normalize_assignee_choice(
                parsed.get("assignee").and_then(|v| v.as_str()),
                &default_assignee,
                config,
            ))
        } else {
            None
        };
        if title.is_none() && body.is_none() {
            return outcome(false, "decomposer returned fanout=false with no title/body");
        }
        return match store.specify_triage_task(
            task_id,
            title.as_deref(),
            body.as_deref(),
            assignee.as_deref(),
            &audit_author,
        ) {
            Ok(true) => DecomposeOutcome {
                task_id: task_id.to_string(),
                ok: true,
                reason: "single task (no fanout)".to_string(),
                fanout: false,
                child_ids: None,
                new_title: title,
            },
            Ok(false) | Err(_) => outcome(false, "task moved out of triage before promotion"),
        };
    }

    let Some(raw_tasks) = parsed.get("tasks").and_then(|v| v.as_array()) else {
        return outcome(
            false,
            "decomposer returned fanout=true with empty tasks list",
        );
    };
    if raw_tasks.is_empty() {
        return outcome(
            false,
            "decomposer returned fanout=true with empty tasks list",
        );
    }

    let mut children: Vec<DecomposeChild> = Vec::new();
    for (idx, entry) in raw_tasks.iter().enumerate() {
        let Some(entry) = entry.as_object() else {
            return outcome(false, &format!("tasks[{idx}] is not an object"));
        };
        let Some(title) = entry
            .get("title")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|t| !t.is_empty())
        else {
            return outcome(false, &format!("tasks[{idx}].title is missing or empty"));
        };
        let body = entry
            .get("body")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or_default()
            .to_string();
        let chosen = normalize_assignee_choice(
            entry.get("assignee").and_then(|v| v.as_str()),
            &default_assignee,
            config,
        );
        let parents: Vec<usize> = entry
            .get("parents")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|p| p.as_u64())
                    .map(|p| p as usize)
                    .filter(|&p| p < raw_tasks.len() && p != idx)
                    .collect()
            })
            .unwrap_or_default();
        children.push(DecomposeChild {
            title: title.chars().take(200).collect(),
            body,
            assignee: Some(chosen),
            parents,
        });
    }

    match store.decompose_triage_task(
        task_id,
        Some(&orchestrator),
        &children,
        &audit_author,
        auto_promote,
    ) {
        Ok(Some(child_ids)) => DecomposeOutcome {
            task_id: task_id.to_string(),
            ok: true,
            reason: format!("decomposed into {} children", child_ids.len()),
            fanout: true,
            child_ids: Some(child_ids),
            new_title: None,
        },
        Ok(None) => outcome(false, "task moved out of triage before decomposition"),
        Err(e) => outcome(false, &format!("DB rejected graph: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_json_blob_is_lenient() {
        let plain = extract_json_blob(r#"{"title": "A", "body": "B"}"#).unwrap();
        assert_eq!(plain["title"], "A");
        let fenced = extract_json_blob("```json\n{\"fanout\": false}\n```").unwrap();
        assert_eq!(fenced["fanout"], false);
        let noisy =
            extract_json_blob("Sure! Here you go: {\"tasks\": []} hope that helps").unwrap();
        assert!(noisy.get("tasks").is_some());
        assert!(extract_json_blob("no json here").is_none());
        assert!(extract_json_blob("").is_none());
        assert!(extract_json_blob("[1, 2]").is_none());
    }

    #[test]
    fn default_assignee_and_orchestrator_resolution() {
        let mut config = UlncLawConfig::default();
        assert_eq!(resolve_default_assignee(&config), "default");
        assert_eq!(resolve_orchestrator_profile(&config), "default");
        config
            .profiles
            .insert("writer".into(), crate::config::ProfileOverride::default());
        config.kanban.default_assignee = Some("writer".into());
        config.kanban.orchestrator_profile = Some("ghost".into()); // unknown
        assert_eq!(resolve_default_assignee(&config), "writer");
        assert_eq!(resolve_orchestrator_profile(&config), "default");
    }

    #[test]
    fn roster_lists_profiles_sorted() {
        let mut config = UlncLawConfig::default();
        config
            .profiles
            .insert("zeta".into(), crate::config::ProfileOverride::default());
        config
            .profiles
            .insert("alpha".into(), crate::config::ProfileOverride::default());
        let (roster, names) = build_roster(&config);
        assert_eq!(names, vec!["alpha".to_string(), "zeta".to_string()]);
        let rendered = format_roster(&roster);
        assert!(rendered.contains("- alpha"));
        assert!(rendered.contains("undescribed"));
    }

    #[test]
    fn list_triage_ids_only_sees_triage_column() {
        let dir = tempfile::tempdir().unwrap();
        let store = KanbanStore::open(dir.path().join("kanban.db")).unwrap();
        store
            .create_task(&crate::kanban::NewTask {
                title: "idea".into(),
                created_by: "tester".into(),
                triage: true,
                ..Default::default()
            })
            .unwrap();
        store
            .create_task(&crate::kanban::NewTask {
                title: "normal".into(),
                created_by: "tester".into(),
                ..Default::default()
            })
            .unwrap();
        let ids = list_triage_ids(&store).unwrap();
        assert_eq!(ids.len(), 1);
    }
}

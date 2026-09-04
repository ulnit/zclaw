//! Kanban diagnostics — structured, actionable distress signals for
//! tasks (P678).
//!
//! Port of the operator-fixable rule subset of hermes
//! `kanban_diagnostics.py` (v2026.8.3): repeated failures, crash loops,
//! stuck-blocked, stranded-in-ready, block/unblock cycling, and
//! hallucinated card references. Rules run over (task, events, runs),
//! are stateless and read-only — no DB writes. Diagnostics auto-clear
//! when the underlying failure mode resolves.
//!
//! Skipped hermes rules: `triage_aux_unavailable` (hermes aux-model
//! config surface) and `prose_phantom_refs` (subsumed here by
//! `hallucinated_cards`).

use serde::{Deserialize, Serialize};

use crate::kanban::{KanbanStore, Run, Task, TaskEvent};

pub const SEVERITY_WARNING: &str = "warning";
pub const SEVERITY_ERROR: &str = "error";
pub const SEVERITY_CRITICAL: &str = "critical";

/// One recovery action attached to a diagnostic (hermes
/// `DiagnosticAction`). `kind` drives UI/CLI rendering: reclaim /
/// reassign / unblock / comment / cli_hint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticAction {
    pub kind: String,
    pub label: String,
    /// Rendered copy-and-run CLI command (ulnclaw extension over the
    /// hermes payload-free action model).
    #[serde(default)]
    pub hint: String,
    #[serde(default)]
    pub suggested: bool,
}

/// One active distress signal on a task (hermes `Diagnostic`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnostic {
    pub kind: String,
    pub severity: String,
    pub title: String,
    pub detail: String,
    pub actions: Vec<DiagnosticAction>,
}

/// Rule thresholds (hermes `DEFAULT_CONFIG`).
#[derive(Debug, Clone)]
pub struct DiagnosticsConfig {
    /// Consecutive failed runs before firing (hermes
    /// `failure_threshold`, matches the dispatcher failure limit).
    pub failure_threshold: usize,
    /// Consecutive crashed runs before firing (hermes `crash_threshold`).
    pub crash_threshold: usize,
    /// Blocked age before a task counts as stuck (hermes
    /// `blocked_stale_hours`, default 24h).
    pub blocked_stale_secs: i64,
    /// Ready-but-unclaimed age before a task counts as stranded (hermes
    /// default 30 min).
    pub stranded_ready_secs: i64,
    /// Block/unblock event count in the recent trail that counts as
    /// cycling (hermes default window).
    pub cycling_events: usize,
}

impl Default for DiagnosticsConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 2,
            crash_threshold: 2,
            blocked_stale_secs: 24 * 3600,
            stranded_ready_secs: 30 * 60,
            cycling_events: 4,
        }
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn action(kind: &str, label: &str, hint: &str, suggested: bool) -> DiagnosticAction {
    DiagnosticAction {
        kind: kind.to_string(),
        label: label.to_string(),
        hint: hint.to_string(),
        suggested,
    }
}

/// Run outcomes that count as a failure for `repeated_failures`.
const FAILURE_OUTCOMES: &[&str] = &["failed", "gave_up", "timed_out", "spawn_failed"];

/// Count trailing runs (newest first) whose outcome matches `matches`,
/// stopping at the first run that does not.
fn trailing_runs(runs: &[Run], matches: impl Fn(&Run) -> bool) -> Vec<&Run> {
    let mut streak = Vec::new();
    for run in runs.iter().rev() {
        if matches(run) {
            streak.push(run);
        } else {
            break;
        }
    }
    streak
}

fn rule_repeated_failures(task: &Task, runs: &[Run], cfg: &DiagnosticsConfig) -> Vec<Diagnostic> {
    let streak = trailing_runs(runs, |run| {
        run.outcome
            .as_deref()
            .map(|outcome| FAILURE_OUTCOMES.contains(&outcome))
            .unwrap_or(false)
    });
    if streak.len() < cfg.failure_threshold {
        return Vec::new();
    }
    let severity = if streak.len() > cfg.failure_threshold {
        SEVERITY_ERROR
    } else {
        SEVERITY_WARNING
    };
    let detail = streak
        .iter()
        .filter_map(|run| run.error.as_deref())
        .next()
        .map(|error| format!("last error: {error}"))
        .unwrap_or_default();
    vec![Diagnostic {
        kind: "repeated_failures".to_string(),
        severity: severity.to_string(),
        title: format!("{} consecutive failed run(s)", streak.len()),
        detail,
        actions: vec![
            action(
                "reclaim",
                "Reclaim for a fresh worker",
                &format!("ulnclaw kanban reclaim {}", task.id),
                true,
            ),
            action(
                "reassign",
                "Reassign to another worker",
                &format!("ulnclaw kanban assign {} <worker>", task.id),
                false,
            ),
            action(
                "cli_hint",
                "Inspect the task",
                &format!("ulnclaw kanban show {}", task.id),
                false,
            ),
        ],
    }]
}

fn rule_repeated_crashes(task: &Task, runs: &[Run], cfg: &DiagnosticsConfig) -> Vec<Diagnostic> {
    let streak = trailing_runs(runs, |run| run.outcome.as_deref() == Some("crashed"));
    if streak.len() < cfg.crash_threshold {
        return Vec::new();
    }
    vec![Diagnostic {
        kind: "repeated_crashes".to_string(),
        severity: SEVERITY_ERROR.to_string(),
        title: format!("crash loop: {} consecutive crashed run(s)", streak.len()),
        detail: "workers die before completing; check the dispatcher host and task prompt"
            .to_string(),
        actions: vec![
            action(
                "reclaim",
                "Reclaim for a fresh worker",
                &format!("ulnclaw kanban reclaim {}", task.id),
                true,
            ),
            action(
                "reassign",
                "Reassign to another worker",
                &format!("ulnclaw kanban assign {} <worker>", task.id),
                false,
            ),
            action(
                "cli_hint",
                "Show the run history",
                &format!("ulnclaw kanban runs {}", task.id),
                false,
            ),
        ],
    }]
}

fn last_event_ts(events: &[TaskEvent], kinds: &[&str]) -> Option<i64> {
    events
        .iter()
        .filter(|event| kinds.contains(&event.kind.as_str()))
        .map(|event| event.created_at)
        .max()
}

fn rule_stuck_in_blocked(
    task: &Task,
    events: &[TaskEvent],
    cfg: &DiagnosticsConfig,
    now: i64,
) -> Vec<Diagnostic> {
    if task.status != "blocked" {
        return Vec::new();
    }
    let blocked_at = last_event_ts(events, &["blocked"]).unwrap_or(task.created_at);
    let age = now - blocked_at;
    if age < cfg.blocked_stale_secs {
        return Vec::new();
    }
    vec![Diagnostic {
        kind: "stuck_in_blocked".to_string(),
        severity: SEVERITY_WARNING.to_string(),
        title: format!("blocked for {}h with no resolution", age / 3600),
        detail: "the blocking reason may need a human decision".to_string(),
        actions: vec![
            action(
                "unblock",
                "Unblock back to ready",
                &format!("ulnclaw kanban unblock {}", task.id),
                true,
            ),
            action(
                "comment",
                "Add a comment with the decision",
                &format!("ulnclaw kanban comment {} \"<decision>\"", task.id),
                false,
            ),
        ],
    }]
}

fn rule_stranded_in_ready(
    task: &Task,
    events: &[TaskEvent],
    cfg: &DiagnosticsConfig,
    now: i64,
) -> Vec<Diagnostic> {
    if task.status != "ready" || task.claim_lock.is_some() {
        return Vec::new();
    }
    let ready_at =
        last_event_ts(events, &["ready", "promoted", "reclaimed"]).unwrap_or(task.created_at);
    let age = now - ready_at;
    if age < cfg.stranded_ready_secs {
        return Vec::new();
    }
    vec![Diagnostic {
        kind: "stranded_in_ready".to_string(),
        severity: SEVERITY_WARNING.to_string(),
        title: format!("ready for {}m but never claimed", age / 60),
        detail: "no dispatcher/worker picked the task up".to_string(),
        actions: vec![action(
            "cli_hint",
            "Dispatch workers",
            "ulnclaw kanban dispatch",
            true,
        )],
    }]
}

fn rule_block_unblock_cycling(
    task: &Task,
    events: &[TaskEvent],
    cfg: &DiagnosticsConfig,
) -> Vec<Diagnostic> {
    let recent: Vec<&TaskEvent> = events
        .iter()
        .filter(|event| event.kind == "blocked" || event.kind == "unblocked")
        .collect();
    if recent.len() < cfg.cycling_events {
        return Vec::new();
    }
    vec![Diagnostic {
        kind: "block_unblock_cycling".to_string(),
        severity: SEVERITY_WARNING.to_string(),
        title: format!(
            "block/unblock cycling ({} events in the trail)",
            recent.len()
        ),
        detail: "the task keeps getting blocked and unblocked — the underlying issue is unresolved"
            .to_string(),
        actions: vec![action(
            "comment",
            "Add a comment explaining the cycle",
            &format!("ulnclaw kanban comment {} \"<root cause>\"", task.id),
            true,
        )],
    }]
}

fn rule_hallucinated_cards(task: &Task, store: &KanbanStore) -> Vec<Diagnostic> {
    // Scan title + body for task-id shaped references; any that resolve
    // to nothing on this board are hallucinated (hermes
    // `hallucinated_cards`).
    let mut candidates: Vec<String> = Vec::new();
    for text in [task.title.as_str(), task.body.as_str()] {
        let mut rest = text;
        while let Some(start) = rest.find("t_") {
            let tail = &rest[start..];
            let end = tail
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
                .unwrap_or(tail.len());
            let candidate = &tail[..end];
            if candidate.len() >= 5 && !candidates.iter().any(|c| c == candidate) {
                candidates.push(candidate.to_string());
            }
            rest = &tail[end.min(tail.len())..];
            if rest.is_empty() {
                break;
            }
        }
    }
    let mut missing: Vec<String> = Vec::new();
    for candidate in candidates {
        if candidate == task.id {
            continue;
        }
        match store.resolve_task_id(&candidate) {
            Ok(Some(_)) => {}
            _ => missing.push(candidate),
        }
    }
    if missing.is_empty() {
        return Vec::new();
    }
    vec![Diagnostic {
        kind: "hallucinated_cards".to_string(),
        severity: SEVERITY_WARNING.to_string(),
        title: format!("references {} non-existent card id(s)", missing.len()),
        detail: format!("unknown ids: {}", missing.join(", ")),
        actions: vec![
            action(
                "comment",
                "Correct the references in a comment",
                &format!("ulnclaw kanban comment {} \"<correct ids>\"", task.id),
                true,
            ),
            action(
                "cli_hint",
                "Edit the card text",
                &format!("ulnclaw kanban edit {}", task.id),
                false,
            ),
        ],
    }]
}

/// All diagnostics for one task (hermes `diagnose_task`).
pub fn diagnose_task_with(
    store: &KanbanStore,
    task: &Task,
    cfg: &DiagnosticsConfig,
) -> crate::error::Result<Vec<Diagnostic>> {
    let events = store.events(&task.id)?;
    let runs = store.list_runs(&task.id, true, None, None)?;
    let now = now_secs();
    let mut diagnostics = Vec::new();
    diagnostics.extend(rule_repeated_failures(task, &runs, cfg));
    diagnostics.extend(rule_repeated_crashes(task, &runs, cfg));
    diagnostics.extend(rule_stuck_in_blocked(task, &events, cfg, now));
    diagnostics.extend(rule_stranded_in_ready(task, &events, cfg, now));
    diagnostics.extend(rule_block_unblock_cycling(task, &events, cfg));
    diagnostics.extend(rule_hallucinated_cards(task, store));
    // Critical fires first (hermes severity ordering).
    diagnostics.sort_by_key(|d| match d.severity.as_str() {
        SEVERITY_CRITICAL => 0,
        SEVERITY_ERROR => 1,
        _ => 2,
    });
    Ok(diagnostics)
}

/// Diagnostics for one task by id with the default thresholds.
pub fn diagnose_task(store: &KanbanStore, task_id: &str) -> crate::error::Result<Vec<Diagnostic>> {
    let Some(task) = store.get_task(task_id)? else {
        return Ok(Vec::new());
    };
    diagnose_task_with(store, &task, &DiagnosticsConfig::default())
}

/// Board-wide diagnostics: `(task_id, diagnostics)` for every flagged
/// task on the current board (hermes board-load scan).
pub fn diagnose_board(store: &KanbanStore) -> crate::error::Result<Vec<(String, Vec<Diagnostic>)>> {
    let cfg = DiagnosticsConfig::default();
    let tasks = store.list_tasks(None, None, None, None, 500)?;
    let mut flagged = Vec::new();
    for task in tasks {
        let diagnostics = diagnose_task_with(store, &task, &cfg)?;
        if !diagnostics.is_empty() {
            flagged.push((task.id, diagnostics));
        }
    }
    Ok(flagged)
}

/// [`diagnose_task_with`] entry point used by the CLI twin: pulls
/// thresholds from the loaded [`crate::config::UlncLawConfig`]
/// (defaults until `[kanban.diagnostics]` lands) and never fails —
/// diagnostics are advisory.
pub fn compute_task_diagnostics(
    store: &KanbanStore,
    _config: &crate::config::UlncLawConfig,
    task: &Task,
) -> Vec<Diagnostic> {
    diagnose_task_with(store, task, &DiagnosticsConfig::default()).unwrap_or_default()
}

fn severity_rank(severity: &str) -> u8 {
    match severity {
        SEVERITY_CRITICAL => 2,
        SEVERITY_ERROR => 1,
        _ => 0,
    }
}

/// True when `severity` is at/above the `min` filter (unknown or
/// missing filters accept everything).
pub fn severity_at_or_above(severity: &str, min: Option<&str>) -> bool {
    let Some(min) = min else {
        return true;
    };
    severity_rank(severity) >= severity_rank(min)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kanban::{KanbanStore, NewTask};

    fn temp_store() -> (tempfile::TempDir, KanbanStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = KanbanStore::open(dir.path().join("kanban.db")).unwrap();
        (dir, store)
    }

    fn make_task(store: &KanbanStore, title: &str, body: &str) -> Task {
        store
            .create_task(&NewTask {
                title: title.into(),
                body: body.into(),
                created_by: "tester".into(),
                ..Default::default()
            })
            .unwrap()
    }

    /// One failed attempt: claim -> close run as failed -> release the
    /// claim back to ready (dispatcher accounting, high limit so the
    /// breaker never trips).
    fn fail_run(store: &KanbanStore, task_id: &str, worker: &str, error: &str) {
        store.claim_task(task_id, worker, 60).unwrap();
        store
            .close_active_run(task_id, "failed", "failed", None, Some(error))
            .unwrap();
        store
            .record_task_failure_with_release(task_id, error, "failed", 99)
            .unwrap();
    }

    #[test]
    fn repeated_failures_fires_on_streak() {
        let (_dir, store) = temp_store();
        let task = make_task(&store, "Flaky", "");
        store.ready_task(&task.id).unwrap();
        // Two failed runs in a row -> warning at threshold 2.
        fail_run(&store, &task.id, "w1", "boom one");
        fail_run(&store, &task.id, "w2", "boom two");

        let diagnostics = diagnose_task(&store, &task.id).unwrap();
        let kinds: Vec<&str> = diagnostics.iter().map(|d| d.kind.as_str()).collect();
        assert!(kinds.contains(&"repeated_failures"), "{kinds:?}");
        let failure = diagnostics
            .iter()
            .find(|d| d.kind == "repeated_failures")
            .unwrap();
        assert_eq!(failure.severity, SEVERITY_WARNING);
        assert!(failure.title.contains("2 consecutive"), "{}", failure.title);
        assert!(failure.detail.contains("boom two"), "{}", failure.detail);
        assert!(failure
            .actions
            .iter()
            .any(|a| a.kind == "reclaim" && a.suggested));
    }

    #[test]
    fn success_resets_failure_streak() {
        let (_dir, store) = temp_store();
        let task = make_task(&store, "Recovering", "");
        store.ready_task(&task.id).unwrap();
        fail_run(&store, &task.id, "w1", "boom");
        store.claim_task(&task.id, "w2", 60).unwrap();
        store.complete_task(&task.id, Some("ok")).unwrap();

        let diagnostics = diagnose_task(&store, &task.id).unwrap();
        assert!(
            !diagnostics.iter().any(|d| d.kind == "repeated_failures"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn repeated_crashes_fires_on_crash_loop() {
        let (_dir, store) = temp_store();
        let task = make_task(&store, "Crashy", "");
        store.ready_task(&task.id).unwrap();
        for worker in ["w1", "w2"] {
            store.claim_task(&task.id, worker, 60).unwrap();
            store
                .close_active_run(&task.id, "crashed", "crashed", None, Some("segfault"))
                .unwrap();
            store
                .record_task_failure_with_release(&task.id, "segfault", "crashed", 99)
                .unwrap();
        }
        let diagnostics = diagnose_task(&store, &task.id).unwrap();
        let crash = diagnostics
            .iter()
            .find(|d| d.kind == "repeated_crashes")
            .expect("repeated_crashes fires");
        assert_eq!(crash.severity, SEVERITY_ERROR);
        assert!(crash
            .actions
            .iter()
            .any(|a| a.kind == "reclaim" && a.suggested));
    }

    #[test]
    fn stuck_blocked_and_stranded_ready() {
        let (_dir, store) = temp_store();
        let cfg = DiagnosticsConfig {
            blocked_stale_secs: 1,
            stranded_ready_secs: 1,
            ..Default::default()
        };

        let blocked = make_task(&store, "Blocked", "");
        store.ready_task(&blocked.id).unwrap();
        let blocked = store.block_task(&blocked.id, "needs input").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let diagnostics = diagnose_task_with(&store, &blocked, &cfg).unwrap();
        assert!(
            diagnostics.iter().any(|d| d.kind == "stuck_in_blocked"),
            "{diagnostics:?}"
        );

        let stranded = make_task(&store, "Stranded", "");
        let stranded = store.ready_task(&stranded.id).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let diagnostics = diagnose_task_with(&store, &stranded, &cfg).unwrap();
        assert!(
            diagnostics.iter().any(|d| d.kind == "stranded_in_ready"),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn hallucinated_cards_detects_unknown_ids() {
        let (_dir, store) = temp_store();
        let real = make_task(&store, "Real", "");
        let haunted = make_task(
            &store,
            "Haunted",
            &format!("depends on {} and t_missingcard12", real.id),
        );
        let diagnostics = diagnose_task(&store, &haunted.id).unwrap();
        let hallucinated = diagnostics
            .iter()
            .find(|d| d.kind == "hallucinated_cards")
            .expect("hallucinated_cards fires");
        assert!(
            hallucinated.detail.contains("t_missingcard12"),
            "{}",
            hallucinated.detail
        );
        // The real reference does not count as missing.
        assert!(
            !hallucinated.detail.contains(&real.id),
            "{}",
            hallucinated.detail
        );
    }

    #[test]
    fn block_unblock_cycling_counts_trail() {
        let (_dir, store) = temp_store();
        let task = make_task(&store, "Yo-yo", "");
        store.ready_task(&task.id).unwrap();
        // Alternate block kinds: same-cause re-blocks trip the
        // loop-breaker (triage + block_loop_detected) instead of a
        // plain blocked event.
        for kind in ["needs_input", "capability"] {
            store
                .block_task_kind(&task.id, "again", Some(kind))
                .unwrap();
            store.unblock_task(&task.id).unwrap();
        }
        let diagnostics = diagnose_task(&store, &task.id).unwrap();
        assert!(
            diagnostics
                .iter()
                .any(|d| d.kind == "block_unblock_cycling"),
            "{diagnostics:?}"
        );
    }
}

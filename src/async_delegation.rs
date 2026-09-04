//! Background (fire-and-forget) delegation — port of hermes
//! `tools/async_delegation.py` core semantics.
//!
//! `delegate_task` with `background: true` dispatches the fan-out as a
//! detached task and returns immediately with a `delegation_id`. Per-child
//! results stream into live log files (`<home>/cache/delegation/live/<id>/
//! task-<n>.log`); when ALL children finish a consolidated report is written
//! (`result.json`) and delivered back to the parent conversation:
//!
//! - REPL turns drain the process-local completion queue between prompts
//!   (hermes CLI drain with positive-ownership filtering by session key).
//! - Gateway session chats drain the same queue before each turn.
//!
//! Durable registry (hermes sqlite store): when a session store is wired,
//! dispatches and consolidated results persist to the `async_delegations`
//! table so finished work survives process restarts. On startup
//! `recover_from_store` abandons delegations whose workers died with the
//! previous process — hermes `recover_abandoned_delegations`: the row is
//! given a terminal `unknown` outcome whose consolidated result is
//! delivered through the normal path. `drain_completions` claims both the
//! in-memory queue (same-process, ownership-filtered) and undelivered DB
//! rows through the durable delivery-claim lifecycle (hermes
//! `claim_completion_delivery` / `release_completion_delivery`): each claim
//! increments `delivery_attempts`, stale claims (>300s) are re-claimable,
//! completions whose origin session is gone are released and converge to
//! terminal `dropped` after 8 attempts, and successfully injected rows are
//! marked `delivered` under the claim token.
//! Live transcripts under `cache/delegation/live/` remain the inspectable
//! artifact.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;

use crate::session::sqlite::SqliteSessionStore;
use crate::tools::context::SubAgentRunner;

/// Status of a background delegation.
pub const STATUS_RUNNING: &str = "running";
pub const STATUS_COMPLETED: &str = "completed";
pub const STATUS_FAILED: &str = "failed";

#[derive(Debug, Clone)]
pub struct DelegationRecord {
    pub id: String,
    pub parent_session_key: String,
    pub tasks: usize,
    pub status: String,
    pub created_ms: i64,
    pub finished_ms: Option<i64>,
    pub log_dir: PathBuf,
}

/// A finished delegation waiting to be injected into the parent session.
#[derive(Debug, Clone)]
pub struct Completion {
    pub delegation_id: String,
    pub session_key: String,
    /// Consolidated, model-ready report text.
    pub message: String,
    pub result: serde_json::Value,
    pub finished_ms: i64,
}

#[derive(Default)]
struct DelegationState {
    records: HashMap<String, DelegationRecord>,
    completions: VecDeque<Completion>,
}

fn state() -> &'static Mutex<DelegationState> {
    static STATE: OnceLock<Mutex<DelegationState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(DelegationState::default()))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Live directory root: `<home>/cache/delegation/live` (hermes layout).
pub fn live_root(home: &Path) -> PathBuf {
    home.join("cache").join("delegation").join("live")
}

/// Dispatch a background fan-out. Returns the record immediately; the
/// children run on a detached tokio task bounded by `max_concurrent`.
pub fn dispatch_background_delegation(
    runner: Arc<dyn SubAgentRunner>,
    tasks: Vec<(String, String)>,
    parent_session_key: String,
    home: PathBuf,
    max_concurrent: usize,
    store: Option<Arc<SqliteSessionStore>>,
) -> Result<DelegationRecord, String> {
    if tasks.is_empty() {
        return Err("no tasks to delegate".to_string());
    }
    let id = uuid::Uuid::new_v4().to_string();
    let log_dir = live_root(&home).join(&id);
    std::fs::create_dir_all(&log_dir)
        .map_err(|e| format!("cannot create delegation log dir: {e}"))?;

    let record = DelegationRecord {
        id: id.clone(),
        parent_session_key: parent_session_key.clone(),
        tasks: tasks.len(),
        status: STATUS_RUNNING.to_string(),
        created_ms: now_ms(),
        finished_ms: None,
        log_dir: log_dir.clone(),
    };
    state()
        .lock()
        .unwrap()
        .records
        .insert(id.clone(), record.clone());
    if let Some(store) = &store {
        let tasks_json = serde_json::to_string(
            &tasks
                .iter()
                .map(|(goal, context)| json!({"goal": goal, "context": context}))
                .collect::<Vec<_>>(),
        )
        .unwrap_or_else(|_| "[]".to_string());
        if let Err(e) = store.persist_delegation_dispatch(&id, &parent_session_key, &tasks_json) {
            // Durability is best-effort; in-memory delivery still works.
            eprintln!("warning: persist delegation dispatch: {e}");
        }
    }

    let store_for_task = store.clone();
    tokio::spawn(async move {
        let results = run_batch(runner, tasks, &log_dir, max_concurrent.max(1)).await;
        finalize_delegation(
            &id,
            &parent_session_key,
            &log_dir,
            results,
            store_for_task.as_deref(),
        );
    });

    Ok(record)
}

/// Run every child (bounded concurrency), writing per-task live logs.
async fn run_batch(
    runner: Arc<dyn SubAgentRunner>,
    tasks: Vec<(String, String)>,
    log_dir: &Path,
    max_concurrent: usize,
) -> Vec<serde_json::Value> {
    let semaphore = Arc::new(tokio::sync::Semaphore::new(max_concurrent));
    let mut handles = Vec::new();
    for (idx, (goal, context)) in tasks.into_iter().enumerate() {
        let runner = runner.clone();
        let permit = Arc::clone(&semaphore);
        let log_path = log_dir.join(format!("task-{}.log", idx + 1));
        handles.push(tokio::spawn(async move {
            let _permit = permit.acquire().await.expect("semaphore open");
            let started = now_ms();
            let outcome = runner.run_subagent(&goal, &context).await;
            let elapsed_ms = now_ms() - started;
            let entry = match &outcome {
                Ok(answer) => json!({
                    "task": goal,
                    "status": "completed",
                    "elapsed_ms": elapsed_ms,
                    "result": answer,
                }),
                Err(e) => json!({
                    "task": goal,
                    "status": "error",
                    "elapsed_ms": elapsed_ms,
                    "error": e.to_string(),
                }),
            };
            let mut log = String::new();
            log.push_str(&format!("task: {}\n", goal));
            if !context.is_empty() {
                log.push_str(&format!("context: {}\n", context));
            }
            log.push_str(&format!("status: {}\n", entry["status"]));
            log.push_str(&serde_json::to_string_pretty(&entry).unwrap_or_default());
            log.push('\n');
            let _ = std::fs::write(&log_path, log);
            entry
        }));
    }

    let mut results = Vec::new();
    for (idx, handle) in handles.into_iter().enumerate() {
        match handle.await {
            Ok(entry) => results.push(entry),
            Err(e) => results.push(json!({
                "task": format!("task-{}", idx + 1),
                "status": "error",
                "error": format!("join: {e}"),
            })),
        }
    }
    results
}

/// Write the consolidated result, update the registry and enqueue the
/// completion for delivery to the parent session.
fn finalize_delegation(
    id: &str,
    parent_session_key: &str,
    log_dir: &Path,
    results: Vec<serde_json::Value>,
    store: Option<&SqliteSessionStore>,
) {
    let failed = results
        .iter()
        .filter(|r| r["status"] != "completed")
        .count();
    let status = if failed == results.len() {
        STATUS_FAILED
    } else {
        STATUS_COMPLETED
    };
    let finished = now_ms();

    let consolidated = json!({
        "delegation_id": id,
        "status": status,
        "subagents": results.len(),
        "failed": failed,
        "results": results,
    });
    let _ = std::fs::write(
        log_dir.join("result.json"),
        serde_json::to_string_pretty(&consolidated).unwrap_or_default(),
    );
    let _ = std::fs::write(log_dir.join("DONE"), format!("{}\n", status));

    if let Some(store) = store {
        let _ = store.finish_delegation(
            id,
            status,
            &serde_json::to_string(&consolidated).unwrap_or_default(),
        );
    }

    let message = format_consolidated_report(id, &consolidated);

    let mut guard = state().lock().unwrap();
    if let Some(record) = guard.records.get_mut(id) {
        record.status = status.to_string();
        record.finished_ms = Some(finished);
    }
    guard.completions.push_back(Completion {
        delegation_id: id.to_string(),
        session_key: parent_session_key.to_string(),
        message,
        result: consolidated,
        finished_ms: finished,
    });
}

/// Model-facing text for a finished delegation (hermes consolidated block).
pub fn format_consolidated_report(delegation_id: &str, consolidated: &serde_json::Value) -> String {
    let subagents = consolidated["subagents"].as_u64().unwrap_or(0);
    let failed = consolidated["failed"].as_u64().unwrap_or(0);
    let mut out = format!(
        "Background delegation {} finished: {} subagent(s), {} failed.\n\n",
        delegation_id, subagents, failed
    );
    if let Some(results) = consolidated["results"].as_array() {
        for (i, entry) in results.iter().enumerate() {
            let task = entry["task"].as_str().unwrap_or("");
            let status = entry["status"].as_str().unwrap_or("error");
            out.push_str(&format!("{}. [{}] {}\n", i + 1, status, task));
            if status == "completed" {
                let result = entry["result"].as_str().unwrap_or("");
                out.push_str(result.trim_end());
                out.push('\n');
            } else {
                out.push_str(&format!(
                    "error: {}\n",
                    entry["error"].as_str().unwrap_or("unknown")
                ));
            }
            out.push('\n');
        }
    }
    out.trim_end().to_string()
}

/// Claim all queued completions owned by `session_key` (hermes
/// positive-ownership drain). Completions for other sessions stay queued.
pub fn drain_completions(store: Option<&SqliteSessionStore>, session_key: &str) -> Vec<Completion> {
    if session_key.is_empty() {
        return Vec::new();
    }
    let mut claimed = Vec::new();
    {
        let mut guard = state().lock().unwrap();
        let mut rest = VecDeque::new();
        while let Some(completion) = guard.completions.pop_front() {
            if completion.session_key == session_key {
                if let Some(store) = store {
                    let _ = store.mark_delegation_delivered(&completion.delegation_id);
                }
                claimed.push(completion);
            } else {
                rest.push_back(completion);
            }
        }
        guard.completions = rest;
    }

    // Durability catch-up (hermes delivery claim): undelivered rows from a
    // previous process, or from a crash between finalize and drain. Rows
    // are claimed with a delivery token (incrementing delivery_attempts),
    // then completed or released:
    // - origin session gone from the store → release; after
    //   MAX_DELIVERY_ATTEMPTS failed claims the row converges to terminal
    //   `dropped` instead of replaying on every restart (hermes
    //   release_completion_delivery convergence).
    // - parse failure → release for a later retry.
    if let Some(store) = store {
        for (id, origin_session, result_json) in store.undelivered_delegations() {
            let claim_id = format!("drain:{}:{}", std::process::id(), uuid::Uuid::new_v4());
            let Ok(claimed_now) = store.claim_delegation_delivery(&id, &claim_id) else {
                continue;
            };
            if !claimed_now {
                continue; // another consumer holds the claim
            }
            // Delivery target must still exist: delegations are only
            // persisted when a store is wired, and the agent stamps its
            // session row before dispatching, so a missing origin row
            // means the session was deleted/pruned — permanently
            // undeliverable.
            let deliverable = matches!(store.get_session_row(&origin_session), Ok(Some(_)));
            if !deliverable {
                let _ = store.release_delegation_delivery(&id, &claim_id);
                continue;
            }
            let Ok(consolidated) = serde_json::from_str::<serde_json::Value>(&result_json) else {
                let _ = store.release_delegation_delivery(&id, &claim_id);
                continue;
            };
            let message = format_consolidated_report(&id, &consolidated);
            match store.complete_delegation_delivery(&id, &claim_id) {
                Ok(true) => claimed.push(Completion {
                    delegation_id: id,
                    session_key: origin_session,
                    message,
                    result: consolidated,
                    finished_ms: now_ms(),
                }),
                _ => {
                    let _ = store.release_delegation_delivery(&id, &claim_id);
                }
            }
        }
    }
    claimed
}

/// Startup recovery (hermes `recover_abandoned_delegations`): delegations
/// still marked `running` belonged to a process that died, so give them a
/// terminal `unknown` outcome with a consolidated result — the normal
/// delivery claim injects the "outcome unknown" report into the
/// conversation on the next drain. Completed-but-undelivered rows also
/// stay in the store until claimed, so a crash between recovery and the
/// first drain cannot lose results. Returns the number of abandoned
/// delegations.
pub fn recover_from_store(store: &SqliteSessionStore) -> usize {
    store.abandon_running_delegations().len()
}

/// Snapshot of all live/finished delegations in this process.
pub fn list_delegations() -> Vec<DelegationRecord> {
    let guard = state().lock().unwrap();
    let mut records: Vec<DelegationRecord> = guard.records.values().cloned().collect();
    records.sort_by(|a, b| b.created_ms.cmp(&a.created_ms));
    records
}

/// Fetch a single delegation record by id.
pub fn get_delegation(id: &str) -> Option<DelegationRecord> {
    state().lock().unwrap().records.get(id).cloned()
}

/// Read the consolidated result for a finished delegation, if present.
pub fn read_result(home: &Path, id: &str) -> Option<serde_json::Value> {
    let path = live_root(home).join(id).join("result.json");
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Humanize a millisecond duration (`5s`, `3m`, `2h`, `4d`).
fn human_ms(ms: i64) -> String {
    let secs = (ms / 1000).max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

/// Text digest of active + recently finished delegations (P666 — the
/// `/agents` slash, hermes "show active agents and running tasks").
/// Pure over a record slice so REPL and gateway share one formatter.
pub fn format_agents_digest(records: &[DelegationRecord]) -> String {
    if records.is_empty() {
        return "(o_o) no agents running and no recent delegations.\n".to_string();
    }
    let now = now_ms();
    let running: Vec<&DelegationRecord> = records
        .iter()
        .filter(|r| r.status == STATUS_RUNNING)
        .collect();
    let finished: Vec<&DelegationRecord> = records
        .iter()
        .filter(|r| r.status != STATUS_RUNNING)
        .take(10)
        .collect();

    let mut out = String::new();
    out.push_str(&format!(
        "agents: {} running, {} finished (recent)\n",
        running.len(),
        finished.len()
    ));
    if !running.is_empty() {
        out.push_str("running:\n");
        for record in &running {
            let elapsed = human_ms(now - record.created_ms);
            out.push_str(&format!(
                "  {}  {} task(s)  elapsed {}  ({})\n",
                record.id, record.tasks, elapsed, record.parent_session_key
            ));
        }
    }
    if !finished.is_empty() {
        out.push_str("recent:\n");
        for record in &finished {
            let when = record
                .finished_ms
                .map(|ms| format!("{} ago", human_ms(now - ms)))
                .unwrap_or_else(|| "unknown".to_string());
            out.push_str(&format!(
                "  {}  {:<9}  {} task(s)  finished {}\n",
                record.id, record.status, record.tasks, when
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::AgentError;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FakeRunner {
        delay_ms: u64,
        fail: bool,
        runs: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl SubAgentRunner for FakeRunner {
        async fn run_subagent(&self, goal: &str, _context: &str) -> crate::error::Result<String> {
            self.runs.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
            if self.fail {
                Err(AgentError::tool("boom"))
            } else {
                Ok(format!("answer to: {goal}"))
            }
        }
    }

    fn unique_home(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("ulnclaw-asyncdel-{}-{}", tag, uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    async fn wait_for_completion(
        store: Option<&SqliteSessionStore>,
        key: &str,
        timeout_ms: u64,
    ) -> Vec<Completion> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        loop {
            let drained = drain_completions(store, key);
            if !drained.is_empty() {
                return drained;
            }
            if std::time::Instant::now() > deadline {
                return Vec::new();
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn dispatch_runs_children_and_delivers_completion() {
        let home = unique_home("basic");
        let runner: Arc<dyn SubAgentRunner> = Arc::new(FakeRunner {
            delay_ms: 5,
            fail: false,
            runs: AtomicUsize::new(0),
        });
        let record = dispatch_background_delegation(
            runner,
            vec![
                ("goal A".to_string(), "ctx A".to_string()),
                ("goal B".to_string(), String::new()),
            ],
            "session-owner".to_string(),
            home.clone(),
            2,
            None,
        )
        .unwrap();

        let completions = wait_for_completion(None, "session-owner", 5000).await;
        assert_eq!(completions.len(), 1);
        let completion = &completions[0];
        assert_eq!(completion.delegation_id, record.id);
        assert!(completion.message.contains("goal A"));
        assert!(completion.message.contains("answer to: goal A"));
        assert!(completion.message.contains("answer to: goal B"));
        assert_eq!(completion.result["status"], "completed");
        assert_eq!(completion.result["failed"], 0);

        // Files on disk: per-task logs + result.json + DONE marker.
        let dir = live_root(&home).join(&record.id);
        assert!(dir.join("task-1.log").exists());
        assert!(dir.join("task-2.log").exists());
        assert!(dir.join("result.json").exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("DONE")).unwrap().trim(),
            "completed"
        );

        // Registry bookkeeping.
        let stored = get_delegation(&record.id).unwrap();
        assert_eq!(stored.status, STATUS_COMPLETED);
        assert!(stored.finished_ms.is_some());
        let from_disk = read_result(&home, &record.id).unwrap();
        assert_eq!(from_disk["subagents"], 2);
    }

    #[tokio::test]
    async fn failures_are_reported_and_status_failed_when_all_fail() {
        let home = unique_home("fail");
        let runner: Arc<dyn SubAgentRunner> = Arc::new(FakeRunner {
            delay_ms: 1,
            fail: true,
            runs: AtomicUsize::new(0),
        });
        let record = dispatch_background_delegation(
            runner,
            vec![("x".to_string(), String::new())],
            "session-fail".to_string(),
            home.clone(),
            1,
            None,
        )
        .unwrap();
        let completions = wait_for_completion(None, "session-fail", 5000).await;
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].result["status"], "failed");
        assert_eq!(completions[0].result["failed"], 1);
        assert!(completions[0].message.contains("boom"));
        assert_eq!(get_delegation(&record.id).unwrap().status, STATUS_FAILED);
    }

    #[tokio::test]
    async fn drain_respects_session_ownership() {
        let home = unique_home("ownership");
        let runner: Arc<dyn SubAgentRunner> = Arc::new(FakeRunner {
            delay_ms: 1,
            fail: false,
            runs: AtomicUsize::new(0),
        });
        dispatch_background_delegation(
            runner,
            vec![("only for alice".to_string(), String::new())],
            "alice".to_string(),
            home,
            1,
            None,
        )
        .unwrap();
        // Bob drains first: nothing for him, Alice's completion stays queued.
        assert!(drain_completions(None, "bob").is_empty());
        let completions = wait_for_completion(None, "alice", 5000).await;
        assert_eq!(completions.len(), 1);
        // Second drain is empty (claimed exactly once).
        assert!(drain_completions(None, "alice").is_empty());
        // Empty session key never claims anything.
        assert!(drain_completions(None, "").is_empty());
    }

    #[tokio::test]
    async fn concurrency_is_bounded() {
        let home = unique_home("bounded");
        let runner = Arc::new(FakeRunner {
            delay_ms: 60,
            fail: false,
            runs: AtomicUsize::new(0),
        });
        let runner_dyn: Arc<dyn SubAgentRunner> = runner.clone();
        let record = dispatch_background_delegation(
            runner_dyn,
            (0..4)
                .map(|i| (format!("task {i}"), String::new()))
                .collect(),
            "session-bounded".to_string(),
            home,
            2,
            None,
        )
        .unwrap();
        // Shortly after dispatch at most 2 children can have started.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(runner.runs.load(Ordering::SeqCst) <= 2);
        let completions = wait_for_completion(None, "session-bounded", 5000).await;
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].result["subagents"], 4);
        assert!(get_delegation(&record.id).is_some());
    }

    // The store-backed tests share the process-global store handle; keep
    // them serialized so drains observe the intended store.
    static STORE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[tokio::test]
    async fn durable_registry_recovery_and_delivery_claim() {
        let _guard = STORE_LOCK.lock().unwrap();
        let home = unique_home("durable");
        let store = Arc::new(SqliteSessionStore::open(home.join("state.db")).unwrap());

        // Simulate a previous process (different session key): one
        // delegation still running, one completed but never delivered.
        // The origin session row must exist — delivery is refused (and
        // eventually dropped) when the target session is gone.
        let old_sess = store.create_session("cli", None, None).unwrap();
        store
            .persist_delegation_dispatch(
                "d-abandoned",
                &old_sess,
                "[{\"goal\":\"old work\",\"context\":\"\"}]",
            )
            .unwrap();
        store
            .persist_delegation_dispatch("d-finished", &old_sess, "[]")
            .unwrap();
        store
            .finish_delegation(
                "d-finished",
                "completed",
                "{\"delegation_id\":\"d-finished\",\"status\":\"completed\",\"subagents\":1,\"failed\":0,\"results\":[{\"task\":\"t\",\"status\":\"completed\",\"result\":\"done\"}]}",
            )
            .unwrap();

        // Restart recovery: the running row becomes a terminal `unknown`
        // outcome with a consolidated result (hermes recover_abandoned).
        assert_eq!(recover_from_store(&store), 1);
        let states: std::collections::HashMap<String, String> = store
            .delegation_rows(10)
            .into_iter()
            .map(|(id, _, st, _, _, _, _)| (id, st))
            .collect();
        assert_eq!(
            states.get("d-abandoned").map(String::as_str),
            Some("unknown")
        );
        assert_eq!(
            states.get("d-finished").map(String::as_str),
            Some("completed")
        );

        // The restarted process has a NEW session key; the delivery claim
        // still hands over every pending row (single-consumer process).
        let drained = drain_completions(Some(&store), "new-sess");
        assert_eq!(drained.len(), 2);
        let lost = drained
            .iter()
            .find(|c| c.delegation_id == "d-abandoned")
            .unwrap();
        assert!(lost.message.contains("outcome unknown"));
        assert!(lost.message.contains("old work"));
        let recovered = drained
            .iter()
            .find(|c| c.delegation_id == "d-finished")
            .unwrap();
        assert!(recovered.message.contains("done"));
        // Claim is durable: nothing left undelivered, second drain empty.
        assert!(store.undelivered_delegations().is_empty());
        assert!(drain_completions(Some(&store), "new-sess").is_empty());
    }

    #[tokio::test]
    async fn dispatch_with_store_persists_and_delivers_once() {
        let _guard = STORE_LOCK.lock().unwrap();
        let home = unique_home("durable-dispatch");
        let store = Arc::new(SqliteSessionStore::open(home.join("state.db")).unwrap());
        let runner: Arc<dyn SubAgentRunner> = Arc::new(FakeRunner {
            delay_ms: 1,
            fail: false,
            runs: AtomicUsize::new(0),
        });
        let record = dispatch_background_delegation(
            runner,
            vec![("persist me".to_string(), String::new())],
            "sess-persist".to_string(),
            home,
            1,
            Some(store.clone()),
        )
        .unwrap();
        let completions = wait_for_completion(Some(&store), "sess-persist", 5000).await;
        assert_eq!(completions.len(), 1);
        assert!(completions[0].message.contains("persist me"));

        // Row persisted end-to-end: dispatch -> completed -> delivered.
        let rows = store.delegation_rows(10);
        assert_eq!(rows.len(), 1);
        let (id, origin, row_state, _, _, result_json, _) = &rows[0];
        assert_eq!(id, &record.id);
        assert_eq!(origin, "sess-persist");
        assert_eq!(row_state, "delivered");
        assert!(result_json.as_deref().unwrap_or("").contains("persist me"));
        // Delivery claim prevents any second delivery.
        assert!(store.undelivered_delegations().is_empty());
        assert!(drain_completions(Some(&store), "sess-persist").is_empty());
    }

    #[test]
    fn delivery_claim_lifecycle() {
        let _guard = STORE_LOCK.lock().unwrap();
        let home = unique_home("claim-lifecycle");
        let store = SqliteSessionStore::open(home.join("state.db")).unwrap();
        store
            .persist_delegation_dispatch("d-claim", "sess-x", "[]")
            .unwrap();
        store
            .finish_delegation("d-claim", "completed", "{\"results\":[]}")
            .unwrap();

        // Claim wins once; competing claims lose while it is held.
        assert!(store
            .claim_delegation_delivery("d-claim", "claim-a")
            .unwrap());
        assert!(!store
            .claim_delegation_delivery("d-claim", "claim-b")
            .unwrap());
        // Release clears the claim; attempts were counted at claim time.
        assert!(!store
            .release_delegation_delivery("d-claim", "claim-a")
            .unwrap());
        assert_eq!(store.delegation_delivery_attempts("d-claim"), 1);

        // Re-claim + complete under the claim token only.
        assert!(store
            .claim_delegation_delivery("d-claim", "claim-c")
            .unwrap());
        assert!(!store
            .complete_delegation_delivery("d-claim", "wrong-claim")
            .unwrap());
        assert!(store
            .complete_delegation_delivery("d-claim", "claim-c")
            .unwrap());
        let states: std::collections::HashMap<String, String> = store
            .delegation_rows(10)
            .into_iter()
            .map(|(id, _, st, _, _, _, _)| (id, st))
            .collect();
        assert_eq!(states.get("d-claim").map(String::as_str), Some("delivered"));
        // Delivered rows are not re-claimable.
        assert!(!store
            .claim_delegation_delivery("d-claim", "claim-d")
            .unwrap());

        // drop_delegation_delivery: terminal drop of a claimed row.
        store
            .persist_delegation_dispatch("d-drop", "sess-y", "[]")
            .unwrap();
        store
            .finish_delegation("d-drop", "completed", "{\"results\":[]}")
            .unwrap();
        assert!(store
            .claim_delegation_delivery("d-drop", "claim-e")
            .unwrap());
        assert!(store.drop_delegation_delivery("d-drop", "claim-e").unwrap());
        assert!(store
            .undelivered_delegations()
            .iter()
            .all(|(id, _, _)| id != "d-drop"));
        assert!(!store
            .claim_delegation_delivery("d-drop", "claim-f")
            .unwrap());
    }

    #[tokio::test]
    async fn undeliverable_completion_converges_to_dropped() {
        let _guard = STORE_LOCK.lock().unwrap();
        let home = unique_home("dropped");
        let store = Arc::new(SqliteSessionStore::open(home.join("state.db")).unwrap());
        // Finished delegation whose origin session never existed in this
        // store (deleted/pruned session): permanently undeliverable.
        store
            .persist_delegation_dispatch("d-orphan", "ghost-sess", "[]")
            .unwrap();
        store
            .finish_delegation(
                "d-orphan",
                "completed",
                "{\"delegation_id\":\"d-orphan\",\"status\":\"completed\",\"subagents\":1,\"failed\":0,\"results\":[]}",
            )
            .unwrap();

        // Each drain claims (attempts++) and releases — nothing delivered.
        for expected_attempts in 1..=7i64 {
            assert!(drain_completions(Some(&store), "new-sess").is_empty());
            assert_eq!(
                store.delegation_delivery_attempts("d-orphan"),
                expected_attempts
            );
            assert_eq!(
                store.undelivered_delegations().len(),
                1,
                "still pending before the budget is exhausted"
            );
        }
        // The 8th attempt exhausts the budget → terminal `dropped`.
        assert!(drain_completions(Some(&store), "new-sess").is_empty());
        assert_eq!(store.delegation_delivery_attempts("d-orphan"), 8);
        assert!(
            store.undelivered_delegations().is_empty(),
            "dropped rows never replay"
        );
        let states: std::collections::HashMap<String, String> = store
            .delegation_rows(10)
            .into_iter()
            .map(|(id, _, st, _, _, _, _)| (id, st))
            .collect();
        assert_eq!(states.get("d-orphan").map(String::as_str), Some("dropped"));
        // Further drains stay empty forever (no replay across restarts).
        assert!(drain_completions(Some(&store), "new-sess").is_empty());
    }

    #[test]
    fn report_format() {
        let consolidated = json!({
            "delegation_id": "abc",
            "status": "completed",
            "subagents": 2,
            "failed": 1,
            "results": [
                {"task": "t1", "status": "completed", "result": "done 1"},
                {"task": "t2", "status": "error", "error": "bad"},
            ],
        });
        let report = format_consolidated_report("abc", &consolidated);
        assert!(report.contains("Background delegation abc finished: 2 subagent(s), 1 failed."));
        assert!(report.contains("1. [completed] t1"));
        assert!(report.contains("done 1"));
        assert!(report.contains("2. [error] t2"));
        assert!(report.contains("error: bad"));
    }

    #[test]
    fn agents_digest_empty_and_populated() {
        let out = format_agents_digest(&[]);
        assert!(out.contains("no agents running"), "{out}");

        let now = now_ms();
        let records = vec![
            DelegationRecord {
                id: "del_running".into(),
                parent_session_key: "sess-1".into(),
                tasks: 3,
                status: STATUS_RUNNING.to_string(),
                created_ms: now - 120_000,
                finished_ms: None,
                log_dir: PathBuf::from("/tmp/x"),
            },
            DelegationRecord {
                id: "del_done".into(),
                parent_session_key: "sess-2".into(),
                tasks: 2,
                status: STATUS_COMPLETED.to_string(),
                created_ms: now - 600_000,
                finished_ms: Some(now - 60_000),
                log_dir: PathBuf::from("/tmp/y"),
            },
        ];
        let out = format_agents_digest(&records);
        assert!(out.contains("1 running"), "{out}");
        assert!(out.contains("del_running"), "{out}");
        assert!(out.contains("elapsed 2m"), "{out}");
        assert!(out.contains("del_done"), "{out}");
        assert!(out.contains("1m ago"), "{out}");
    }
}

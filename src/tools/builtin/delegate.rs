//! delegate_task — aligned with hermes' tools/delegate_tool.py
//!
//! Spawns one or more sub-agents in isolated contexts. The actual agent
//! execution is provided by the `SubAgentRunner` installed on the context
//! (implemented by `Agent`), keeping tools/ free of an agent dependency.
//!
//! Background semantics (hermes v2026.8.3): top-level delegations run in
//! the background automatically — dispatch returns immediately with a
//! delegation id and live-log paths, and ONE consolidated result re-enters
//! the conversation when all children finish (see `async_delegation`).
//! Orchestrator subagents (depth > 0) stay synchronous because they need
//! their workers' results within their own turn. Sessions that cannot
//! receive detached results (one-shot runners) fall back to synchronous
//! execution with an explanatory note.

use crate::tools::{tool, ToolRegistry};
use serde_json::json;

pub fn register(registry: &mut ToolRegistry) {
    registry.register(delegate_task_tool());
}

fn delegate_task_tool() -> crate::tools::Tool {
    tool("delegate_task")
        .description(
            "Spawn subagents in isolated contexts; each gets its own conversation and only its \
             final summary returns to you. Provide 'goal' for a single task or 'tasks' for a \
             parallel batch.\n\nRuns in the background: dispatch returns immediately with live \
             transcript paths, and the completed result (one consolidated message for a batch) \
             re-enters the conversation on its own. Do NOT wait or poll; continue other work.\n\n\
             USE FOR: reasoning-heavy subtasks, work that would flood your context with \
             intermediate data, or independent parallel workstreams.\n\
             DO NOT USE FOR: mechanical multi-step work (use execute_code), a single tool call \
             (call the tool directly), tasks needing user interaction (subagents cannot ask \
             questions), or durable work that must survive this session (use cronjob or \
             terminal background).\n\n\
             RULES: children know nothing of this conversation — pass everything needed via \
             'context'. Child summaries are SELF-REPORTS, not verified facts: verify external \
             side effects (fetch the URL, read back the file) before reporting success.",
        )
        .parameters(json!({
            "type": "object",
            "properties": {
                "goal": {"type": "string", "description": "What the subagent should accomplish. Be specific and self-contained."},
                "context": {"type": "string", "description": "Background information the subagent needs: file paths, error messages, project structure, constraints."},
                "tasks": {
                    "type": "array",
                    "description": "Multiple tasks to run as parallel subagents (alternative to single goal).",
                    "items": {
                        "type": "object",
                        "properties": {
                            "goal": {"type": "string", "description": "Task goal"},
                            "context": {"type": "string", "description": "Task-specific context"}
                        },
                        "required": ["goal"]
                    }
                },
                "background": {
                    "type": "boolean",
                    "description": "DEPRECATED / IGNORED. Top-level single and batch delegations run in the background automatically — you do not need to (and cannot) opt in or out. A single result or consolidated batch result re-enters the conversation when the work finishes; just continue working in the meantime."
                }
            },
            "required": []
        }))
        .handler(|args, ctx| async move {
            let Some(runner) = ctx.subagent_runner() else {
                return Ok(json!({
                    "success": false,
                    "error": "Delegation is not available in this run (no sub-agent runner wired)."
                }));
            };

            let max_children = ctx.config.delegation.max_concurrent_children.max(1);

            // Collect (goal, context) pairs.
            let mut tasks: Vec<(String, String)> = Vec::new();
            if let Some(list) = args.get("tasks").and_then(|v| v.as_array()) {
                for item in list {
                    let goal = item.get("goal").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    if goal.is_empty() {
                        continue;
                    }
                    let context = item.get("context").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    tasks.push((goal, context));
                }
            }
            if tasks.is_empty() {
                let goal = args.get("goal").and_then(|v| v.as_str()).unwrap_or("").to_string();
                if goal.is_empty() {
                    return Ok(json!({"success": false, "error": "delegate_task: provide 'goal' or 'tasks'"}));
                }
                let context = args.get("context").and_then(|v| v.as_str()).unwrap_or("").to_string();
                tasks.push((goal, context));
            }
            if tasks.len() > max_children {
                return Ok(json!({
                    "success": false,
                    "error": format!(
                        "delegation.max_concurrent_children is {} but {} tasks were requested. Reduce the task count.",
                        max_children, tasks.len()
                    )
                }));
            }

            // Hermes `_model_background_value`: top-level delegations always
            // run in the background; orchestrator children (depth > 0) need
            // their workers' results within the same turn and stay sync.
            let wants_background = ctx.delegate_depth() == 0;

            if wants_background && ctx.async_delivery() {
                let goals: Vec<String> = tasks.iter().map(|(g, _)| g.clone()).collect();
                match crate::async_delegation::dispatch_background_delegation(
                    runner,
                    tasks,
                    ctx.session_id.clone(),
                    ctx.home.clone(),
                    max_children,
                    ctx.store.clone(),
                ) {
                    Ok(record) => {
                        let n = goals.len();
                        let note = if n == 1 {
                            "Subagent is running in the background. You and the user can keep \
                             working; its full result re-enters the conversation as a new \
                             message when it finishes. Do not wait or poll — just continue."
                                .to_string()
                        } else {
                            format!(
                                "{n} subagents are running in parallel in the background. You \
                                 and the user can keep working; their consolidated results \
                                 re-enter the conversation as a single message once ALL of them \
                                 finish. Do not wait or poll — just continue."
                            )
                        };
                        let live_transcripts: Vec<String> = (1..=n)
                            .map(|i| record.log_dir.join(format!("task-{i}.log")).display().to_string())
                            .collect();
                        return Ok(json!({
                            "status": "dispatched",
                            "mode": "background",
                            "count": n,
                            "delegation_id": record.id,
                            "goals": goals,
                            "live_transcripts": live_transcripts,
                            "live_transcripts_hint": "Each subagent writes its result to the file listed above. Read or tail these paths at any time to watch a child work while it runs.",
                            "note": note,
                        }));
                    }
                    Err(e) => {
                        return Ok(json!({
                            "success": false,
                            "error": format!("background delegation dispatch failed: {e}")
                        }));
                    }
                }
            }

            // Synchronous execution: orchestrator subagents, or top-level on
            // sessions that cannot receive detached results (hermes forced
            // sync fallback for one-shot/stateless runners).
            let mut handles = Vec::new();
            for (goal, context) in tasks.clone() {
                let runner = runner.clone();
                handles.push(tokio::spawn(async move {
                    runner.run_subagent(&goal, &context).await
                }));
            }

            let mut results = Vec::new();
            let mut failed = 0usize;
            for (idx, handle) in handles.into_iter().enumerate() {
                let (goal, _) = &tasks[idx];
                match handle.await {
                    Ok(Ok(answer)) => results.push(json!({
                        "task": goal,
                        "status": "completed",
                        "result": answer,
                    })),
                    Ok(Err(e)) => {
                        failed += 1;
                        results.push(json!({"task": goal, "status": "error", "error": e.to_string()}));
                    }
                    Err(e) => {
                        failed += 1;
                        results.push(json!({"task": goal, "status": "error", "error": format!("join: {}", e)}));
                    }
                }
            }

            let mut payload = json!({
                "success": failed == 0,
                "subagents": results.len(),
                "failed": failed,
                "results": results,
            });
            if wants_background && !ctx.async_delivery() {
                payload["note"] = json!(
                    "Background delegation is not available in this session — it cannot receive \
                     a detached subagent result after the turn ends (a one-shot runner such as \
                     `ulnclaw run`, a cron job, or a stateless HTTP endpoint). The subagent(s) \
                     ran SYNCHRONOUSLY and the result is included above."
                );
            }
            Ok(payload)
        })
        .toolset("delegation")
        .emoji("🤝")
        .build()
        .expect("delegate_task builds")
}

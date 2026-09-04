//! cronjob — port of hermes' tools/cronjob_tools.py
//!
//! One compressed tool managing scheduled jobs: create/list/update/pause/
//! resume/remove/run. Jobs run in fresh sessions; the final response is
//! delivered to the configured target.

use crate::cron::{next_run, parse_schedule, CronJob, CronStore, Schedule};
use crate::error::Result;
use crate::tools::{tool, ToolContext, ToolRegistry};
use serde_json::json;

pub fn register(registry: &mut ToolRegistry) {
    registry.register(cronjob_tool());
}

fn open_store(ctx: &ToolContext) -> Result<CronStore> {
    CronStore::open(&ctx.home.join("state.db"))
}

fn job_to_json(job: &CronJob) -> serde_json::Value {
    json!({
        "job_id": job.id,
        "name": job.name,
        "schedule": job.schedule,
        "prompt": job.prompt,
        "skills": job.skills,
        "enabled": job.enabled,
        "repeat": job.repeat,
        "next_run": job.next_run.map(|t| chrono::DateTime::from_timestamp(t as i64, 0).map(|d| d.to_rfc3339()).unwrap_or_default()),
        "last_run": job.last_run.map(|t| chrono::DateTime::from_timestamp(t as i64, 0).map(|d| d.to_rfc3339()).unwrap_or_default()),
        "last_status": job.last_status,
        "deliver": job.deliver,
        "origin": job.origin,
        "last_delivery_error": job.last_delivery_error,
        "attach_to_session": job.attach_to_session,
    })
}

fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn cronjob_tool() -> crate::tools::Tool {
    tool("cronjob")
        .description(
            "Manage scheduled cron jobs with a single compressed tool.\n\n\
             Use action='create' to schedule a new job from a prompt or one or more skills.\n\
             Use action='list' to inspect jobs.\n\
             Use action='update', 'pause', 'resume', 'remove', or 'run' to manage an existing job.\n\n\
             To stop a job the user no longer wants: first action='list' to find the job_id, \
             then action='remove' with that job_id. Never guess job IDs — always list first.\n\n\
             Jobs run in a fresh session with no current-chat context, so prompts must be \
             self-contained. If skills are provided on create, the future cron run loads those \
             skills in order, then follows the prompt as the task instruction.\n\n\
             NOTE: The agent's final response is auto-delivered to the target. Cron jobs run \
             autonomously with no user present — they cannot ask questions.\n\n\
             Important safety rule: cron-run sessions should not recursively schedule more cron jobs.",
        )
        .parameters(json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "description": "One of: create, list, update, pause, resume, remove, run. When action=create, the 'schedule' and 'prompt' fields are REQUIRED."},
                "job_id": {"type": "string", "description": "Required for update/pause/resume/remove/run"},
                "prompt": {"type": "string", "description": "For create: the full self-contained prompt."},
                "schedule": {"type": "string", "description": "REQUIRED for action=create. '30m', 'every 2h', '0 9 * * *', or ISO timestamp (one-shot)."},
                "name": {"type": "string", "description": "Optional human-friendly name"},
                "skills": {"type": "array", "items": {"type": "string"}, "description": "Skills to load before the prompt when the job runs"},
                "repeat": {"type": "integer", "description": "Optional repeat count. Omit for defaults (once for one-shot, forever for recurring)."},
                "deliver": {"type": "string", "description": "Where the final response is auto-delivered: 'origin' (the chat the job was created in, the default when created from a chat), 'local' (save only), a platform name ('telegram', 'discord', ...), 'platform:chat_id[:thread_id]', or a comma mix incl. 'all'. Defaults to origin when created inside a chat, else local."},
                "attach_to_session": {"type": "boolean", "description": "Optional per-job delivery-mirror override: when true, each successful delivery to the job's origin chat is also appended to that chat's session transcript, so the next reply there sees the cron output in context. When false, mirroring is forced off for this job. Omit to follow the global cron.mirror_delivery setting (default off)."}
            },
            "required": ["action"]
        }))
        .handler(|args, ctx| async move {
            let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
            let store = match open_store(&ctx) {
                Ok(store) => store,
                Err(e) => return Ok(json!({"success": false, "error": format!("cron store: {}", e)})),
            };
            match action {
                "create" => {
                    let schedule_raw = args.get("schedule").and_then(|v| v.as_str()).unwrap_or("");
                    let prompt = args.get("prompt").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    if schedule_raw.is_empty() || prompt.is_empty() {
                        return Ok(json!({"success": false, "error": "cronjob create requires both 'schedule' and 'prompt'"}));
                    }
                    let schedule = match parse_schedule(schedule_raw) {
                        Ok(s) => s,
                        Err(e) => return Ok(json!({"success": false, "error": e.to_string()})),
                    };
                    let skills: Vec<String> = args.get("skills").and_then(|v| v.as_array())
                        .map(|arr| arr.iter().filter_map(|s| s.as_str().map(String::from)).collect())
                        .unwrap_or_default();
                    let repeat = args.get("repeat").and_then(|v| v.as_i64());
                    let repeat = match (&schedule, repeat) {
                        (Schedule::OneShot(_), None) => Some(1),
                        (_, r) => r,
                    };
                    // Capture where the job was created so `deliver =
                    // "origin"` can route the final response back to the
                    // requesting chat (hermes cronjob origin capture).
                    let origin = crate::messaging::current_messaging_ctx().map(|ctx| {
                        crate::cron::JobOrigin {
                            platform: ctx.platform,
                            chat_id: ctx.chat_id,
                            thread_id: None,
                        }
                    });
                    // Default delivery to origin when available,
                    // otherwise local (hermes `create_job` default).
                    let deliver = match args.get("deliver").and_then(|v| v.as_str()) {
                        Some(explicit) => crate::cron::delivery::normalize_deliver_value(Some(
                            &serde_json::Value::String(explicit.to_string()),
                        )),
                        None => {
                            if origin.is_some() {
                                "origin".to_string()
                            } else {
                                "local".to_string()
                            }
                        }
                    };
                    let job = CronJob {
                        id: format!("cron-{}", &uuid::Uuid::new_v4().to_string()[..8]),
                        name: args.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                        schedule: schedule_raw.to_string(),
                        prompt,
                        skills,
                        enabled: true,
                        repeat,
                        next_run: next_run(&schedule),
                        created_at: now(),
                        last_run: None,
                        last_status: None,
                        deliver: Some(deliver),
                        origin,
                        last_delivery_error: None,
                        attach_to_session: args.get("attach_to_session").and_then(|v| v.as_bool()),
                    };
                    if let Err(e) = store.add(&job) {
                        return Ok(json!({"success": false, "error": e.to_string()}));
                    }
                    Ok(json!({
                        "success": true,
                        "action": "create",
                        "job": job_to_json(&job),
                        "note": "Job scheduled. It runs in a fresh session; the final response is delivered automatically.",
                    }))
                }
                "list" => {
                    let jobs = match store.list() {
                        Ok(jobs) => jobs,
                        Err(e) => return Ok(json!({"success": false, "error": e.to_string()})),
                    };
                    Ok(json!({
                        "success": true,
                        "jobs": jobs.iter().map(job_to_json).collect::<Vec<_>>(),
                        "count": jobs.len(),
                    }))
                }
                "update" | "pause" | "resume" | "remove" | "run" => {
                    let Some(job_id) = args.get("job_id").and_then(|v| v.as_str()) else {
                        return Ok(json!({"success": false, "error": format!("cronjob {} requires 'job_id'", action)}));
                    };
                    let job_found = match store.get(job_id) {
                        Ok(Some(job)) => Some(job),
                        Ok(None) => None,
                        Err(e) => return Ok(json!({"success": false, "error": e.to_string()})),
                    };
                    let Some(mut job) = job_found else {
                        return Ok(json!({"success": false, "error": format!("No job with id '{}'. Use action='list' to see jobs.", job_id)}));
                    };
                    match action {
                        "update" => {
                            if let Some(schedule_raw) = args.get("schedule").and_then(|v| v.as_str()) {
                                match parse_schedule(schedule_raw) {
                                    Ok(schedule) => {
                                        job.schedule = schedule_raw.to_string();
                                        job.next_run = next_run(&schedule);
                                    }
                                    Err(e) => return Ok(json!({"success": false, "error": e.to_string()})),
                                }
                            }
                            if let Some(prompt) = args.get("prompt").and_then(|v| v.as_str()) {
                                job.prompt = prompt.to_string();
                            }
                            if let Some(name) = args.get("name").and_then(|v| v.as_str()) {
                                job.name = name.to_string();
                            }
                            if args.get("skills").is_some() {
                                job.skills = args.get("skills").and_then(|v| v.as_array())
                                    .map(|arr| arr.iter().filter_map(|s| s.as_str().map(String::from)).collect())
                                    .unwrap_or_default();
                            }
                            if let Some(repeat) = args.get("repeat").and_then(|v| v.as_i64()) {
                                job.repeat = Some(repeat);
                            }
                            if let Some(deliver) = args.get("deliver").and_then(|v| v.as_str()) {
                                job.deliver = Some(crate::cron::delivery::normalize_deliver_value(
                                    Some(&serde_json::Value::String(deliver.to_string())),
                                ));
                            }
                            if let Some(flag) = args.get("attach_to_session").and_then(|v| v.as_bool()) {
                                job.attach_to_session = Some(flag);
                            }
                            store.update(&job).ok();
                            Ok(json!({"success": true, "action": "update", "job": job_to_json(&job)}))
                        }
                        "pause" => {
                            job.enabled = false;
                            store.update(&job).ok();
                            Ok(json!({"success": true, "action": "pause", "job_id": job_id}))
                        }
                        "resume" => {
                            job.enabled = true;
                            if let Ok(schedule) = parse_schedule(&job.schedule) {
                                job.next_run = next_run(&schedule);
                            }
                            store.update(&job).ok();
                            Ok(json!({"success": true, "action": "resume", "job_id": job_id}))
                        }
                        "remove" => {
                            store.remove(job_id).ok();
                            Ok(json!({"success": true, "action": "remove", "job_id": job_id}))
                        }
                        "run" => {
                            let Some(runner) = ctx.cron_runner() else {
                                return Ok(json!({"success": false, "error": "Immediate run is not available in this run (no cron runner wired)."}));
                            };
                            let prompt = job.prompt.clone();
                            let skills = job.skills.clone();
                            match runner.run_prompt(&prompt, &skills).await {
                                Ok(answer) => {
                                    job.last_run = Some(now());
                                    job.last_status = Some("ok (manual run)".into());
                                    store.update(&job).ok();
                                    Ok(json!({"success": true, "action": "run", "job_id": job_id, "result": answer}))
                                }
                                Err(e) => {
                                    job.last_run = Some(now());
                                    job.last_status = Some(format!("error: {}", e));
                                    store.update(&job).ok();
                                    Ok(json!({"success": false, "error": format!("run failed: {}", e)}))
                                }
                            }
                        }
                        _ => unreachable!(),
                    }
                }
                other => Ok(json!({"success": false, "error": format!("Unknown action '{}' (use create/list/update/pause/resume/remove/run)", other)})),
            }
        })
        .toolset("cronjob")
        .emoji("⏰")
        .build()
        .expect("cronjob builds")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn test_create_list_remove() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Arc::new(ToolContext::new().with_home(dir.path()));
        let tool = cronjob_tool();

        let result = (tool.handler)(
            json!({"action": "create", "schedule": "every 2h", "prompt": "Check the build", "name": "build-check"}),
            ctx.clone(),
        )
        .await
        .unwrap();
        assert_eq!(result["success"], json!(true));
        let job_id = result["job"]["job_id"].as_str().unwrap().to_string();

        let result = (tool.handler)(json!({"action": "list"}), ctx.clone())
            .await
            .unwrap();
        assert_eq!(result["count"], json!(1));

        let result = (tool.handler)(json!({"action": "pause", "job_id": job_id}), ctx.clone())
            .await
            .unwrap();
        assert_eq!(result["success"], json!(true));

        let result = (tool.handler)(json!({"action": "remove", "job_id": job_id}), ctx)
            .await
            .unwrap();
        assert_eq!(result["success"], json!(true));
    }

    #[tokio::test]
    async fn test_create_requires_schedule_and_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Arc::new(ToolContext::new().with_home(dir.path()));
        let tool = cronjob_tool();
        let result = (tool.handler)(json!({"action": "create", "prompt": "x"}), ctx)
            .await
            .unwrap();
        assert_eq!(result["success"], json!(false));
    }
}

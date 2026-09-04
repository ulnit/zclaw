//! Batch runner (P262) — port of hermes `batch_runner.py`: parallel batch
//! processing for running the agent across many prompts from a JSONL
//! dataset, with checkpointing for fault tolerance/resumption, trajectory
//! saving in the hermes from/value training format, and aggregated tool
//! usage statistics.
//!
//! Usage:
//! ```text
//! ulnclaw batch --dataset-file data.jsonl --batch-size 10 --run-name my_run
//! ulnclaw batch --dataset-file data.jsonl --run-name my_run --resume
//! ```

use futures::stream::StreamExt;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// CLI options (hermes `batch_runner.py` flags).
pub struct BatchOptions {
    pub dataset_file: PathBuf,
    pub batch_size: usize,
    pub run_name: String,
    pub num_workers: usize,
    pub resume: bool,
    pub verbose: bool,
    pub max_iterations: Option<usize>,
    pub model: Option<String>,
}

// ---------------------------------------------------------------------------
// Dataset + checkpoint (hermes `_load_dataset` / `_load_checkpoint`)
// ---------------------------------------------------------------------------

fn load_dataset(path: &Path) -> Result<Vec<Value>, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read dataset {}: {e}", path.display()))?;
    let mut entries = Vec::new();
    for (lineno, line) in raw.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(line).map_err(|e| {
            format!(
                "invalid JSON on line {} of {}: {e}",
                lineno + 1,
                path.display()
            )
        })?;
        if value
            .get("prompt")
            .and_then(Value::as_str)
            .unwrap_or("")
            .is_empty()
        {
            return Err(format!(
                "line {} of {} is missing a 'prompt' field",
                lineno + 1,
                path.display()
            ));
        }
        entries.push(value);
    }
    Ok(entries)
}

fn default_checkpoint(run_name: &str) -> Value {
    json!({
        "run_name": run_name,
        "completed_prompts": [],
        "batch_stats": {},
        "last_updated": null,
    })
}

fn load_checkpoint(path: &Path, run_name: &str) -> Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .filter(|value| value.get("run_name").and_then(Value::as_str) == Some(run_name))
        .unwrap_or_else(|| default_checkpoint(run_name))
}

fn save_checkpoint(path: &Path, checkpoint: &Value) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let tmp = path.with_extension("json.tmp");
    if let Ok(raw) = serde_json::to_string_pretty(checkpoint) {
        if std::fs::write(&tmp, raw).is_ok() {
            std::fs::rename(&tmp, path).ok();
        }
    }
}

/// hermes `_scan_completed_prompts_by_content`: collect the human prompts
/// already present in saved batch files so resume can skip them even when
/// the checkpoint lagged.
fn scan_completed_prompts_by_content(output_dir: &Path) -> std::collections::BTreeSet<String> {
    let mut done = std::collections::BTreeSet::new();
    let Ok(read_dir) = std::fs::read_dir(output_dir) else {
        return done;
    };
    for entry in read_dir.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("batch_") || !name.ends_with(".json") {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(results) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        for result in results.as_array().unwrap_or(&Vec::new()) {
            if let Some(prompt) = result.get("prompt").and_then(Value::as_str) {
                done.insert(prompt.to_string());
            }
        }
    }
    done
}

// ---------------------------------------------------------------------------
// Agent construction (non-interactive, no persistence — hermes batch
// workers run with save_trajectories=False / skip_memory=True)
// ---------------------------------------------------------------------------

async fn build_batch_agent(opts: &BatchOptions) -> Result<Arc<crate::agent::Agent>, String> {
    let mut config = crate::config::UlncLawConfig::load(None).unwrap_or_default();
    if let Some(model) = &opts.model {
        config.model.model = model.clone();
    }
    let api_key = config.resolve_api_key();
    let base_url = config.resolve_base_url();
    let provider: Arc<dyn crate::provider::Provider> = if config.model.provider == "anthropic" {
        let mut builder = crate::provider::anthropic::AnthropicProvider::builder()
            .endpoint(&base_url)
            .model(&config.model.model)
            .name(&config.model.provider)
            .max_retries(config.model.max_retries);
        if let Some(ref key) = api_key {
            builder = builder.api_key(key);
        }
        Arc::new(builder.build().map_err(|e| e.to_string())?)
    } else {
        let mut builder = crate::provider::openai::OpenAiProvider::builder()
            .endpoint(&base_url)
            .model(&config.model.model)
            .name(&config.model.provider)
            .max_retries(config.model.max_retries);
        if let Some(ref key) = api_key {
            builder = builder.api_key(key);
        }
        Arc::new(builder.build().map_err(|e| e.to_string())?)
    };

    let mut registry = crate::tools::ToolRegistry::new();
    crate::tools::builtin::register_builtin_tools(&mut registry);
    crate::toolsets::apply_toolset_policy(
        &mut registry,
        &config.enabled_toolsets,
        &config.disabled_toolsets,
    );
    let home = crate::config::ulnclaw_home();
    let context = crate::tools::context::ToolContext::new()
        .with_home(home)
        .with_config(config.clone())
        .with_provider(provider.clone());
    context.set_tool_definitions(registry.definitions());

    let agent =
        crate::agent::Agent::new(provider, registry).with_config(crate::agent::AgentConfig {
            max_iterations: opts.max_iterations.unwrap_or(config.agent.max_iterations),
            concurrent_tool_execution: config.agent.concurrent_tool_execution,
            max_concurrent_tools: config.agent.max_concurrent_tools,
            approval: false, // autonomous dataset runs (hermes batch semantics)
            context_budget_tokens: config.agent.context_budget_tokens,
            persist: false, // trajectories are saved by the runner itself
            source: "batch".to_string(),
            environment_probe: false,
            terminal_backend: config
                .terminal
                .backend
                .clone()
                .unwrap_or_else(|| "local".to_string()),
            ..Default::default()
        });
    let agent = agent
        .with_tool_context(context)
        .with_fallback_specs(&config.model.fallbacks);
    let agent = Arc::new(agent);
    agent.wire_runners();
    Ok(agent)
}

// ---------------------------------------------------------------------------
// Stats + trajectory conversion (hermes `_extract_tool_stats` /
// `_extract_reasoning_stats` / `convert_to_trajectory_format`)
// ---------------------------------------------------------------------------

/// hermes `_extract_tool_stats`: per-tool count/success/failure with JSON
/// error-shape detection.
pub fn extract_tool_stats(messages: &[crate::provider::Message]) -> Value {
    use crate::provider::Role;
    let mut stats: BTreeMap<String, (u64, u64, u64)> = BTreeMap::new();
    let mut calls_map: std::collections::HashMap<String, String> = std::collections::HashMap::new();

    for message in messages {
        match message.role {
            Role::Assistant => {
                for call in message.tool_calls.clone().unwrap_or_default() {
                    let entry = stats.entry(call.function.name.clone()).or_insert((0, 0, 0));
                    entry.0 += 1;
                    calls_map.insert(call.id.clone(), call.function.name.clone());
                }
            }
            Role::Tool => {
                let content = message.content.clone().unwrap_or_default();
                let is_success = tool_result_is_success(&content);
                if let Some(name) = message
                    .tool_call_id
                    .as_ref()
                    .and_then(|id| calls_map.get(id))
                {
                    if let Some(entry) = stats.get_mut(name) {
                        if is_success {
                            entry.1 += 1;
                        } else {
                            entry.2 += 1;
                        }
                    }
                }
            }
            _ => {}
        }
    }

    let mut out = serde_json::Map::new();
    for (name, (count, success, failure)) in stats {
        out.insert(
            name,
            json!({"count": count, "success": success, "failure": failure}),
        );
    }
    Value::Object(out)
}

fn tool_result_is_success(content: &str) -> bool {
    let trimmed = content.trim();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
            if let Some(object) = value.as_object() {
                if object
                    .get("error")
                    .map(|error| !error.is_null())
                    .unwrap_or(false)
                {
                    return false;
                }
                if let Some(inner) = object.get("content").and_then(Value::as_object) {
                    if inner
                        .get("error")
                        .map(|error| !error.is_null())
                        .unwrap_or(false)
                    {
                        return false;
                    }
                }
                if object.get("success") == Some(&json!(false)) {
                    return false;
                }
            }
            return true;
        }
    }
    if trimmed.is_empty() {
        return false;
    }
    !trimmed.to_lowercase().starts_with("error:")
}

/// hermes `_extract_reasoning_stats` (ulnclaw stores no separate reasoning
/// field — scratchpad detection still applies).
pub fn extract_reasoning_stats(messages: &[crate::provider::Message]) -> Value {
    use crate::provider::Role;
    let mut total = 0u64;
    let mut with_reasoning = 0u64;
    for message in messages {
        if message.role != Role::Assistant {
            continue;
        }
        total += 1;
        let content = message.content.clone().unwrap_or_default();
        if content.contains("<REASONING_SCRATCHPAD>") || content.contains("<think>") {
            with_reasoning += 1;
        }
    }
    json!({
        "total_assistant_turns": total,
        "turns_with_reasoning": with_reasoning,
        "turns_without_reasoning": total.saturating_sub(with_reasoning),
        "has_any_reasoning": with_reasoning > 0,
    })
}

/// hermes `convert_to_trajectory_format`: from/value training pairs with
/// `<tool_call>` / `<tool_response>` XML wrappers.
pub fn convert_to_trajectory(
    messages: &[crate::provider::Message],
    user_query: &str,
    tool_definitions_json: &str,
) -> Vec<Value> {
    use crate::provider::Role;
    let mut trajectory = Vec::new();

    let system_msg = messages
        .iter()
        .find(|message| message.role == Role::System)
        .and_then(|message| message.content.clone())
        .unwrap_or_else(|| {
            format!(
                "You are a function calling AI model. Available tools:\n<tools>\n{tool_definitions_json}\n</tools>"
            )
        });
    trajectory.push(json!({"from": "system", "value": system_msg}));
    trajectory.push(json!({"from": "human", "value": user_query}));

    let mut i = 0;
    // Skip the system prompt + the first user turn (already emitted above).
    while i < messages.len() {
        let is_first_user =
            messages[i].role == Role::User && messages[..i].iter().all(|m| m.role == Role::System);
        if is_first_user {
            i += 1;
            break;
        }
        i += 1;
    }

    while i < messages.len() {
        let message = &messages[i];
        match message.role {
            Role::Assistant => {
                let mut content = String::new();
                if let Some(text) = message.content.as_ref().filter(|t| !t.trim().is_empty()) {
                    content.push_str(text);
                    content.push('\n');
                }
                let tool_calls = message.tool_calls.clone().unwrap_or_default();
                for call in &tool_calls {
                    let arguments: Value =
                        serde_json::from_str(&call.function.arguments).unwrap_or(json!({}));
                    content.push_str(&format!(
                        "<tool_call>\n{}\n</tool_call>\n",
                        json!({"name": call.function.name, "arguments": arguments})
                    ));
                }
                if !tool_calls.is_empty() {
                    // Gather the subsequent tool responses (hermes pairs
                    // them with the calling turn).
                    let mut j = i + 1;
                    let mut call_index = 0;
                    while j < messages.len() && messages[j].role == Role::Tool {
                        let tool_msg = &messages[j];
                        let tool_content = tool_msg.content.clone().unwrap_or_default();
                        let parsed: Value = if tool_content.trim().starts_with('{')
                            || tool_content.trim().starts_with('[')
                        {
                            serde_json::from_str(&tool_content)
                                .unwrap_or(Value::String(tool_content.clone()))
                        } else {
                            Value::String(tool_content)
                        };
                        let tool_name = tool_calls
                            .get(call_index)
                            .map(|call| call.function.name.clone())
                            .unwrap_or_else(|| "unknown".to_string());
                        content.push_str(&format!(
                            "<tool_response>\n{}\n</tool_response>\n",
                            json!({
                                "tool_call_id": tool_msg.tool_call_id.clone().unwrap_or_default(),
                                "name": tool_name,
                                "content": parsed,
                            })
                        ));
                        call_index += 1;
                        j += 1;
                    }
                    i = j;
                } else {
                    i += 1;
                }
                if !content.trim().is_empty() {
                    trajectory.push(json!({
                        "from": "gpt",
                        "value": content.trim_end().to_string(),
                    }));
                }
            }
            Role::User => {
                if let Some(text) = message.content.as_ref().filter(|t| !t.trim().is_empty()) {
                    trajectory.push(json!({"from": "human", "value": text}));
                }
                i += 1;
            }
            Role::Tool => {
                // Orphan tool response (no matching assistant turn) — keep
                // it visible instead of dropping.
                if let Some(text) = message.content.as_ref().filter(|t| !t.trim().is_empty()) {
                    trajectory.push(json!({"from": "tool", "value": text}));
                }
                i += 1;
            }
            Role::System => i += 1,
        }
    }
    trajectory
}

// ---------------------------------------------------------------------------
// Runner
// ---------------------------------------------------------------------------

fn iso_now() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string()
}

async fn process_single_prompt(
    agent: &crate::agent::Agent,
    prompt_index: usize,
    prompt_data: &Value,
    batch_num: usize,
    model: &str,
    verbose: bool,
) -> Value {
    let prompt = prompt_data
        .get("prompt")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if verbose {
        eprintln!("   Prompt {prompt_index}: starting (batch {batch_num})");
    }
    match agent.run(&prompt, None).await {
        Ok(result) => {
            let tool_definitions_json = serde_json::to_string(
                &agent
                    .context()
                    .tool_registry_snapshot()
                    .iter()
                    .map(|def| serde_json::to_value(def).unwrap_or(json!({})))
                    .collect::<Vec<_>>(),
            )
            .unwrap_or_else(|_| "[]".to_string());
            let trajectory =
                convert_to_trajectory(&result.conversation, &prompt, &tool_definitions_json);
            let tool_stats = extract_tool_stats(&result.conversation);
            let reasoning_stats = extract_reasoning_stats(&result.conversation);
            if verbose {
                eprintln!(
                    "   Prompt {prompt_index}: done ({} iterations)",
                    result.iterations
                );
            }
            json!({
                "success": true,
                "prompt_index": prompt_index,
                "prompt": prompt,
                "trajectory": trajectory,
                "tool_stats": tool_stats,
                "reasoning_stats": reasoning_stats,
                "completed": true,
                "api_calls": result.iterations,
                "metadata": {
                    "batch_num": batch_num,
                    "timestamp": iso_now(),
                    "model": model,
                },
            })
        }
        Err(e) => {
            eprintln!("❌ Error processing prompt {prompt_index}: {e}");
            json!({
                "success": false,
                "prompt_index": prompt_index,
                "prompt": prompt,
                "error": e.to_string(),
                "trajectory": null,
                "tool_stats": {},
                "toolsets_used": [],
                "metadata": {
                    "batch_num": batch_num,
                    "timestamp": iso_now(),
                },
            })
        }
    }
}

/// Entry point (hermes `BatchRunner.run`).
pub async fn run(opts: BatchOptions) -> Result<(), String> {
    println!("\n{}", "=".repeat(70));
    println!("🚀 Starting Batch Processing");
    println!("{}", "=".repeat(70));

    let dataset = load_dataset(&opts.dataset_file)?;
    if dataset.is_empty() {
        return Err("dataset is empty".to_string());
    }
    let output_dir = PathBuf::from("batch_runs").join(&opts.run_name);
    std::fs::create_dir_all(&output_dir)
        .map_err(|e| format!("cannot create {}: {e}", output_dir.display()))?;
    let checkpoint_path = output_dir.join("checkpoint.json");

    // Resume filtering (hermes content-scan + checkpoint indices).
    let mut entries: Vec<(usize, Value)> = dataset.iter().cloned().enumerate().collect();
    if opts.resume {
        let done_content = scan_completed_prompts_by_content(&output_dir);
        let checkpoint = load_checkpoint(&checkpoint_path, &opts.run_name);
        let done_indices: std::collections::BTreeSet<usize> = checkpoint
            .get("completed_prompts")
            .and_then(Value::as_array)
            .map(|indices| {
                indices
                    .iter()
                    .filter_map(Value::as_u64)
                    .map(|index| index as usize)
                    .collect()
            })
            .unwrap_or_default();
        let before = entries.len();
        entries.retain(|(index, entry)| {
            let prompt = entry.get("prompt").and_then(Value::as_str).unwrap_or("");
            !done_indices.contains(index) && !done_content.contains(prompt)
        });
        println!("\n📊 RESUME SUMMARY");
        println!("   Original dataset size:     {before} prompts");
        println!(
            "   Already completed:         {} prompts",
            before - entries.len()
        );
        println!("   🎯 RESUMING WITH:          {} prompts", entries.len());
        if entries.is_empty() {
            println!("\n✅ All prompts have already been processed!");
            return Ok(());
        }
    }

    let mut checkpoint = load_checkpoint(&checkpoint_path, &opts.run_name);

    let batch_size = opts.batch_size.max(1);
    let batches: Vec<Vec<(usize, Value)>> = entries
        .chunks(batch_size)
        .map(<[(usize, Value)]>::to_vec)
        .collect();
    println!(
        "   {} prompts → {} batches (batch size {}, {} workers)\n",
        entries.len(),
        batches.len(),
        batch_size,
        opts.num_workers
    );

    let agent = build_batch_agent(&opts).await?;
    let model = opts.model.clone().unwrap_or_else(|| {
        crate::config::UlncLawConfig::load(None)
            .map(|c| c.model.model)
            .unwrap_or_default()
    });
    let start = std::time::Instant::now();

    // Parallel batches (hermes Pool over batch tasks); prompts inside a
    // batch run sequentially within their worker, exactly like hermes.
    let agent_ref = agent.clone();
    let verbose = opts.verbose;
    let model_ref = model.clone();
    let batch_results: Vec<(usize, Vec<Value>)> =
        futures::stream::iter(batches.iter().enumerate().map(|(batch_num, batch)| {
            let agent = agent_ref.clone();
            let batch = batch.clone();
            let model = model_ref.clone();
            async move {
                let mut results = Vec::new();
                for (prompt_index, prompt_data) in &batch {
                    results.push(
                        process_single_prompt(
                            &agent,
                            *prompt_index,
                            prompt_data,
                            batch_num,
                            &model,
                            verbose,
                        )
                        .await,
                    );
                }
                (batch_num, results)
            }
        }))
        .buffer_unordered(opts.num_workers.max(1))
        .collect::<Vec<_>>()
        .await;

    // Persist batch files + checkpoint (hermes per-batch writes).
    let mut completed_indices: Vec<usize> = checkpoint
        .get("completed_prompts")
        .and_then(Value::as_array)
        .map(|indices| {
            indices
                .iter()
                .filter_map(Value::as_u64)
                .map(|index| index as usize)
                .collect()
        })
        .unwrap_or_default();
    let mut total_succeeded = 0usize;
    let mut total_failed = 0usize;
    let mut aggregate_tool_stats: BTreeMap<String, (u64, u64, u64)> = BTreeMap::new();

    let mut ordered = batch_results;
    ordered.sort_by_key(|(batch_num, _)| *batch_num);
    for (batch_num, results) in &ordered {
        let batch_path = output_dir.join(format!("batch_{batch_num}.json"));
        if let Ok(raw) = serde_json::to_string_pretty(&results) {
            std::fs::write(&batch_path, raw).ok();
        }
        let mut batch_ok = 0usize;
        for result in results {
            if result
                .get("success")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                total_succeeded += 1;
                batch_ok += 1;
                if let Some(index) = result.get("prompt_index").and_then(Value::as_u64) {
                    completed_indices.push(index as usize);
                }
                if let Some(stats) = result.get("tool_stats").and_then(Value::as_object) {
                    for (name, entry) in stats {
                        let agg = aggregate_tool_stats
                            .entry(name.clone())
                            .or_insert((0, 0, 0));
                        agg.0 += entry.get("count").and_then(Value::as_u64).unwrap_or(0);
                        agg.1 += entry.get("success").and_then(Value::as_u64).unwrap_or(0);
                        agg.2 += entry.get("failure").and_then(Value::as_u64).unwrap_or(0);
                    }
                }
            } else {
                total_failed += 1;
            }
        }
        checkpoint["batch_stats"][batch_num.to_string()] =
            json!({"prompts": results.len(), "succeeded": batch_ok});
        checkpoint["completed_prompts"] = json!(completed_indices);
        checkpoint["last_updated"] = json!(iso_now());
        save_checkpoint(&checkpoint_path, &checkpoint);
        println!(
            "   ✅ Batch {batch_num}: {batch_ok}/{} prompts succeeded",
            results.len()
        );
    }

    let duration = start.elapsed().as_secs_f64();
    let mut aggregate = serde_json::Map::new();
    for (name, (count, success, failure)) in &aggregate_tool_stats {
        aggregate.insert(
            name.clone(),
            json!({"count": count, "success": success, "failure": failure}),
        );
    }
    let summary = json!({
        "run_name": opts.run_name,
        "dataset_file": opts.dataset_file.display().to_string(),
        "model": model,
        "total_prompts": entries.len(),
        "succeeded": total_succeeded,
        "failed": total_failed,
        "duration_secs": duration,
        "tool_stats": Value::Object(aggregate),
        "finished_at": iso_now(),
    });
    if let Ok(raw) = serde_json::to_string_pretty(&summary) {
        std::fs::write(output_dir.join("summary.json"), raw).ok();
    }

    println!("\n{}", "=".repeat(70));
    println!("🏁 Batch run complete");
    println!("   Succeeded: {total_succeeded} / {}", entries.len());
    println!("   Failed:    {total_failed}");
    println!("   Duration:  {duration:.1}s");
    println!("   Output:    {}", output_dir.display());
    println!("{}", "=".repeat(70));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{FunctionCall, Message, Role, ToolCall};

    fn msg(role: Role, content: &str) -> Message {
        Message {
            role,
            content: Some(content.to_string()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }

    #[test]
    fn dataset_loading_rejects_bad_lines() {
        let dir = std::env::temp_dir().join(format!("ulnclaw-batch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data.jsonl");
        std::fs::write(&path, "{\"prompt\":\"a\"}\n\n{\"prompt\":\"b\"}\n").unwrap();
        let entries = load_dataset(&path).unwrap();
        assert_eq!(entries.len(), 2);

        std::fs::write(&path, "{\"nope\":1}\n").unwrap();
        assert!(load_dataset(&path).is_err());
    }

    #[test]
    fn tool_stats_detect_failures() {
        let messages = vec![
            msg(Role::Assistant, ""),
            Message {
                role: Role::Assistant,
                content: None,
                tool_calls: Some(vec![
                    ToolCall {
                        id: "c1".into(),
                        call_type: "function".into(),
                        function: FunctionCall {
                            name: "terminal".into(),
                            arguments: "{}".into(),
                        },
                    },
                    ToolCall {
                        id: "c2".into(),
                        call_type: "function".into(),
                        function: FunctionCall {
                            name: "read_file".into(),
                            arguments: "{}".into(),
                        },
                    },
                ]),
                tool_call_id: None,
                name: None,
            },
            Message {
                role: Role::Tool,
                content: Some("{\"content\": {\"output\": \"ok\"}}".into()),
                tool_calls: None,
                tool_call_id: Some("c1".into()),
                name: None,
            },
            Message {
                role: Role::Tool,
                content: Some("{\"error\": \"boom\"}".into()),
                tool_calls: None,
                tool_call_id: Some("c2".into()),
                name: None,
            },
        ];
        let stats = extract_tool_stats(&messages);
        assert_eq!(stats["terminal"]["count"], 1);
        assert_eq!(stats["terminal"]["success"], 1);
        assert_eq!(stats["read_file"]["failure"], 1);
    }

    #[test]
    fn trajectory_shape_matches_hermes() {
        let messages = vec![
            msg(Role::System, "sys"),
            msg(Role::User, "do the thing"),
            Message {
                role: Role::Assistant,
                content: Some("on it".into()),
                tool_calls: Some(vec![ToolCall {
                    id: "c1".into(),
                    call_type: "function".into(),
                    function: FunctionCall {
                        name: "terminal".into(),
                        arguments: "{\"command\":\"ls\"}".into(),
                    },
                }]),
                tool_call_id: None,
                name: None,
            },
            Message {
                role: Role::Tool,
                content: Some("file.txt".into()),
                tool_calls: None,
                tool_call_id: Some("c1".into()),
                name: None,
            },
            msg(Role::Assistant, "done!"),
        ];
        let trajectory = convert_to_trajectory(&messages, "do the thing", "[]");
        assert_eq!(trajectory[0]["from"], "system");
        assert_eq!(trajectory[1]["from"], "human");
        assert_eq!(trajectory[1]["value"], "do the thing");
        assert_eq!(trajectory[2]["from"], "gpt");
        let gpt = trajectory[2]["value"].as_str().unwrap();
        assert!(gpt.contains("<tool_call>"));
        assert!(gpt.contains("\"name\":\"terminal\""));
        assert!(gpt.contains("<tool_response>"));
        assert_eq!(trajectory[3]["from"], "gpt");
        assert_eq!(trajectory[3]["value"], "done!");
    }

    #[test]
    fn reasoning_stats_counts_scratchpads() {
        let messages = vec![
            msg(Role::Assistant, "plain"),
            msg(
                Role::Assistant,
                "<REASONING_SCRATCHPAD>hmm</REASONING_SCRATCHPAD>",
            ),
        ];
        let stats = extract_reasoning_stats(&messages);
        assert_eq!(stats["total_assistant_turns"], 2);
        assert_eq!(stats["turns_with_reasoning"], 1);
    }

    #[test]
    fn checkpoint_roundtrip_requires_run_name_match() {
        let dir = std::env::temp_dir().join(format!("ulnclaw-batch-ck-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("checkpoint.json");
        let mut checkpoint = default_checkpoint("run-a");
        checkpoint["completed_prompts"] = json!([0, 1]);
        save_checkpoint(&path, &checkpoint);
        let loaded = load_checkpoint(&path, "run-a");
        assert_eq!(loaded["completed_prompts"].as_array().unwrap().len(), 2);
        // Different run name → fresh checkpoint.
        let other = load_checkpoint(&path, "run-b");
        assert!(other["completed_prompts"].as_array().unwrap().is_empty());
    }

    #[test]
    fn content_scan_finds_saved_prompts() {
        let dir = std::env::temp_dir().join(format!("ulnclaw-batch-scan-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("batch_0.json"),
            json!([{"prompt": "already done", "success": true}]).to_string(),
        )
        .unwrap();
        let done = scan_completed_prompts_by_content(&dir);
        assert!(done.contains("already done"));
    }
}

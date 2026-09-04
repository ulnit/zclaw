//! Mixture-of-Agents (MoA) — port of hermes' `agent/moa_loop.py` synthesis
//! path (`aggregate_moa_context`) plus the preset model of
//! `hermes_cli/moa_config.py`.
//!
//! A preset fans the user prompt out to several *reference* models in
//! parallel, joins their answers, and asks an *aggregator* model to
//! synthesize concise guidance. Failed references degrade loudly (or
//! silently, per preset) instead of aborting the turn.

use crate::config::{default_base_url, MoaPreset, MoaSlot, UlncLawConfig};
use crate::error::{AgentError, Result};
use crate::provider::auxiliary::{build_task_provider, is_keyless};
use crate::provider::{Message, Provider, ProviderRequest, Role};
use std::sync::Arc;

/// Sentinel prefix marking a failed reference output (hermes `_is_failed_reference`).
const FAILED_PREFIX: &str = "[failed:";
/// Sentinel prefix marking a skipped reference output.
const SKIPPED_PREFIX: &str = "[skipped:";

/// One reference model's outcome.
#[derive(Debug, Clone)]
pub struct MoaReferenceOutcome {
    /// `provider:model` label.
    pub label: String,
    /// Reference text, or the failure note when the call failed.
    pub text: String,
}

impl MoaReferenceOutcome {
    /// True when the output is a failure/skip sentinel, not real advice.
    pub fn failed(&self) -> bool {
        let trimmed = self.text.trim_start().to_lowercase();
        trimmed.starts_with(FAILED_PREFIX) || trimmed.starts_with(SKIPPED_PREFIX)
    }
}

/// Full MoA run result.
#[derive(Debug, Clone)]
pub struct MoaOutcome {
    /// Aggregator synthesis (joined references when aggregation fails).
    pub synthesis: String,
    /// The wrapped "[Mixture of Agents context …]" block (hermes format).
    pub wrapped: String,
    /// Per-reference outcomes in preset order.
    pub references: Vec<MoaReferenceOutcome>,
    /// Aggregator label.
    pub aggregator_label: String,
}

/// Builds the provider for one slot (injectable for tests).
pub type SlotProviderFactory = Arc<dyn Fn(&MoaSlot) -> Result<Arc<dyn Provider>> + Send + Sync>;

/// Production slot factory: builds an OpenAI-compatible or Anthropic client
/// per slot, with credential fallback to the main runtime (mirrors the
/// auxiliary resolution rules).
pub fn default_slot_factory(config: UlncLawConfig) -> SlotProviderFactory {
    Arc::new(move |slot: &MoaSlot| {
        let provider = slot.provider.trim().to_string();
        let model = slot.model.trim().to_string();
        if provider.is_empty() || model.is_empty() {
            return Err(AgentError::config(format!(
                "MoA slot needs provider and model (got '{}')",
                slot.label()
            )));
        }
        let api_key = slot.resolved_api_key(&config);
        if api_key.is_none() && !is_keyless(&provider) {
            return Err(AgentError::config(format!(
                "MoA slot {}: no API key (set api_key, key_env, or the main provider key)",
                slot.label()
            )));
        }
        let base_url = slot
            .base_url
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| default_base_url(&provider));
        build_task_provider(
            &provider,
            &model,
            &base_url,
            api_key.as_deref(),
            config.model.max_retries,
        )
    })
}

fn simple_user_request(
    prompt: &str,
    model: &str,
    temperature: Option<f32>,
    max_tokens: Option<u32>,
) -> ProviderRequest {
    ProviderRequest {
        messages: vec![Message {
            role: Role::User,
            content: Some(prompt.to_string()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }],
        tools: vec![],
        model: model.to_string(),
        max_tokens,
        temperature,
        stream: false,
        stop: None,
        images: None,
    }
}

/// Run the reference fan-out for one preset.
async fn run_references(
    preset: &MoaPreset,
    prompt: &str,
    factory: &SlotProviderFactory,
) -> Vec<MoaReferenceOutcome> {
    let mut handles = Vec::new();
    for slot in preset.reference_models.iter().filter(|s| s.enabled) {
        let slot = slot.clone();
        let prompt = prompt.to_string();
        let factory = factory.clone();
        let temperature = preset.reference_temperature;
        let max_tokens = preset.reference_max_tokens;
        handles.push(tokio::spawn(async move {
            let label = slot.label();
            let outcome = async {
                let provider = factory(&slot)?;
                let request = simple_user_request(
                    &prompt,
                    &slot.model.trim().to_string(),
                    temperature,
                    max_tokens,
                );
                let response = provider.chat_completion(request).await?;
                Ok::<String, AgentError>(response.content.unwrap_or_default())
            }
            .await;
            match outcome {
                Ok(text) if !text.trim().is_empty() => MoaReferenceOutcome { label, text },
                Ok(_) => MoaReferenceOutcome {
                    label,
                    text: format!("{} empty response]", FAILED_PREFIX),
                },
                Err(e) => MoaReferenceOutcome {
                    label,
                    text: format!("{} {}]", FAILED_PREFIX, e),
                },
            }
        }));
    }
    let mut outcomes = Vec::new();
    for handle in handles {
        if let Ok(outcome) = handle.await {
            outcomes.push(outcome);
        }
    }
    outcomes
}

/// Join successful reference outputs (hermes "Reference {idx} — {label}:").
pub fn join_references(references: &[MoaReferenceOutcome]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut index = 0usize;
    for outcome in references {
        if outcome.failed() {
            continue;
        }
        index += 1;
        parts.push(format!(
            "Reference {} — {}:\n{}",
            index, outcome.label, outcome.text
        ));
    }
    parts.join("\n\n")
}

/// Failed-reference notice (hermes `_degraded_notice`).
pub fn degraded_notice(references: &[MoaReferenceOutcome], policy: &str) -> String {
    let failed: Vec<&str> = references
        .iter()
        .filter(|r| r.failed())
        .map(|r| r.label.as_str())
        .collect();
    if failed.is_empty() || policy.trim().eq_ignore_ascii_case("silent") {
        return String::new();
    }
    format!("[Reference models unavailable: {}]", failed.join(", "))
}

/// Aggregator synthesis prompt (port of hermes `aggregate_moa_context`).
pub fn aggregator_prompt(user_prompt: &str, joined_references: &str) -> String {
    format!(
        "You are the aggregator in a Mixture of Agents process. Synthesize the \
reference responses into concise, actionable guidance for the main agent. \
Focus on next steps, tool-use strategy, risks, and any disagreements. Do not \
answer the user directly unless that is all that is needed; produce context \
the main agent should use in its normal loop.\n\n\
Original user prompt:\n{}\n\n\
Reference responses:\n{}",
        user_prompt, joined_references
    )
}

/// Run one MoA turn: fan-out → join → aggregate (hermes
/// `aggregate_moa_context` semantics, including the all-references-failed
/// early return and the joined-fallback when the aggregator fails).
pub async fn run_moa_with(
    preset_name: Option<&str>,
    prompt: &str,
    config: &UlncLawConfig,
    factory: SlotProviderFactory,
    session_id: Option<&str>,
) -> Result<MoaOutcome> {
    let (preset_name, preset) = config.moa.resolve_preset(preset_name)?;
    let references = run_references(preset, prompt, &factory).await;
    let privacy_mode = config.moa.privacy_mode();
    let display_refs: Vec<MoaReferenceOutcome> = if privacy_mode.is_empty() {
        references.clone()
    } else {
        references
            .iter()
            .map(|outcome| MoaReferenceOutcome {
                label: outcome.label.clone(),
                text: if outcome.failed() {
                    outcome.text.clone()
                } else {
                    redact_reference_text(&outcome.text)
                },
            })
            .collect()
    };
    // `full` additionally redacts the advisor text the aggregator sees
    // (hermes issue #59959); `display` keeps the aggregator on raw text.
    let aggregation_refs: Vec<MoaReferenceOutcome> = if privacy_mode == "full" {
        display_refs.clone()
    } else {
        references.clone()
    };
    let successful: Vec<&MoaReferenceOutcome> =
        aggregation_refs.iter().filter(|r| !r.failed()).collect();
    let degraded = degraded_notice(&aggregation_refs, &preset.degraded_reference_policy);
    let ref_labels: Vec<String> = preset
        .reference_models
        .iter()
        .filter(|s| s.enabled)
        .map(|s| s.label())
        .collect();
    let aggregator_label = preset.aggregator.label();

    // All references failed → skip the aggregator call entirely (hermes).
    if successful.is_empty() {
        let notice = if degraded.is_empty() {
            "[Reference models unavailable]".to_string()
        } else {
            degraded
        };
        let wrapped = format!(
            "[Mixture of Agents context — all reference models failed. \
Proceeding without aggregated guidance.]\nReferences: {}\n\n{}",
            ref_labels.join(", "),
            notice
        );
        return Ok(MoaOutcome {
            synthesis: notice.clone(),
            wrapped,
            references,
            aggregator_label,
        });
    }

    let joined = join_references(&aggregation_refs);
    let joined_with_notice = if degraded.is_empty() {
        joined.clone()
    } else {
        format!("{}\n\n{}", joined, degraded)
    };

    let synthesis = match factory(&preset.aggregator) {
        Ok(provider) => {
            let request = ProviderRequest {
                messages: vec![Message {
                    role: Role::User,
                    content: Some(aggregator_prompt(prompt, &joined_with_notice)),
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                }],
                tools: vec![],
                model: preset.aggregator.model.trim().to_string(),
                max_tokens: None,
                temperature: preset.aggregator_temperature,
                stream: false,
                stop: None,

                images: None,
            };
            match provider.chat_completion(request).await {
                Ok(response) => response.content.unwrap_or_default(),
                Err(e) => {
                    tracing::warn!("MoA aggregator {} failed: {}", aggregator_label, e);
                    String::new()
                }
            }
        }
        Err(e) => {
            tracing::warn!("MoA aggregator resolution failed: {}", e);
            String::new()
        }
    };
    let synthesis = if synthesis.trim().is_empty() {
        joined_with_notice.clone()
    } else {
        synthesis.trim().to_string()
    };

    let wrapped = format!(
        "[Mixture of Agents context — use this as private guidance for the \
normal agent loop. You may call tools, continue reasoning, or finish \
normally.]\nAggregator: {}\nReferences: {}\n\n{}",
        aggregator_label,
        ref_labels.join(", "),
        synthesis
    );
    save_moa_turn(
        config,
        session_id,
        &preset_name,
        &display_refs,
        &aggregator_label,
        &synthesis,
        privacy_mode,
    );
    Ok(MoaOutcome {
        synthesis,
        wrapped,
        references: display_refs,
        aggregator_label,
    })
}

/// Run one MoA turn with the production slot factory.
pub async fn run_moa(
    config: &UlncLawConfig,
    prompt: &str,
    preset_name: Option<&str>,
) -> Result<MoaOutcome> {
    run_moa_with(
        preset_name,
        prompt,
        config,
        default_slot_factory(config.clone()),
        None,
    )
    .await
}

// ---------------------------------------------------------------------------
// Privacy filter (hermes moa_loop.py "MoA privacy filter")
// ---------------------------------------------------------------------------

use std::sync::OnceLock;

fn moa_email_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}\b").unwrap()
    })
}

/// Delimited formatted phones only — bare digit runs, dates, times, hex
/// IDs, IPs and version numbers never match (hermes `_MOA_PHONE_RE`).
/// Rust's `regex` has no lookarounds, so the hermes boundary asserts are
/// applied by hand around each core match.
fn moa_phone_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"(?:\+?1[ .-])?(?:\(\d{3}\)[ .-]?|\d{3}[.-])\d{3}[.-]\d{4}").unwrap()
    })
}

fn boundary_ok_before(text: &str, start: usize) -> bool {
    match text[..start].chars().next_back() {
        None => true,
        Some(c) => !(c.is_ascii_alphanumeric() || c == '_' || matches!(c, '.' | '+' | '-')),
    }
}

fn boundary_ok_after(text: &str, end: usize) -> bool {
    match text[end..].chars().next() {
        None => true,
        Some(c) => !(c.is_ascii_alphanumeric() || c == '_' || c == '-'),
    }
}

fn redact_phones(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    for m in moa_phone_re().find_iter(text) {
        if boundary_ok_before(text, m.start()) && boundary_ok_after(text, m.end()) {
            out.push_str(&text[cursor..m.start()]);
            out.push_str("[redacted phone]");
            cursor = m.end();
        }
    }
    out.push_str(&text[cursor..]);
    out
}

/// Redact secrets + PII from one advisor/reference text surface (hermes
/// `_redact_reference_text`): centralized secret shapes first (code-file
/// mode so source snippets survive), then the MoA-specific email and
/// formatted-phone patterns.
pub fn redact_reference_text(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let mut out = crate::redact::redact_sensitive_text(
        text,
        crate::redact::RedactOpts {
            code_file: true,
            file_read: false,
            redact_url_credentials: false,
        },
    );
    out = moa_email_re()
        .replace_all(&out, "[redacted email]")
        .into_owned();
    redact_phones(&out)
}

// ---------------------------------------------------------------------------
// Traces (hermes agent/moa_trace.py)
// ---------------------------------------------------------------------------

/// Sanitize a session id into a safe trace-file stem (hermes
/// `_sanitize_session_id`).
fn sanitize_session_id(session_id: Option<&str>) -> String {
    let raw = session_id.unwrap_or("default").trim();
    if raw.is_empty() {
        return "default".to_string();
    }
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    cleaned.chars().take(120).collect()
}

/// Append one MoA turn record to the session trace JSONL when
/// `[moa] save_traces` is on (hermes `save_moa_turn`). Best-effort:
/// tracing never breaks a live turn.
pub fn save_moa_turn(
    config: &UlncLawConfig,
    session_id: Option<&str>,
    preset_name: &str,
    references: &[MoaReferenceOutcome],
    aggregator_label: &str,
    aggregator_output: &str,
    privacy_mode: &str,
) {
    if !config.moa.save_traces {
        return;
    }
    let result = (|| -> std::io::Result<()> {
        let dir = match config
            .moa
            .trace_dir
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty())
        {
            Some(dir) => std::path::PathBuf::from(dir),
            None => crate::config::ulnclaw_home().join("moa-traces"),
        };
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}.jsonl", sanitize_session_id(session_id)));
        let refs: Vec<serde_json::Value> = references
            .iter()
            .map(|r| {
                serde_json::json!({
                    "label": r.label,
                    "failed": r.failed(),
                    "chars": r.text.chars().count(),
                    "text": r.text,
                })
            })
            .collect();
        let record = serde_json::json!({
            "ts": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0),
            "session_id": session_id,
            "preset": preset_name,
            "privacy_filter": privacy_mode,
            "references": refs,
            "aggregator": {
                "label": aggregator_label,
                "output": aggregator_output,
            },
        });
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        writeln!(file, "{}", record)?;
        Ok(())
    })();
    if let Err(e) = result {
        tracing::debug!("MoA trace write failed: {}", e);
    }
}

// ---------------------------------------------------------------------------
// Persistent `provider = "moa"` facade (hermes build_moa_facade /
// MoAChatCompletions): the whole agent loop runs on MoA — each turn fans
// out to the reference models once, then the aggregator acts as the
// acting model (tools forwarded) with the reference guidance attached.
// ---------------------------------------------------------------------------

/// Tool-result budget when rendering the conversation for reference
/// models (hermes `_REFERENCE_TOOL_RESULT_BUDGET`).
const REFERENCE_TOOL_RESULT_BUDGET: usize = 2000;

/// Render the conversation into one prompt for the reference fan-out
/// (port of hermes `_reference_messages` + `_truncate_tool_result`):
/// role-tagged lines, tool results truncated to the budget.
pub fn render_conversation_for_references(messages: &[Message]) -> String {
    let mut lines = Vec::new();
    for message in messages {
        let role = match message.role {
            Role::System => "System",
            Role::User => "User",
            Role::Assistant => "Assistant",
            Role::Tool => "Tool",
        };
        let mut text = message.content.clone().unwrap_or_default();
        if matches!(message.role, Role::Tool) && text.len() > REFERENCE_TOOL_RESULT_BUDGET {
            let cut = text
                .char_indices()
                .nth(REFERENCE_TOOL_RESULT_BUDGET)
                .map(|(i, _)| i)
                .unwrap_or(text.len());
            text.truncate(cut);
            text.push_str("\n[truncated]");
        }
        if text.trim().is_empty() {
            continue;
        }
        lines.push(format!("{}: {}", role, text));
    }
    lines.join("\n\n")
}

fn truncate_for_key(text: &str, budget: usize) -> String {
    if text.len() <= budget {
        return text.to_string();
    }
    let cut = text
        .char_indices()
        .nth(budget)
        .map(|(i, _)| i)
        .unwrap_or(text.len());
    let mut out = text[..cut].to_string();
    out.push_str("\n[truncated]");
    out
}

/// Cache key: everything up to and including the last user message —
/// tool-loop iterations after it leave the prefix (and the reference
/// fan-out) unchanged (hermes MoAChatCompletions reference cache).
fn reference_cache_key(messages: &[Message]) -> Option<String> {
    let last_user = messages
        .iter()
        .rposition(|m| matches!(m.role, Role::User))?;
    let mut parts = Vec::new();
    for message in &messages[..=last_user] {
        let role = match message.role {
            Role::System => "s",
            Role::User => "u",
            Role::Assistant => "a",
            Role::Tool => "t",
        };
        parts.push(format!(
            "{}:{}",
            role,
            truncate_for_key(&message.content.clone().unwrap_or_default(), 4000)
        ));
    }
    Some(parts.join("\n"))
}

/// One cached fan-out (references + the wrapped guidance block).
#[derive(Default)]
pub struct MoaReferenceCache {
    inner: std::sync::Mutex<Option<(String, Vec<MoaReferenceOutcome>, String)>>,
}

impl MoaReferenceCache {
    fn get(&self, key: &str) -> Option<(Vec<MoaReferenceOutcome>, String)> {
        let guard = self.inner.lock().ok()?;
        let entry = guard.as_ref()?;
        if entry.0 == key {
            Some((entry.1.clone(), entry.2.clone()))
        } else {
            None
        }
    }

    fn put(&self, key: String, references: Vec<MoaReferenceOutcome>, wrapped: String) {
        if let Ok(mut guard) = self.inner.lock() {
            *guard = Some((key, references, wrapped));
        }
    }
}

/// Attach the per-turn reference block at the END of the aggregator
/// messages (hermes `_attach_reference_guidance`): merge into a trailing
/// user turn when present, else append one. Keeps the conversation
/// prefix stable for provider prompt caching.
fn attach_reference_guidance(messages: &mut Vec<Message>, guidance: &str) {
    if let Some(last) = messages.last_mut() {
        if matches!(last.role, Role::User) {
            let content = last.content.take().unwrap_or_default();
            last.content = Some(format!("{}\n\n{}", content, guidance));
            return;
        }
    }
    messages.push(Message {
        role: Role::User,
        content: Some(guidance.to_string()),
        tool_calls: None,
        tool_call_id: None,
        name: None,
    });
}

/// Run one facade turn: cached reference fan-out → guidance attachment →
/// aggregator call with the caller's tools forwarded. Returns the
/// aggregator's response (content and/or tool calls) as the assistant
/// turn for the agent loop.
pub async fn run_moa_facade_turn(
    request: &ProviderRequest,
    config: &UlncLawConfig,
    preset_name: &str,
    factory: &SlotProviderFactory,
    cache: &MoaReferenceCache,
    session_id: Option<&str>,
) -> Result<crate::provider::ProviderResponse> {
    let (_, preset) = config.moa.resolve_preset(Some(preset_name))?;
    let privacy_mode = config.moa.privacy_mode();

    // 1. Reference fan-out (cache hit on tool-loop iterations).
    let cache_key = reference_cache_key(&request.messages);
    let (display_refs, wrapped) = match cache_key.as_deref().and_then(|key| cache.get(key)) {
        Some(hit) => hit,
        None => {
            let prompt = render_conversation_for_references(&request.messages);
            let references = run_references(preset, &prompt, factory).await;
            let display_refs: Vec<MoaReferenceOutcome> = if privacy_mode.is_empty() {
                references.clone()
            } else {
                references
                    .iter()
                    .map(|outcome| MoaReferenceOutcome {
                        label: outcome.label.clone(),
                        text: if outcome.failed() {
                            outcome.text.clone()
                        } else {
                            redact_reference_text(&outcome.text)
                        },
                    })
                    .collect()
            };
            let aggregation_refs: Vec<MoaReferenceOutcome> = if privacy_mode == "full" {
                display_refs.clone()
            } else {
                references.clone()
            };
            let degraded = degraded_notice(&aggregation_refs, &preset.degraded_reference_policy);
            let joined = join_references(&aggregation_refs);
            let wrapped = if aggregation_refs.iter().all(|r| r.failed()) {
                format!(
                    "[Mixture of Agents context — all reference models failed. \
Proceeding without aggregated guidance.]\n{}",
                    degraded
                )
            } else {
                format!(
                    "[Mixture of Agents context — private reference guidance \
for this turn. Synthesize it into your answer; do not quote it verbatim.]\n{}",
                    if degraded.is_empty() {
                        joined
                    } else {
                        format!("{}\n\n{}", joined, degraded)
                    }
                )
            };
            save_moa_turn(
                config,
                session_id,
                preset_name,
                &display_refs,
                &preset.aggregator.label(),
                "",
                privacy_mode,
            );
            if let Some(key) = cache_key.clone() {
                cache.put(key, display_refs.clone(), wrapped.clone());
            }
            (display_refs, wrapped)
        }
    };
    let _ = &display_refs;

    // 2. Aggregator acts as the acting model (tools forwarded).
    let mut messages = request.messages.clone();
    attach_reference_guidance(&mut messages, &wrapped);
    let aggregator_request = ProviderRequest {
        messages,
        tools: request.tools.clone(),
        model: preset.aggregator.model.trim().to_string(),
        max_tokens: request.max_tokens,
        temperature: preset.aggregator_temperature,
        stream: false,
        stop: request.stop.clone(),

        images: None,
    };
    let provider = factory(&preset.aggregator)?;
    match provider.chat_completion(aggregator_request).await {
        Ok(response) => Ok(response),
        Err(e) => {
            // Degrade loudly but keep the turn alive: the reference
            // guidance itself becomes the assistant text.
            tracing::warn!("MoA aggregator {} failed: {}", preset.aggregator.label(), e);
            Ok(crate::provider::ProviderResponse {
                content: Some(wrapped),
                tool_calls: Vec::new(),
                usage: None,
                model: preset.aggregator.model.trim().to_string(),
                reasoning: None,
                finish_reason: Some("stop".to_string()),
            })
        }
    }
}

/// Provider facade for `[model] provider = "moa"` (hermes
/// `build_moa_facade`): the agent loop talks to MoA, `model` selects the
/// preset. Non-streaming — callers fall back to `chat_completion`.
pub struct MoaProvider {
    config: UlncLawConfig,
    preset_name: String,
    factory: SlotProviderFactory,
    cache: Arc<MoaReferenceCache>,
    session_id: Option<String>,
}

impl MoaProvider {
    /// Build the facade, resolving the preset eagerly so a missing
    /// preset fails at startup with the available list.
    pub fn new(config: UlncLawConfig, session_id: Option<String>) -> Result<Self> {
        let requested = config.model.model.trim().to_string();
        let wanted = if requested.is_empty() {
            None
        } else {
            Some(requested.as_str())
        };
        let (preset_name, _) = config.moa.resolve_preset(wanted)?;
        let factory = default_slot_factory(config.clone());
        Ok(Self {
            config,
            preset_name,
            factory,
            cache: Arc::new(MoaReferenceCache::default()),
            session_id,
        })
    }

    /// The resolved preset name this facade runs on.
    pub fn preset_name(&self) -> &str {
        &self.preset_name
    }
}

#[async_trait::async_trait]
impl Provider for MoaProvider {
    async fn chat_completion(
        &self,
        request: ProviderRequest,
    ) -> Result<crate::provider::ProviderResponse> {
        run_moa_facade_turn(
            &request,
            &self.config,
            &self.preset_name,
            &self.factory,
            &self.cache,
            self.session_id.as_deref(),
        )
        .await
    }

    fn model(&self) -> &str {
        &self.preset_name
    }

    fn name(&self) -> &str {
        "moa"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{MoaConfig, MoaPreset, MoaSlot};
    use crate::provider::{ProviderResponse, Usage};
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Stub provider returning canned text; records request prompts.
    struct StubProvider {
        label: String,
        reply: String,
        log: Arc<Mutex<Vec<(String, String)>>>,
    }

    #[async_trait]
    impl Provider for StubProvider {
        async fn chat_completion(&self, request: ProviderRequest) -> Result<ProviderResponse> {
            let prompt = request
                .messages
                .first()
                .and_then(|m| m.content.clone())
                .unwrap_or_default();
            self.log
                .lock()
                .unwrap()
                .push((self.label.clone(), prompt.clone()));
            if self.reply == "<fail>" {
                return Err(AgentError::provider("stub failure"));
            }
            Ok(ProviderResponse {
                content: Some(self.reply.clone()),
                tool_calls: vec![],
                usage: Some(Usage::default()),
                model: request.model,
                reasoning: None,
                finish_reason: Some("stop".into()),
            })
        }
        fn model(&self) -> &str {
            "stub"
        }
        fn name(&self) -> &str {
            "stub"
        }
    }

    fn slot(provider: &str, model: &str) -> MoaSlot {
        MoaSlot {
            provider: provider.into(),
            model: model.into(),
            enabled: true,
            base_url: None,
            api_key: None,
            key_env: None,
        }
    }

    fn moa_config(preset: MoaPreset) -> UlncLawConfig {
        let mut config = UlncLawConfig::default();
        let mut presets = HashMap::new();
        presets.insert("default".to_string(), preset);
        config.moa = MoaConfig {
            default_preset: None,
            presets,
            ..Default::default()
        };
        config
    }

    fn preset(replies: &[(&str, &str)], aggregator: (&str, &str)) -> MoaPreset {
        MoaPreset {
            reference_models: replies
                .iter()
                .map(|(model, _)| slot("stubprov", model))
                .collect(),
            aggregator: slot("stubprov", aggregator.0),
            reference_temperature: None,
            reference_max_tokens: None,
            aggregator_temperature: None,
            degraded_reference_policy: "loud".into(),
        }
    }

    fn factory_for(
        replies: Vec<(&'static str, &'static str)>,
        log: Arc<Mutex<Vec<(String, String)>>>,
    ) -> SlotProviderFactory {
        Arc::new(move |slot: &MoaSlot| {
            let reply = replies
                .iter()
                .find(|(model, _)| *model == slot.model.trim())
                .map(|(_, reply)| *reply)
                .unwrap_or("<fail>");
            Ok(Arc::new(StubProvider {
                label: slot.label(),
                reply: reply.to_string(),
                log: log.clone(),
            }) as Arc<dyn Provider>)
        })
    }

    #[tokio::test]
    async fn fan_out_and_aggregate() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let config = moa_config(preset(
            &[("ref-a", "advice A"), ("ref-b", "advice B")],
            ("agg", "SYNTHESIS"),
        ));
        let factory = factory_for(
            vec![
                ("ref-a", "advice A"),
                ("ref-b", "advice B"),
                ("agg", "SYNTHESIS"),
            ],
            log.clone(),
        );
        let outcome = run_moa_with(None, "test prompt", &config, factory, None)
            .await
            .unwrap();
        assert_eq!(outcome.synthesis, "SYNTHESIS");
        assert!(outcome.wrapped.starts_with("[Mixture of Agents context"));
        assert!(outcome.wrapped.contains("Aggregator: stubprov:agg"));
        assert_eq!(outcome.references.len(), 2);
        // Aggregator saw both references and the original prompt.
        let agg_call = &log.lock().unwrap()[2];
        assert!(agg_call.1.contains("Reference 1 — stubprov:ref-a"));
        assert!(agg_call.1.contains("Reference 2 — stubprov:ref-b"));
        assert!(agg_call.1.contains("test prompt"));
    }

    #[tokio::test]
    async fn failed_reference_degrades_loudly() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let config = moa_config(preset(
            &[("ref-a", "advice A"), ("ref-b", "<fail>")],
            ("agg", "SYNTH"),
        ));
        let factory = factory_for(vec![("ref-a", "advice A"), ("agg", "SYNTH")], log.clone());
        let outcome = run_moa_with(None, "p", &config, factory, None)
            .await
            .unwrap();
        let failed = &outcome.references[1];
        assert!(failed.failed());
        let calls = log.lock().unwrap();
        let agg_call = calls.last().expect("aggregator call");
        assert!(agg_call
            .1
            .contains("[Reference models unavailable: stubprov:ref-b]"));
        assert_eq!(outcome.synthesis, "SYNTH");
    }

    #[tokio::test]
    async fn silent_policy_hides_failures() {
        let mut preset = preset(&[("ref-a", "advice A"), ("ref-b", "<fail>")], ("agg", "S"));
        preset.degraded_reference_policy = "silent".into();
        let config = moa_config(preset);
        let log = Arc::new(Mutex::new(Vec::new()));
        let factory = factory_for(vec![("ref-a", "advice A"), ("agg", "S")], log.clone());
        run_moa_with(None, "p", &config, factory, None)
            .await
            .unwrap();
        let calls = log.lock().unwrap();
        let agg_call = calls.last().expect("aggregator call");
        assert!(!agg_call.1.contains("Reference models unavailable"));
    }

    #[tokio::test]
    async fn all_references_failed_skips_aggregator() {
        let config = moa_config(preset(&[("ref-a", "<fail>")], ("agg", "S")));
        let log = Arc::new(Mutex::new(Vec::new()));
        let factory = factory_for(vec![], log.clone());
        let outcome = run_moa_with(None, "p", &config, factory, None)
            .await
            .unwrap();
        assert!(outcome.wrapped.contains("all reference models failed"));
        // Only the reference call happened — no aggregator call.
        assert_eq!(log.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn aggregator_failure_falls_back_to_joined() {
        let config = moa_config(preset(&[("ref-a", "advice A")], ("agg", "<fail>")));
        let log = Arc::new(Mutex::new(Vec::new()));
        let factory = factory_for(vec![("ref-a", "advice A")], log.clone());
        let outcome = run_moa_with(None, "p", &config, factory, None)
            .await
            .unwrap();
        assert!(outcome.synthesis.contains("Reference 1 — stubprov:ref-a"));
        assert!(outcome.synthesis.contains("advice A"));
    }

    #[tokio::test]
    async fn unknown_preset_lists_available() {
        let config = moa_config(preset(&[("ref-a", "x")], ("agg", "y")));
        let error = run_moa_with(
            Some("nope"),
            "p",
            &config,
            factory_for(vec![], Arc::new(Mutex::new(Vec::new()))),
            None,
        )
        .await
        .err()
        .unwrap();
        let message = error.to_string();
        assert!(message.contains("'nope'"), "{}", message);
        assert!(message.contains("default"), "{}", message);
    }

    #[test]
    fn privacy_filter_coercion() {
        use crate::config::MoaPrivacyFilter;
        assert_eq!(MoaPrivacyFilter::Flag(true).mode(), "full");
        assert_eq!(MoaPrivacyFilter::Flag(false).mode(), "");
        assert_eq!(MoaPrivacyFilter::Mode("display".into()).mode(), "display");
        assert_eq!(MoaPrivacyFilter::Mode("FULL".into()).mode(), "full");
        assert_eq!(MoaPrivacyFilter::Mode("on".into()).mode(), "full");
        assert_eq!(MoaPrivacyFilter::Mode("garbage".into()).mode(), "");
    }

    #[test]
    fn redact_reference_text_masks_pii() {
        let text = "mail bob@example.com or call (555) 123-4567, ref 2026-07-12";
        let out = redact_reference_text(text);
        assert!(out.contains("[redacted email]"), "{}", out);
        assert!(out.contains("[redacted phone]"), "{}", out);
        // Dates never match the phone pattern.
        assert!(out.contains("2026-07-12"), "{}", out);
    }

    #[test]
    fn conversation_render_truncates_tool_results() {
        let long = "x".repeat(REFERENCE_TOOL_RESULT_BUDGET + 50);
        let messages = vec![
            Message {
                role: Role::System,
                content: Some("be brief".into()),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            },
            Message {
                role: Role::User,
                content: Some("do it".into()),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            },
            Message {
                role: Role::Tool,
                content: Some(long),
                tool_calls: None,
                tool_call_id: Some("t1".into()),
                name: Some("shell".into()),
            },
        ];
        let rendered = render_conversation_for_references(&messages);
        assert!(rendered.starts_with("System: be brief"));
        assert!(rendered.contains("User: do it"));
        assert!(rendered.contains("[truncated]"));
        assert!(rendered.len() < "x".repeat(REFERENCE_TOOL_RESULT_BUDGET).len() + 200);
    }

    fn facade_config(privacy: Option<crate::config::MoaPrivacyFilter>) -> UlncLawConfig {
        let mut config = moa_config(preset(
            &[("ref-a", "advice from bob@example.com")],
            ("agg", "SYNTHESIS"),
        ));
        config.moa.privacy_filter = privacy;
        config
    }

    fn facade_request(text: &str) -> ProviderRequest {
        ProviderRequest {
            messages: vec![Message {
                role: Role::User,
                content: Some(text.to_string()),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            }],
            tools: vec![],
            model: "default".to_string(),
            max_tokens: None,
            temperature: None,
            stream: false,
            stop: None,
            images: None,
        }
    }

    #[tokio::test]
    async fn facade_turn_caches_references_across_tool_iterations() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let config = facade_config(None);
        let factory = factory_for(
            vec![("ref-a", "advice A"), ("agg", "SYNTHESIS")],
            log.clone(),
        );
        let cache = MoaReferenceCache::default();

        // Turn 1: fan-out + aggregator.
        let response = run_moa_facade_turn(
            &facade_request("task"),
            &config,
            "default",
            &factory,
            &cache,
            None,
        )
        .await
        .unwrap();
        assert_eq!(response.content.as_deref(), Some("SYNTHESIS"));
        assert_eq!(log.lock().unwrap().len(), 2);

        // Turn 2 (tool iteration: same prefix + assistant/tool tail):
        // references are cached — only the aggregator runs.
        let mut messages = facade_request("task").messages;
        messages.push(Message {
            role: Role::Assistant,
            content: Some("thinking".into()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        });
        messages.push(Message {
            role: Role::Tool,
            content: Some("tool output".into()),
            tool_calls: None,
            tool_call_id: Some("t".into()),
            name: Some("shell".into()),
        });
        let request = ProviderRequest {
            messages,
            ..facade_request("task")
        };
        run_moa_facade_turn(&request, &config, "default", &factory, &cache, None)
            .await
            .unwrap();
        assert_eq!(log.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn facade_privacy_full_redacts_aggregator_input() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let config = facade_config(Some(crate::config::MoaPrivacyFilter::Mode("full".into())));
        let factory = factory_for(
            vec![
                ("ref-a", "advice from bob@example.com"),
                ("agg", "SYNTHESIS"),
            ],
            log.clone(),
        );
        let cache = MoaReferenceCache::default();
        run_moa_facade_turn(
            &facade_request("task"),
            &config,
            "default",
            &factory,
            &cache,
            None,
        )
        .await
        .unwrap();
        let calls = log.lock().unwrap();
        let agg_call = calls.last().expect("aggregator call");
        assert!(!agg_call.1.contains("bob@example.com"), "{}", agg_call.1);
        assert!(agg_call.1.contains("[redacted email]"), "{}", agg_call.1);
    }

    #[tokio::test]
    async fn facade_privacy_display_keeps_aggregator_raw() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let config = facade_config(Some(crate::config::MoaPrivacyFilter::Mode(
            "display".into(),
        )));
        let factory = factory_for(
            vec![
                ("ref-a", "advice from bob@example.com"),
                ("agg", "SYNTHESIS"),
            ],
            log.clone(),
        );
        let cache = MoaReferenceCache::default();
        run_moa_facade_turn(
            &facade_request("task"),
            &config,
            "default",
            &factory,
            &cache,
            None,
        )
        .await
        .unwrap();
        let calls = log.lock().unwrap();
        let agg_call = calls.last().expect("aggregator call");
        assert!(agg_call.1.contains("bob@example.com"), "{}", agg_call.1);
    }

    #[tokio::test]
    async fn traces_write_session_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let saved_home = std::env::var("ULNCLAW_HOME").ok();
        std::env::set_var("ULNCLAW_HOME", dir.path());

        let mut config = moa_config(preset(&[("ref-a", "advice A")], ("agg", "SYNTH")));
        config.moa.save_traces = true;
        let factory = factory_for(
            vec![("ref-a", "advice A"), ("agg", "SYNTH")],
            Arc::new(Mutex::new(Vec::new())),
        );
        run_moa_with(Some("default"), "p", &config, factory, Some("sess/01"))
            .await
            .unwrap();

        let path = dir.path().join("moa-traces").join("sess_01.jsonl");
        let content = std::fs::read_to_string(&path).expect("trace file");
        assert!(content.contains("\"preset\":\"default\""), "{}", content);
        assert!(content.contains("SYNTH"), "{}", content);

        match saved_home {
            Some(v) => std::env::set_var("ULNCLAW_HOME", v),
            None => std::env::remove_var("ULNCLAW_HOME"),
        }
    }

    #[test]
    fn preset_resolution_prefers_default_preset() {
        let mut config = moa_config(preset(&[("ref-a", "x")], ("agg", "y")));
        config
            .moa
            .presets
            .insert("other".to_string(), preset(&[("ref-b", "x")], ("agg", "y")));
        config.moa.default_preset = Some("other".into());
        let (name, preset) = config.moa.resolve_preset(None).unwrap();
        assert_eq!(name, "other");
        assert_eq!(preset.reference_models[0].model, "ref-b");
    }
}

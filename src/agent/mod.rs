//! Agent — the core conversation loop
//!
//! Port of hermes' AIAgent (run_agent.py): provider calls, tool dispatch with
//! approval gating, session persistence, context compression, delegation
//! (SubAgentRunner), cron execution (CronRunner), memory injection, and
//! callbacks.

use crate::error::{AgentError, Result};
use crate::provider::{Message, Provider, ProviderRequest, Role, Usage};
use crate::session::sqlite::SqliteSessionStore;
use crate::tools::approval::{classify_command, ApprovalDecision};
use crate::tools::context::{CronRunner, SubAgentRunner};
use crate::tools::ToolContext;
use crate::tools::ToolRegistry;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, warn};

/// Default identity prompt when no custom system prompt is configured.
/// Shared with the `prompt-size` diagnostic so the numbers it reports match
/// what `Agent::effective_system_prompt` actually injects.
pub const DEFAULT_SYSTEM_PROMPT: &str = "You are ulnclaw, a capable AI assistant with tools for \
                                         shell, files, web, memory, skills, and scheduling. Be \
                                         concise and precise.";

/// Events surfaced to streaming consumers (gateway SSE).
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// Content token delta from the model.
    Delta(String),
    /// Tool lifecycle progress (rendered as `hermes.tool.progress` SSE
    /// events so frontends can show activity without polluting history).
    ToolProgress { tool: String, status: String },
    /// Tool call about to run (Responses API `function_call` items).
    ToolStarted {
        name: String,
        call_id: String,
        arguments: String,
    },
    /// Tool call finished (Responses API `function_call_output` items).
    ToolCompleted { call_id: String, result: String },
}

tokio::task_local! {
    static STREAM_EMITTER: Arc<dyn Fn(StreamEvent) + Send + Sync>;
    static MODEL_OVERRIDE: String;
    static CRON_RUN: bool;
}

/// Emit an event to the active stream consumer, if any (task-local scope
/// installed by the gateway for streaming requests).
pub fn emit_stream_event(event: StreamEvent) {
    if let Ok(emitter) = STREAM_EMITTER.try_with(|e| e.clone()) {
        emitter(event);
    }
}

/// Run a future with a stream emitter installed (gateway streaming scope).
pub fn stream_scope<F: std::future::Future>(
    emitter: Arc<dyn Fn(StreamEvent) + Send + Sync>,
    future: F,
) -> tokio::task::futures::TaskLocalFuture<Arc<dyn Fn(StreamEvent) + Send + Sync>, F> {
    STREAM_EMITTER.scope(emitter, future)
}

/// The active per-task model override, if any (installed by
/// `model_override_scope` for session model locks).
pub fn current_model_override() -> Option<String> {
    MODEL_OVERRIDE.try_with(|m| m.clone()).ok()
}

/// Run a future with a per-task provider model override in effect — all
/// provider calls and session-row model stamps inside `future` use `model`
/// instead of the agent's configured model (session model-lock support).
pub fn model_override_scope<F: std::future::Future>(
    model: String,
    future: F,
) -> tokio::task::futures::TaskLocalFuture<String, F> {
    MODEL_OVERRIDE.scope(model, future)
}
/// Run a future marked as a cron-triggered agent run (hermes cron
/// approval context): `true` scopes the run as unattended, so approval
/// gates apply `approvals.cron_mode` instead of waiting for a human.
pub fn cron_scope<F: std::future::Future>(
    cron: bool,
    future: F,
) -> tokio::task::futures::TaskLocalFuture<bool, F> {
    CRON_RUN.scope(cron, future)
}

/// Whether the current task runs inside a cron-triggered agent run.
pub fn is_cron_context() -> bool {
    CRON_RUN.try_with(|cron| *cron).unwrap_or(false)
}

/// Agent configuration
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Maximum iterations (tool call loops) per conversation
    pub max_iterations: usize,
    /// Maximum tokens for responses
    pub max_tokens: Option<u32>,
    /// Temperature for generation
    pub temperature: Option<f32>,
    /// System prompt
    pub system_prompt: Option<String>,
    /// Whether to strip thinking blocks from output
    pub strip_thinking_blocks: bool,
    /// Model name override (uses provider default if None)
    pub model: Option<String>,
    /// Whether to execute tool calls concurrently
    pub concurrent_tool_execution: bool,
    /// Maximum concurrent tool calls
    pub max_concurrent_tools: usize,
    /// Require approval for dangerous terminal commands.
    pub approval: bool,
    /// Context budget (tokens) before compression kicks in.
    pub context_budget_tokens: usize,
    /// Persist messages into the SQLite store.
    pub persist: bool,
    /// Source tag for created sessions ("cli", "cron", "delegate").
    pub source: String,
    /// Probe the local Python toolchain for the system prompt
    /// (hermes `agent.environment_probe`).
    pub environment_probe: bool,
    /// Terminal backend name ("local"/"docker"/"ssh") — the probe skips
    /// non-local backends since tools run in the sandbox, not on the host.
    pub terminal_backend: String,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_iterations: 90,
            max_tokens: None,
            temperature: None,
            system_prompt: None,
            strip_thinking_blocks: true,
            model: None,
            concurrent_tool_execution: false,
            max_concurrent_tools: 5,
            approval: true,
            context_budget_tokens: 120_000,
            persist: true,
            source: "cli".to_string(),
            environment_probe: true,
            terminal_backend: "local".to_string(),
        }
    }
}

/// Callbacks for agent events
pub struct AgentCallbacks {
    /// Called when a tool call starts
    pub on_tool_start: Option<Box<dyn Fn(&str, &serde_json::Value) + Send + Sync>>,
    /// Called when a tool call completes
    pub on_tool_complete: Option<Box<dyn Fn(&str, &serde_json::Value) + Send + Sync>>,
    /// Called for streaming text deltas
    pub on_stream_delta: Option<Box<dyn Fn(&str) + Send + Sync>>,
    /// Called when model is thinking
    pub on_thinking: Option<Box<dyn Fn() + Send + Sync>>,
    /// Called after each complete agent turn
    pub on_step: Option<Box<dyn Fn(u32) + Send + Sync>>,
    /// Called when an approval prompt is raised (informational).
    pub on_approval_request: Option<Box<dyn Fn(&str) + Send + Sync>>,
    /// P714: shared session-activity progress stamp (hermes
    /// `_touch_activity`): fires at every observable agent progress
    /// point (tool start/complete, stream delta, step, approval) with
    /// a short description. Observation-only — stall policy lives in
    /// `session_stall`.
    pub on_activity: Option<Box<dyn Fn(&str) + Send + Sync>>,
}

impl Default for AgentCallbacks {
    fn default() -> Self {
        Self {
            on_tool_start: None,
            on_tool_complete: None,
            on_stream_delta: None,
            on_thinking: None,
            on_step: None,
            on_approval_request: None,
            on_activity: None,
        }
    }
}

/// Result of running the agent
#[derive(Debug, Clone)]
pub struct RunResult {
    /// The final text response
    pub content: String,
    /// Complete conversation history
    pub conversation: Vec<Message>,
    /// Total token usage across all iterations
    pub usage: Usage,
    /// Number of iterations executed
    pub iterations: usize,
    /// Tool calls made during this run
    pub tool_calls: Vec<ToolCallRecord>,
    /// Session id (when persistence is wired)
    pub session_id: Option<String>,
}

/// Record of a tool call
#[derive(Debug, Clone)]
pub struct ToolCallRecord {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
    pub result: serde_json::Value,
}

/// The main agent — orchestrates conversation flow
pub struct Agent {
    provider: Arc<dyn Provider>,
    tools: Arc<Mutex<ToolRegistry>>,
    config: AgentConfig,
    callbacks: Arc<Mutex<AgentCallbacks>>,
    context: Arc<ToolContext>,
    store: Option<Arc<SqliteSessionStore>>,
    /// Delegation depth (0 = top-level).
    depth: usize,
    /// Provider fallback chain (hermes `fallback_providers`), tried in
    /// order when a model call fails.
    fallback_chain: Vec<FallbackEntry>,
    /// The raw `"provider:model"` specs the chain was built from (so child
    /// agents can inherit them).
    fallback_specs: Vec<String>,
    /// Currently active fallback index (`None` = primary provider).
    fallback_active: tokio::sync::Mutex<Option<usize>>,
    /// Consecutive smart-approval guardian DENY verdicts (circuit breaker,
    /// hermes `approvals.denial_breaker_threshold`). Any approval resets.
    smart_denial_streak: std::sync::atomic::AtomicU64,
    /// P680: per-session steer inboxes — `/steer` drops messages here
    /// and the session's active turn injects them after the next tool
    /// batch (hermes `/steer` parity).
    steer_inboxes:
        tokio::sync::Mutex<std::collections::HashMap<String, std::collections::VecDeque<String>>>,
}

/// One fallback provider slot (hermes `fallback_providers` entry).
pub struct FallbackEntry {
    pub provider_name: String,
    pub model: String,
    provider: tokio::sync::OnceCell<Arc<dyn Provider>>,
}

impl FallbackEntry {
    pub fn label(&self) -> String {
        format!("{}:{}", self.provider_name, self.model)
    }
}

/// Parse a `"provider:model"` fallback spec (the model may itself contain
/// `:`, e.g. `ollama:qwen3:1.7b`).
pub fn parse_fallback_spec(spec: &str) -> Option<(String, String)> {
    let (provider_name, model) = spec.split_once(':')?;
    let provider_name = provider_name.trim();
    let model = model.trim();
    if provider_name.is_empty() || model.is_empty() {
        return None;
    }
    Some((provider_name.to_string(), model.to_string()))
}

impl Agent {
    /// Create a new agent with a provider and tool registry
    pub fn new(provider: Arc<dyn Provider>, tools: ToolRegistry) -> Self {
        let context = ToolContext::new().with_provider(provider.clone());
        Self {
            provider,
            tools: Arc::new(Mutex::new(tools)),
            config: AgentConfig::default(),
            callbacks: Arc::new(Mutex::new(AgentCallbacks::default())),
            context: Arc::new(context),
            store: None,
            depth: 0,
            fallback_chain: Vec::new(),
            fallback_specs: Vec::new(),
            fallback_active: tokio::sync::Mutex::new(None),
            smart_denial_streak: std::sync::atomic::AtomicU64::new(0),
            steer_inboxes: tokio::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// The configured fallback specs (for child-agent inheritance).
    pub fn fallback_specs(&self) -> Vec<String> {
        self.fallback_specs.clone()
    }

    /// P680: enqueue a steering message for the session's active turn
    /// (hermes `/steer`): injected after the next tool batch without
    /// interrupting the run. Returns the pending steer depth.
    pub async fn steer_message(&self, session_id: &str, message: String) -> usize {
        let mut inboxes = self.steer_inboxes.lock().await;
        let inbox = inboxes.entry(session_id.to_string()).or_default();
        inbox.push_back(message);
        inbox.len()
    }

    /// P680: drain the session's pending steer messages (the active
    /// turn calls this after every tool batch).
    pub async fn drain_steer_messages(&self, session_id: &str) -> Vec<String> {
        let mut inboxes = self.steer_inboxes.lock().await;
        inboxes
            .remove(session_id)
            .map(|inbox| inbox.into_iter().collect())
            .unwrap_or_default()
    }

    /// Configure the fallback chain from `"provider:model"` specs (hermes
    /// `fallback_providers`; ulnclaw `[model] fallbacks`). Malformed specs
    /// are skipped with a warning.
    pub fn with_fallback_specs(mut self, specs: &[String]) -> Self {
        self.fallback_specs = specs.to_vec();
        let mut chain = Vec::new();
        for spec in specs {
            match parse_fallback_spec(spec) {
                Some((provider_name, model)) => chain.push(FallbackEntry {
                    provider_name,
                    model,
                    provider: tokio::sync::OnceCell::new(),
                }),
                None => tracing::warn!("ignoring malformed fallback spec: {}", spec),
            }
        }
        self.fallback_chain = chain;
        self
    }

    /// Provide pre-built fallback providers (tests / custom wiring).
    pub fn with_fallback_providers(mut self, providers: Vec<(String, Arc<dyn Provider>)>) -> Self {
        self.fallback_specs = Vec::new();
        self.fallback_chain = providers
            .into_iter()
            .map(|(label, provider)| {
                let (provider_name, model) = parse_fallback_spec(&label)
                    .unwrap_or_else(|| ("custom".to_string(), provider.model().to_string()));
                let cell = tokio::sync::OnceCell::new();
                cell.set(provider).ok();
                FallbackEntry {
                    provider_name,
                    model,
                    provider: cell,
                }
            })
            .collect();
        self
    }

    /// Whether a fallback provider is available past the current position
    /// (hermes `_has_pending_fallback`).
    pub async fn has_pending_fallback(&self) -> bool {
        let index = *self.fallback_active.lock().await;
        index.map(|i| i + 1).unwrap_or(0) < self.fallback_chain.len()
    }

    /// Build (lazily) the provider instance for a fallback entry, with
    /// credential fallback to the main runtime key.
    async fn build_fallback_provider(&self, entry: &FallbackEntry) -> Result<Arc<dyn Provider>> {
        let config = &self.context.config;
        let api_key = config.resolve_api_key();
        if api_key.is_none() && !crate::provider::auxiliary::is_keyless(&entry.provider_name) {
            return Err(AgentError::config(format!(
                "fallback {}: no API key (set api_key in [model] or the provider env var)",
                entry.label()
            )));
        }
        let base_url = crate::config::default_base_url(&entry.provider_name);
        crate::provider::auxiliary::build_task_provider(
            &entry.provider_name,
            &entry.model,
            &base_url,
            api_key.as_deref(),
            config.model.max_retries,
        )
    }

    /// Set agent configuration
    pub fn with_config(mut self, config: AgentConfig) -> Self {
        self.config = config;
        self
    }

    /// Set callbacks
    pub fn with_callbacks(mut self, callbacks: AgentCallbacks) -> Self {
        self.callbacks = Arc::new(Mutex::new(callbacks));
        self
    }

    /// Replace callbacks after construction (REPL display wiring keeps the
    /// agent in an Arc; the callback set itself is behind a mutex).
    pub async fn set_callbacks(&self, callbacks: AgentCallbacks) {
        *self.callbacks.lock().await = callbacks;
    }

    /// Synchronous construction-time variant of [`Self::set_callbacks`]:
    /// messaging dispatchers install activity stamps on fresh agents
    /// whose callback mutex is uncontended. Returns false (no-op) when
    /// the lock is held.
    pub fn try_set_callbacks(&self, callbacks: AgentCallbacks) -> bool {
        match self.callbacks.try_lock() {
            Ok(mut guard) => {
                *guard = callbacks;
                true
            }
            Err(_) => false,
        }
    }

    /// Set the shared tool context (session id, workdir, callbacks...).
    pub fn with_tool_context(mut self, context: ToolContext) -> Self {
        let mut context = context;
        context.provider = Some(self.provider.clone());
        self.context = Arc::new(context);
        self
    }

    /// Attach the SQLite store (enables session_search + persistence).
    pub fn with_store(mut self, store: Arc<SqliteSessionStore>) -> Self {
        let context = Arc::make_mut(&mut self.context);
        context.store = Some(store.clone());
        self.store = Some(store);
        self
    }

    /// Replace the tool context wholesale (custom home/provider wiring);
    /// the attached session store, if any, carries over.
    pub fn with_context(mut self, mut context: ToolContext) -> Self {
        if let Some(store) = self.store.clone() {
            context.store = Some(store);
        }
        self.context = Arc::new(context);
        self
    }

    /// P457: provider accessor for gateway-side manual compression.
    pub fn provider(&self) -> Arc<dyn Provider> {
        self.provider.clone()
    }

    /// P457: configured context budget in tokens.
    pub fn context_budget_tokens(&self) -> usize {
        self.config.context_budget_tokens
    }

    pub fn tool_context(&self) -> Arc<ToolContext> {
        self.context.clone()
    }

    /// The attached SQLite store, if any.
    pub fn store(&self) -> Option<Arc<SqliteSessionStore>> {
        self.store.clone()
    }

    /// Names of all tools currently registered on this agent.
    pub fn tool_names(&self) -> Vec<String> {
        self.tools.try_lock().map(|r| r.names()).unwrap_or_default()
    }

    /// Reconnect all MCP servers from a freshly loaded config and rebuild
    /// the `mcp:*` tool surface in place (hermes `_reload_mcp`). The
    /// registry is read live on every LLM call, so the new surface takes
    /// effect on the next turn without a snapshot refresh.
    pub async fn reload_mcp(
        &self,
        config: &crate::config::UlncLawConfig,
    ) -> crate::mcp::ReloadReport {
        let mut registry = self.tools.lock().await;
        crate::mcp::reload_mcp_servers(&mut registry, config).await
    }

    /// Enabled toolset names on this agent (banner display).
    pub fn toolset_names(&self) -> Vec<String> {
        self.tools
            .try_lock()
            .map(|r| r.enabled_toolset_names())
            .unwrap_or_default()
    }

    /// Names of the tools belonging to one toolset (banner display).
    pub fn toolset_tool_names(&self, toolset: &str) -> Vec<String> {
        self.tools
            .try_lock()
            .map(|r| {
                r.toolset_tools(toolset)
                    .into_iter()
                    .map(|t| t.definition.name.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The model used for provider calls: per-task override (session model
    /// lock) → configured model → the provider's default.
    fn effective_model(&self) -> String {
        current_model_override()
            .or_else(|| self.config.model.clone())
            .unwrap_or_else(|| self.provider.model().to_string())
    }

    /// Wire this agent as its own delegation + cron runner. Call once after
    /// wrapping the agent in an Arc.
    /// Shared tool context (session key, home, delivery flags).
    pub fn context(&self) -> Arc<ToolContext> {
        self.context.clone()
    }

    pub fn wire_runners(self: &Arc<Self>) {
        self.context.set_subagent_runner(self.clone());
        self.context.set_cron_runner(self.clone());
    }

    /// Build the effective system prompt: configured prompt + memory
    /// injection (hermes injects MEMORY.md/USER.md into every turn).
    /// The system prompt split into its four tiers (P624 refactor so the
    /// context breakdown can attribute each piece — hermes
    /// `build_system_prompt_parts`). Returns (base, memory, env, volatile).
    fn prompt_tiers(&self) -> (String, String, String, String) {
        let base = self
            .config
            .system_prompt
            .clone()
            .unwrap_or_else(|| DEFAULT_SYSTEM_PROMPT.to_string());
        let memory = crate::tools::builtin::memory::load_memory_for_prompt(&self.context.home)
            .map(|memory| format!("## Persistent memory\n{}", memory))
            .unwrap_or_default();
        let mut env_section = format!(
            "## Environment\n- cwd: {}\n- home: {}",
            self.context.cwd().display(),
            self.context.home.display()
        );
        if self.config.environment_probe {
            // hermes tools/env_probe.py: one deterministic toolchain line,
            // only when something non-default is detected; fails open.
            let line = crate::env_probe::get_environment_probe_line(&self.config.terminal_backend);
            if !line.is_empty() {
                env_section.push_str(&format!("\n- {}", line));
            }
        }
        // Volatile timestamp block (hermes system_prompt.py): date-only so
        // the prompt stays byte-stable for the whole day (prefix-cache
        // stability, hermes PR #20451), rendered in the user's configured
        // timezone (hermes_time).
        let mut volatile =
            crate::hermes_time::conversation_started_line(self.context.config.timezone.as_deref());
        volatile.push_str(&format!("\nModel: {}", self.effective_model()));
        volatile.push_str(&format!("\nProvider: {}", self.provider.name()));
        (base, memory, env_section, volatile)
    }

    fn effective_system_prompt(&self) -> Option<String> {
        let (base, memory, env_section, volatile) = self.prompt_tiers();
        let parts: Vec<String> = [base, memory, env_section, volatile]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect();
        Some(parts.join("\n\n"))
    }

    /// P624: live pieces for the context-window breakdown (hermes
    /// `compute_session_context_breakdown` inputs): the non-memory system
    /// prompt tiers, the memory block, and builtin vs `mcp__*` tool-schema
    /// JSON read from the live registry.
    pub async fn context_breakdown_parts(&self) -> crate::context::breakdown::BreakdownParts {
        let (base, memory_block, env_section, volatile) = self.prompt_tiers();
        let system_prompt = [base, env_section, volatile]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        let (builtin_tools_json, mcp_tools_json) = {
            let registry = self.tools.lock().await;
            let mut builtin = Vec::new();
            let mut mcp = Vec::new();
            for definition in registry.definitions() {
                if definition.name.starts_with("mcp__") {
                    mcp.push(definition);
                } else {
                    builtin.push(definition);
                }
            }
            (
                serde_json::to_string(&builtin).unwrap_or_default(),
                serde_json::to_string(&mcp).unwrap_or_default(),
            )
        };
        crate::context::breakdown::BreakdownParts {
            system_prompt,
            memory_block,
            builtin_tools_json,
            mcp_tools_json,
            model: self.effective_model(),
        }
    }

    /// Run the agent with a user message
    pub async fn run(
        &self,
        user_message: &str,
        conversation_history: Option<Vec<Message>>,
    ) -> Result<RunResult> {
        self.run_with_session(user_message, conversation_history, None)
            .await
    }

    /// Run the agent, optionally resuming an existing session id instead of
    /// creating a fresh one (used by the HTTP gateway for continuity).
    pub async fn run_with_session(
        &self,
        user_message: &str,
        conversation_history: Option<Vec<Message>>,
        resume_session_id: Option<&str>,
    ) -> Result<RunResult> {
        self.run_with_session_images(
            user_message,
            Vec::new(),
            conversation_history,
            resume_session_id,
        )
        .await
    }

    /// Run with images natively attached to the user turn (P226
    /// multimodal injection — hermes media-injection parity). Images
    /// ride every model call of THIS turn (the tool loop re-sends the
    /// user turn each iteration, same as hermes keeping the media in
    /// the turn's content parts); they are not persisted into the
    /// session history.
    pub async fn run_with_session_images(
        &self,
        user_message: &str,
        images: Vec<crate::provider::MessageImage>,
        conversation_history: Option<Vec<Message>>,
        resume_session_id: Option<&str>,
    ) -> Result<RunResult> {
        // Per-turn primary restore (hermes `restore_primary_runtime`): a
        // fallback activated on the previous turn does not stick.
        *self.fallback_active.lock().await = None;

        let mut messages = Vec::new();
        let mut total_usage = Usage::default();
        let mut tool_calls_made = Vec::new();
        // hermes `is_first_turn` — no carried-over conversation history.
        let first_turn = conversation_history
            .as_ref()
            .map(|h| h.is_empty())
            .unwrap_or(true);

        // System prompt (with memory injection)
        if let Some(prompt) = self.effective_system_prompt() {
            messages.push(Message {
                role: Role::System,
                content: Some(prompt),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            });
        }

        if let Some(history) = conversation_history {
            messages.extend(history);
        }

        messages.push(Message {
            role: Role::User,
            content: Some(user_message.to_string()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        });

        // Persistence: create/resume a session row.
        let resuming = resume_session_id.is_some();
        let session_id = if self.config.persist {
            if let Some(ref store) = self.store {
                let model = self.effective_model();
                if let Some(sid) = resume_session_id {
                    if let Err(e) = store.ensure_session(
                        sid,
                        &self.config.source,
                        Some(&model),
                        Some(&self.context.cwd().display().to_string()),
                    ) {
                        warn!("session ensure failed: {}", e);
                    }
                    Some(sid.to_string())
                } else {
                    match store.create_session(
                        &self.config.source,
                        Some(&model),
                        Some(&self.context.cwd().display().to_string()),
                    ) {
                        Ok(id) => Some(id),
                        Err(e) => {
                            warn!("session create failed: {}", e);
                            None
                        }
                    }
                }
            } else {
                None
            }
        } else {
            None
        };
        if let Some(ref sid) = session_id {
            // When resuming, history rows already exist in the store — only
            // append the new user message (system prompt + history are known).
            let start = if resuming {
                messages.len().saturating_sub(1)
            } else {
                0
            };
            for message in &messages[start..] {
                if let Some(ref store) = self.store {
                    store.append_message(sid, message).ok();
                }
            }
        }

        // Plugin hook: pre_llm_call — injected context rides the current
        // turn's user message (hermes turn_context semantics: context is
        // appended to the user message, never the system prompt).
        if crate::plugins::has_hook("pre_llm_call") {
            let history_compact: Vec<serde_json::Value> = messages
                .iter()
                .filter(|m| m.role != Role::System)
                .map(|m| {
                    let content = m.content.clone().unwrap_or_default();
                    let truncated: String = content.chars().take(2000).collect();
                    serde_json::json!({
                        "role": m.role.to_string(),
                        "content": truncated,
                    })
                })
                .collect();
            let payload = crate::plugins::hook_payload(
                "pre_llm_call",
                session_id.as_deref().unwrap_or(""),
                &self.context.cwd(),
                vec![
                    ("user_message", serde_json::json!(user_message)),
                    (
                        "conversation_history",
                        serde_json::Value::Array(history_compact),
                    ),
                    ("is_first_turn", serde_json::json!(first_turn)),
                    ("model", serde_json::json!(self.effective_model())),
                    ("platform", serde_json::json!(self.config.source)),
                ],
                serde_json::json!({}),
            );
            let responses = crate::plugins::invoke_hook("pre_llm_call", payload).await;
            let injections = crate::plugins::context_injections(&responses);
            if !injections.is_empty() {
                // Bound oversized hook context via disk spill (hermes
                // hook_output_spill, P234): the blob is appended to every
                // subsequent API call, so a runaway hook must not inflate
                // the whole session.
                let spill_cfg = &self.context.config.hooks.output_spill;
                let home = crate::config::ulnclaw_home();
                let spilled: Vec<String> = injections
                    .iter()
                    .map(|context| {
                        crate::hook_output_spill::spill_if_oversized(
                            context,
                            session_id.as_deref(),
                            "hook",
                            spill_cfg,
                            &home,
                        )
                    })
                    .collect();
                if let Some(last) = messages.last_mut() {
                    if last.role == Role::User {
                        let mut content = last.content.clone().unwrap_or_default();
                        content.push_str("\n\n");
                        content.push_str(&spilled.join("\n\n"));
                        last.content = Some(content);
                    }
                }
            }
        }

        let compressor = crate::context::ContextCompressor::new(self.config.context_budget_tokens)
            .with_timezone(self.context.config.timezone.clone());

        // Main conversation loop
        for iteration in 0..self.config.max_iterations {
            debug!("Agent iteration {}", iteration);

            // Reset checkpoint per-turn dedup (hermes new_turn()).
            self.context.checkpoint_manager().new_turn();

            // Context compression when over budget. Auxiliary model routing:
            // hermes resolves `auxiliary.compression.{provider,model}` and
            // falls back to the main runtime when unset.
            if compressor.needs_compression(&messages) {
                let compressed = match crate::provider::auxiliary::resolve_aux_task(
                    &self.context.config,
                    crate::provider::auxiliary::TASK_COMPRESSION,
                    self.provider.clone(),
                ) {
                    Ok(aux) => {
                        compressor
                            .compress_with_model(
                                messages.clone(),
                                aux.provider.as_ref(),
                                &aux.model,
                            )
                            .await
                    }
                    Err(e) => {
                        tracing::warn!(
                            "auxiliary compression routing failed: {}; using main provider",
                            e
                        );
                        compressor
                            .compress_with_provider(messages.clone(), self.provider.as_ref())
                            .await
                    }
                };
                if let Some(compressed) = compressed {
                    messages = compressed;
                    debug!("Context compressed");
                }
            }

            // Plugin hooks around every provider request (hermes
            // pre_api_request / post_api_request / api_request_error).
            let api_started = std::time::Instant::now();
            if crate::plugins::has_hook("pre_api_request") {
                let request_char_count: usize = messages
                    .iter()
                    .map(|m| m.content.as_deref().map(str::len).unwrap_or(0))
                    .sum();
                let tool_count = self.tools.lock().await.definitions().len();
                let payload = crate::plugins::hook_payload(
                    "pre_api_request",
                    session_id.as_deref().unwrap_or(""),
                    &self.context.cwd(),
                    vec![
                        ("model", serde_json::json!(self.effective_model())),
                        ("provider", serde_json::json!(self.provider.name())),
                        ("api_call_count", serde_json::json!(iteration + 1)),
                        ("message_count", serde_json::json!(messages.len())),
                        ("tool_count", serde_json::json!(tool_count)),
                        (
                            "approx_input_tokens",
                            serde_json::json!(request_char_count / 4),
                        ),
                        ("request_char_count", serde_json::json!(request_char_count)),
                        ("max_tokens", serde_json::json!(self.config.max_tokens)),
                        ("platform", serde_json::json!(self.config.source)),
                    ],
                    serde_json::json!({}),
                );
                let _ = crate::plugins::invoke_hook("pre_api_request", payload).await;
            }
            let response = match self.call_provider(&messages, &images).await {
                Ok(response) => response,
                Err(error) => {
                    if crate::plugins::has_hook("api_request_error") {
                        let payload = crate::plugins::hook_payload(
                            "api_request_error",
                            session_id.as_deref().unwrap_or(""),
                            &self.context.cwd(),
                            vec![
                                ("model", serde_json::json!(self.effective_model())),
                                ("provider", serde_json::json!(self.provider.name())),
                                ("api_call_count", serde_json::json!(iteration + 1)),
                                ("error", serde_json::json!(error.to_string())),
                                (
                                    "api_duration",
                                    serde_json::json!(api_started.elapsed().as_secs_f64()),
                                ),
                                ("platform", serde_json::json!(self.config.source)),
                            ],
                            serde_json::json!({}),
                        );
                        let _ = crate::plugins::invoke_hook("api_request_error", payload).await;
                    }
                    return Err(error);
                }
            };
            if crate::plugins::has_hook("post_api_request") {
                let assistant_chars = response.content.as_deref().map(str::len).unwrap_or(0);
                let usage = response.usage.clone().unwrap_or_default();
                let payload = crate::plugins::hook_payload(
                    "post_api_request",
                    session_id.as_deref().unwrap_or(""),
                    &self.context.cwd(),
                    vec![
                        ("model", serde_json::json!(self.effective_model())),
                        ("provider", serde_json::json!(self.provider.name())),
                        ("api_call_count", serde_json::json!(iteration + 1)),
                        (
                            "api_duration",
                            serde_json::json!(api_started.elapsed().as_secs_f64()),
                        ),
                        ("finish_reason", serde_json::json!(response.finish_reason)),
                        ("message_count", serde_json::json!(messages.len())),
                        ("response_model", serde_json::json!(response.model)),
                        (
                            "usage",
                            serde_json::json!({
                                "input_tokens": usage.prompt_tokens,
                                "output_tokens": usage.completion_tokens,
                            }),
                        ),
                        (
                            "assistant_content_chars",
                            serde_json::json!(assistant_chars),
                        ),
                        (
                            "assistant_tool_call_count",
                            serde_json::json!(response.tool_calls.len()),
                        ),
                        ("platform", serde_json::json!(self.config.source)),
                    ],
                    serde_json::json!({}),
                );
                let _ = crate::plugins::invoke_hook("post_api_request", payload).await;
            }

            if let Some(ref usage) = response.usage {
                total_usage.merge(usage);
                if let (Some(store), Some(sid)) = (self.store.as_ref(), session_id.as_ref()) {
                    store
                        .update_usage(sid, usage.prompt_tokens, usage.completion_tokens, 0)
                        .ok();
                }
            }

            if !response.tool_calls.is_empty() {
                messages.push(Message {
                    role: Role::Assistant,
                    content: response.content.clone(),
                    tool_calls: Some(response.tool_calls.clone()),
                    tool_call_id: None,
                    name: None,
                });
                if let (Some(store), Some(sid)) = (self.store.as_ref(), session_id.as_ref()) {
                    let msg = messages.last().unwrap();
                    store.append_message(sid, msg).ok();
                }

                let tool_results = self.execute_tool_calls(&response.tool_calls).await?;

                // Three-layer tool-result persistence (hermes tool_executor
                // parity, P233): serialize each result, persist oversized
                // outputs to the sandbox as preview+path, then enforce the
                // aggregate per-turn budget.
                let mut result_contents: Vec<String> = tool_results
                    .iter()
                    .map(|result| serde_json::to_string(result).unwrap_or_default())
                    .collect();
                {
                    let budget = crate::tool_result_storage::BudgetConfig::default();
                    let backend = crate::environments::resolve(&self.context.config.terminal).ok();
                    for (content, tool_call) in
                        result_contents.iter_mut().zip(response.tool_calls.iter())
                    {
                        *content = crate::tool_result_storage::maybe_persist_tool_result(
                            content,
                            &tool_call.function.name,
                            &tool_call.id,
                            backend.as_ref(),
                            &budget,
                            None,
                            None,
                        )
                        .await;
                    }
                    let call_ids: Vec<String> = response
                        .tool_calls
                        .iter()
                        .map(|tool_call| tool_call.id.clone())
                        .collect();
                    crate::tool_result_storage::enforce_turn_budget(
                        &mut result_contents,
                        &call_ids,
                        backend.as_ref(),
                        &budget,
                        None,
                    )
                    .await;
                }

                for ((tool_call, result), content) in response
                    .tool_calls
                    .iter()
                    .zip(tool_results.iter())
                    .zip(result_contents.into_iter())
                {
                    let tool_message = Message {
                        role: Role::Tool,
                        content: Some(content),
                        tool_calls: None,
                        tool_call_id: Some(tool_call.id.clone()),
                        name: Some(tool_call.function.name.clone()),
                    };
                    messages.push(tool_message);
                    if let (Some(store), Some(sid)) = (self.store.as_ref(), session_id.as_ref()) {
                        store.append_message(sid, messages.last().unwrap()).ok();
                    }

                    let args: serde_json::Value =
                        serde_json::from_str(&tool_call.function.arguments)
                            .unwrap_or(serde_json::json!({}));
                    tool_calls_made.push(ToolCallRecord {
                        id: tool_call.id.clone(),
                        name: tool_call.function.name.clone(),
                        arguments: args,
                        result: result.clone(),
                    });
                }

                if let (Some(store), Some(sid)) = (self.store.as_ref(), session_id.as_ref()) {
                    store
                        .update_usage(sid, 0, 0, response.tool_calls.len() as u32)
                        .ok();
                }

                // P680: /steer injection (hermes steer parity) —
                // messages queued while this turn was running are
                // delivered right after the tool batch, before the
                // next model call.
                if let Some(sid) = session_id.as_deref() {
                    for steered in self.drain_steer_messages(sid).await {
                        let steer_turn = Message {
                            role: Role::User,
                            content: Some(steered),
                            tool_calls: None,
                            tool_call_id: None,
                            name: None,
                        };
                        messages.push(steer_turn);
                        if let Some(store) = self.store.as_ref() {
                            store.append_message(sid, messages.last().unwrap()).ok();
                        }
                    }
                }

                let callbacks = self.callbacks.lock().await;
                if let Some(ref on_step) = callbacks.on_step {
                    on_step(iteration as u32 + 1);
                }
                if let Some(ref on_activity) = callbacks.on_activity {
                    on_activity(&format!("agent step {}", iteration as u32 + 1));
                }
                continue;
            }

            let mut content = response.content.unwrap_or_default();
            if self.config.strip_thinking_blocks {
                content = strip_thinking_blocks(&content);
            }

            messages.push(Message {
                role: Role::Assistant,
                content: Some(content.clone()),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            });
            if let (Some(store), Some(sid)) = (self.store.as_ref(), session_id.as_ref()) {
                store.append_message(sid, messages.last().unwrap()).ok();
                store.end_session(sid, "complete").ok();

                // Fire-and-forget session title after the first exchange
                // (hermes maybe_auto_title) — background task, never adds
                // latency to the user-facing reply.
                let user_turns = messages.iter().filter(|m| m.role == Role::User).count();
                crate::title_generator::maybe_auto_title(
                    self.context.config.clone(),
                    store.clone(),
                    sid.clone(),
                    user_message.to_string(),
                    content.clone(),
                    user_turns,
                    self.provider.clone(),
                );
            }

            // Plugin hook: post_llm_call — the turn produced a final
            // response (hermes turn_finalizer fires it when final_response
            // is non-empty and the turn was not interrupted).
            if crate::plugins::has_hook("post_llm_call") {
                let payload = crate::plugins::hook_payload(
                    "post_llm_call",
                    session_id.as_deref().unwrap_or(""),
                    &self.context.cwd(),
                    vec![
                        ("user_message", serde_json::json!(user_message)),
                        ("assistant_response", serde_json::json!(content)),
                        ("model", serde_json::json!(self.effective_model())),
                        ("platform", serde_json::json!(self.config.source)),
                        ("completed", serde_json::json!(true)),
                        ("iterations", serde_json::json!(iteration + 1)),
                    ],
                    serde_json::json!({}),
                );
                let _ = crate::plugins::invoke_hook("post_llm_call", payload).await;
            }

            return Ok(RunResult {
                content,
                conversation: messages,
                usage: total_usage,
                iterations: iteration + 1,
                tool_calls: tool_calls_made,
                session_id,
            });
        }

        if let (Some(store), Some(sid)) = (self.store.as_ref(), session_id.as_ref()) {
            store.end_session(sid, "max_iterations").ok();
        }

        if crate::plugins::has_hook("post_llm_call") {
            let payload = crate::plugins::hook_payload(
                "post_llm_call",
                session_id.as_deref().unwrap_or(""),
                &self.context.cwd(),
                vec![
                    ("user_message", serde_json::json!(user_message)),
                    (
                        "assistant_response",
                        serde_json::json!("Reached the iteration budget before finishing."),
                    ),
                    ("model", serde_json::json!(self.effective_model())),
                    ("platform", serde_json::json!(self.config.source)),
                    ("completed", serde_json::json!(false)),
                    ("iterations", serde_json::json!(self.config.max_iterations)),
                ],
                serde_json::json!({}),
            );
            let _ = crate::plugins::invoke_hook("post_llm_call", payload).await;
        }

        Ok(RunResult {
            content: format!(
                "Reached the iteration budget ({} turns) before finishing.",
                self.config.max_iterations
            ),
            conversation: messages,
            usage: total_usage,
            iterations: self.config.max_iterations,
            tool_calls: tool_calls_made,
            session_id,
        })
    }

    /// Simple interface — just return the text response
    pub async fn chat(&self, user_message: &str) -> Result<String> {
        let result = self.run(user_message, None).await?;
        Ok(result.content)
    }

    async fn call_provider(
        &self,
        messages: &[Message],
        images: &[crate::provider::MessageImage],
    ) -> Result<crate::provider::ProviderResponse> {
        // Active runtime: primary provider, or the last activated fallback
        // (hermes keeps the fallback active until the next turn restores
        // the primary).
        let active_index = *self.fallback_active.lock().await;
        let result = self.call_on_active(messages, images, active_index).await;
        match result {
            Ok(response) => Ok(response),
            Err(error) => self.failover(messages, images, active_index, error).await,
        }
    }

    /// Execute one model call on the given runtime (None = primary).
    async fn call_on_active(
        &self,
        messages: &[Message],
        images: &[crate::provider::MessageImage],
        active_index: Option<usize>,
    ) -> Result<crate::provider::ProviderResponse> {
        match active_index {
            Some(index) => match self.fallback_chain.get(index) {
                Some(entry) => {
                    let provider = entry
                        .provider
                        .get_or_try_init(|| self.build_fallback_provider(entry))
                        .await?;
                    self.call_with(messages, images, provider.as_ref(), entry.model.clone())
                        .await
                }
                None => Err(AgentError::provider("fallback index out of range")),
            },
            None => {
                let provider = self.provider.clone();
                self.call_with(messages, images, provider.as_ref(), self.effective_model())
                    .await
            }
        }
    }

    /// Advance through the fallback chain after a failed model call
    /// (hermes `try_activate_fallback`). The first fallback that answers
    /// becomes the active runtime for the rest of this turn.
    async fn failover(
        &self,
        messages: &[Message],
        images: &[crate::provider::MessageImage],
        from_index: Option<usize>,
        mut error: AgentError,
    ) -> Result<crate::provider::ProviderResponse> {
        let start = from_index.map(|i| i + 1).unwrap_or(0);
        for index in start..self.fallback_chain.len() {
            let entry = &self.fallback_chain[index];
            tracing::warn!(
                "provider call failed ({}); trying fallback {} ({})",
                error,
                index + 1,
                entry.label()
            );
            let provider = match entry
                .provider
                .get_or_try_init(|| self.build_fallback_provider(entry))
                .await
            {
                Ok(provider) => provider.clone(),
                Err(build_error) => {
                    tracing::warn!("fallback {} unavailable: {}", entry.label(), build_error);
                    error = build_error;
                    continue;
                }
            };
            match self
                .call_with(messages, images, provider.as_ref(), entry.model.clone())
                .await
            {
                Ok(response) => {
                    *self.fallback_active.lock().await = Some(index);
                    return Ok(response);
                }
                Err(next_error) => error = next_error,
            }
        }
        Err(error)
    }

    /// One model call against an explicit provider + model (streaming when
    /// a stream consumer is attached and the provider supports it).
    async fn call_with(
        &self,
        messages: &[Message],
        images: &[crate::provider::MessageImage],
        provider: &dyn Provider,
        model: String,
    ) -> Result<crate::provider::ProviderResponse> {
        let tools = self.tools.lock().await;
        let tool_definitions = tools.definitions();

        let request = ProviderRequest {
            messages: messages.to_vec(),
            tools: tool_definitions,
            model,
            max_tokens: self.config.max_tokens,
            temperature: self.config.temperature,
            stream: STREAM_EMITTER.try_with(|_| ()).is_ok(),
            stop: None,
            images: if images.is_empty() {
                None
            } else {
                Some(images.to_vec())
            },
        };

        // Non-streaming path (no active stream consumer, or provider
        // doesn't support streaming).
        if !request.stream || !provider.supports_streaming() {
            return provider.chat_completion(request).await;
        }

        // Streaming path: accumulate chunks into a ProviderResponse while
        // emitting content deltas to the stream consumer.
        use futures::TryStreamExt;
        let mut stream = provider.chat_completion_stream(request).await?;
        let mut content = String::new();
        let mut reasoning = String::new();
        let mut tool_deltas: Vec<crate::provider::ToolCallDelta> = Vec::new();
        let mut finish_reason: Option<String> = None;
        let mut usage: Option<Usage> = None;
        let mut model: Option<String> = None;
        // Stateful reasoning/thinking scrubber for streamed deltas — port of
        // hermes `_fire_stream_delta` wiring: a per-delta strip loses tag
        // state across chunk boundaries (an open tag in one delta and the
        // reasoning prose in the next), so every delta is fed through the
        // state machine and the held-back tail is flushed after the stream.
        let mut think_scrubber = crate::think_scrubber::StreamingThinkScrubber::new();
        let scrub_stream = self.config.strip_thinking_blocks;
        while let Some(chunk) = stream.try_next().await? {
            if let Some(delta) = &chunk.delta_content {
                if !delta.is_empty() {
                    let visible = if scrub_stream {
                        think_scrubber.feed(delta)
                    } else {
                        delta.clone()
                    };
                    if !visible.is_empty() {
                        emit_stream_event(StreamEvent::Delta(visible.clone()));
                        {
                            let callbacks = self.callbacks.lock().await;
                            if let Some(ref on_delta) = callbacks.on_stream_delta {
                                on_delta(&visible);
                            }
                            if let Some(ref on_activity) = callbacks.on_activity {
                                on_activity("streaming response");
                            }
                        }
                        content.push_str(&visible);
                    }
                }
            }
            if let Some(delta) = &chunk.delta_reasoning {
                reasoning.push_str(delta);
            }
            tool_deltas.extend(chunk.tool_call_deltas);
            if chunk.finish_reason.is_some() {
                finish_reason = chunk.finish_reason;
            }
            if chunk.usage.is_some() {
                usage = chunk.usage;
            }
            if chunk.model.is_some() {
                model = chunk.model;
            }
        }
        // Flush the scrubber's held-back tail (e.g. a trailing '<' that
        // turned out not to be a tag) so it still reaches the UI — hermes
        // `_reset_stream_delivery_tracking` parity.
        if scrub_stream {
            let tail = think_scrubber.flush();
            if !tail.is_empty() {
                emit_stream_event(StreamEvent::Delta(tail.clone()));
                {
                    let callbacks = self.callbacks.lock().await;
                    if let Some(ref on_delta) = callbacks.on_stream_delta {
                        on_delta(&tail);
                    }
                    if let Some(ref on_activity) = callbacks.on_activity {
                        on_activity("streaming response");
                    }
                }
                content.push_str(&tail);
            }
        }
        let tool_calls = crate::provider::assemble_tool_calls(&tool_deltas);
        Ok(crate::provider::ProviderResponse {
            content: if content.is_empty() {
                None
            } else {
                Some(content)
            },
            tool_calls,
            usage,
            model: model.unwrap_or_else(|| provider.model().to_string()),
            reasoning: if reasoning.is_empty() {
                None
            } else {
                Some(reasoning)
            },
            finish_reason,
        })
    }

    /// Approval gate for terminal commands (port of hermes approval flow).
    async fn approval_check(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
    ) -> Option<serde_json::Value> {
        if tool_name != "terminal" || !self.config.approval {
            return None;
        }
        let command = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
        if command.is_empty() {
            return None;
        }
        // User-defined deny rules (hermes `approvals.deny`): unconditional,
        // matched BEFORE the mode=off / yolo bypass.
        if let Some(pattern) =
            crate::tools::approval::match_deny_glob(command, &self.context.config.approvals.deny)
        {
            return Some(serde_json::json!({
                "success": false,
                "error": format!(
                    "BLOCKED: this command matches your approvals.deny rule ({}). It cannot be approved by any mode — remove the rule from [approvals] deny to allow it.",
                    pattern
                ),
            }));
        }
        // Phase 1 (hermes approval.py): gather findings from the pattern
        // classifier AND the tirith content-level scanner before deciding.
        let pattern_confirm: Option<String> = match classify_command(command) {
            ApprovalDecision::Allow => None,
            ApprovalDecision::Block(reason) => {
                return Some(serde_json::json!({
                    "success": false,
                    "error": format!(
                        "BLOCKED: this command matches the hardline safety floor ({}). It cannot be approved. Use a safer alternative.",
                        reason
                    ),
                }));
            }
            ApprovalDecision::Confirm(reason) => Some(reason),
        };

        // Tirith pre-exec content scan (hermes tools/tirith_security.py):
        // homograph URLs, pipe-to-interpreter, terminal injection, ...
        // Block and warn both become approvable warnings (hermes semantics
        // since the "block without prompt" change); operational failures
        // fail-open/fail-closed inside the wrapper per `tirith_fail_open`.
        let tirith_command = command.to_string();
        let tirith_security = self.context.config.security.clone();
        let tirith_verdict = tokio::task::spawn_blocking(move || {
            crate::tirith::check_command_security(&tirith_command, &tirith_security)
        })
        .await
        .unwrap_or_default();
        let tirith_desc = match tirith_verdict.action {
            crate::tirith::TirithAction::Block | crate::tirith::TirithAction::Warn => {
                Some(crate::tirith::format_description(&tirith_verdict))
            }
            crate::tirith::TirithAction::Allow => None,
        };

        let reason: String = match (pattern_confirm, tirith_desc) {
            (Some(pattern), Some(tirith)) => format!("{pattern}; {tirith}"),
            (Some(pattern), None) => pattern,
            (None, Some(tirith)) => tirith,
            (None, None) => return None,
        };

        {
            let approvals = self.context.config.approvals.clone();
            let mode = crate::tools::approval::parse_approval_mode(&approvals.mode);

            // hermes `approvals.mode: off` bypass — the hardline floor
            // above still holds; only the confirm layer is skipped.
            if mode == crate::tools::approval::ApprovalMode::Off {
                return None;
            }

            // hermes cron approval context: no human can answer, so
            // `approvals.cron_mode` decides (fail-closed by default).
            if is_cron_context() {
                return match crate::tools::approval::parse_cron_mode(&approvals.cron_mode) {
                    crate::tools::approval::CronApprovalMode::Approve => None,
                    crate::tools::approval::CronApprovalMode::Deny => Some(serde_json::json!({
                        "success": false,
                        "error": format!(
                            "BLOCKED: this unattended (cron) run hit the approval gate ({}).                                  approvals.cron_mode is 'deny' and no human is present to consent.                                  Do not retry.",
                            reason
                        ),
                    })),
                };
            }

            // hermes `approvals.mode: smart` — auxiliary guardian LLM
            // verdict before any human prompt.
            if mode == crate::tools::approval::ApprovalMode::Smart {
                let (guard_provider, guard_model) =
                    match crate::provider::auxiliary::resolve_aux_task(
                        &self.context.config,
                        crate::provider::auxiliary::TASK_APPROVAL,
                        self.provider.clone(),
                    ) {
                        Ok(aux) => (aux.provider, aux.model),
                        Err(e) => {
                            tracing::warn!(
                                "auxiliary approval routing failed: {}; using main provider",
                                e
                            );
                            (self.provider.clone(), self.provider.model().to_string())
                        }
                    };
                let verdict = crate::tools::approval::smart_assess(
                    guard_provider.as_ref(),
                    &guard_model,
                    command,
                    &reason,
                    &approvals.smart_policy,
                )
                .await;
                match verdict {
                    crate::tools::approval::SmartVerdict::Approve => {
                        self.smart_denial_streak
                            .store(0, std::sync::atomic::Ordering::Relaxed);
                        tracing::info!("smart approval: auto-approved command ({})", reason);
                        return None;
                    }
                    crate::tools::approval::SmartVerdict::Deny
                        if self.context.approve.is_none() =>
                    {
                        let streak = self
                            .smart_denial_streak
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                            + 1;
                        let threshold = approvals.denial_breaker_threshold;
                        let error = if threshold > 0 && streak >= threshold as u64 {
                            // Circuit breaker tripped (hermes
                            // `denial_breaker_threshold`): escalate from
                            // "do not retry" to a hard stop.
                            format!(
                                    "BLOCKED by smart approval ({}): the guardian has now DENIED {} consecutive dangerous commands — this looks like a stuck loop. STOP pursuing this approach entirely: report the situation to the user and ask them to run the command manually (or approve it themselves). Do not retry in any form.",
                                    reason, streak
                                )
                        } else {
                            format!(
                                    "BLOCKED by smart approval ({}): the guardian assessed this                                      command as genuinely dangerous and no human is present to                                      override. Do NOT retry, rephrase, or reach the same outcome                                      via a different path.",
                                    reason
                                )
                        };
                        return Some(serde_json::json!({
                            "success": false,
                            "error": error,
                        }));
                    }
                    // Guardian DENY with a human available falls through
                    // to the prompt (one-operation override, hermes
                    // semantics); ESCALATE always prompts. A DENY still
                    // counts toward the circuit breaker either way.
                    crate::tools::approval::SmartVerdict::Deny => {
                        self.smart_denial_streak
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    crate::tools::approval::SmartVerdict::Escalate => {}
                }
            }

            {
                let callbacks = self.callbacks.lock().await;
                if let Some(ref hook) = callbacks.on_approval_request {
                    hook(command);
                }
                if let Some(ref on_activity) = callbacks.on_activity {
                    on_activity("approval requested");
                }
            }
            let approve = self.context.approve.clone();
            let approved = match approve {
                Some(callback) => callback(reason.to_string(), command.to_string()).await,
                None => false,
            };
            if approved {
                // Any approval resets the denial circuit breaker.
                self.smart_denial_streak
                    .store(0, std::sync::atomic::Ordering::Relaxed);
                None
            } else {
                Some(serde_json::json!({
                    "success": false,
                    "error": format!(
                        "Command rejected by approval policy ({}). The user declined to run it. Do not retry the same command.",
                        reason
                    ),
                }))
            }
        }
    }

    async fn execute_tool_calls(
        &self,
        tool_calls: &[crate::provider::ToolCall],
    ) -> Result<Vec<serde_json::Value>> {
        let mut results = Vec::new();

        for tool_call in tool_calls {
            let args: serde_json::Value = serde_json::from_str(&tool_call.function.arguments)
                .unwrap_or(serde_json::json!({}));

            {
                let callbacks = self.callbacks.lock().await;
                if let Some(ref on_tool_start) = callbacks.on_tool_start {
                    on_tool_start(&tool_call.function.name, &args);
                }
                if let Some(ref on_activity) = callbacks.on_activity {
                    on_activity(&format!("tool call: {}", tool_call.function.name));
                }
            }

            // Transparent checkpoint before file-mutating tools (hermes
            // checkpoint_manager hook): snapshot the project once per turn
            // before the first write_file/patch.
            if matches!(tool_call.function.name.as_str(), "write_file" | "patch") {
                let manager = self.context.checkpoint_manager();
                if manager.enabled() {
                    let target = args
                        .get("path")
                        .and_then(|v| v.as_str())
                        .map(|raw| self.context.resolve_path(raw))
                        .unwrap_or_else(|| self.context.cwd());
                    let workdir = manager.working_dir_for_path(&target);
                    manager
                        .ensure_checkpoint(
                            &workdir.to_string_lossy(),
                            &format!("before {} ({})", tool_call.function.name, target.display()),
                        )
                        .await;
                }
            }

            emit_stream_event(StreamEvent::ToolProgress {
                tool: tool_call.function.name.clone(),
                status: "started".to_string(),
            });
            emit_stream_event(StreamEvent::ToolStarted {
                name: tool_call.function.name.clone(),
                call_id: tool_call.id.clone(),
                arguments: tool_call.function.arguments.clone(),
            });

            // Plugin pre_tool_call hook — a block decision vetoes the call
            // before approval (hermes model_tools.py invoke_hook site).
            let hook_block = {
                let payload = crate::plugins::hook_payload(
                    "pre_tool_call",
                    &self.context.session_id,
                    &self.context.cwd(),
                    vec![
                        ("tool_name", serde_json::json!(tool_call.function.name)),
                        ("tool_input", args.clone()),
                    ],
                    serde_json::json!({"tool_call_id": tool_call.id}),
                );
                let responses = crate::plugins::invoke_hook("pre_tool_call", payload).await;
                crate::plugins::block_decision(&responses)
            };

            // Approval gate before dispatch.
            let result_value = if let Some(reason) = hook_block {
                serde_json::json!({
                    "success": false,
                    "blocked": true,
                    "error": format!("blocked by plugin hook: {reason}"),
                })
            } else if let Some(blocked) = self.approval_check(&tool_call.function.name, &args).await
            {
                blocked
            } else {
                let tools = self.tools.lock().await;
                match tools
                    .dispatch(&tool_call.function.name, args.clone(), self.context.clone())
                    .await
                {
                    Ok(value) => value,
                    Err(e) => {
                        warn!("Tool {} failed: {}", tool_call.function.name, e);
                        serde_json::json!({"error": e.to_string(), "success": false})
                    }
                }
            };

            // Plugin post_tool_call hook (observer; hermes emits result +
            // status for side effects like webhooks/notifications).
            {
                let payload = crate::plugins::hook_payload(
                    "post_tool_call",
                    &self.context.session_id,
                    &self.context.cwd(),
                    vec![
                        ("tool_name", serde_json::json!(tool_call.function.name)),
                        ("tool_input", args.clone()),
                    ],
                    serde_json::json!({
                        "tool_call_id": tool_call.id,
                        "result": result_value,
                        "status": if result_value.get("success").and_then(|v| v.as_bool()).unwrap_or(true)
                            && result_value.get("error").is_none() { "ok" } else { "error" },
                    }),
                );
                let _ = crate::plugins::invoke_hook("post_tool_call", payload).await;
            }

            {
                let callbacks = self.callbacks.lock().await;
                if let Some(ref on_tool_complete) = callbacks.on_tool_complete {
                    on_tool_complete(&tool_call.function.name, &result_value);
                }
                if let Some(ref on_activity) = callbacks.on_activity {
                    on_activity(&format!("tool result: {}", tool_call.function.name));
                }
            }
            emit_stream_event(StreamEvent::ToolProgress {
                tool: tool_call.function.name.clone(),
                status: "completed".to_string(),
            });
            emit_stream_event(StreamEvent::ToolCompleted {
                call_id: tool_call.id.clone(),
                result: serde_json::to_string(&result_value).unwrap_or_default(),
            });

            results.push(result_value);
        }

        Ok(results)
    }
}

#[async_trait]
impl SubAgentRunner for Agent {
    async fn run_subagent(&self, goal: &str, context: &str) -> Result<String> {
        let delegation = &self.context.config.delegation;
        if self.depth >= delegation.max_depth {
            return Err(AgentError::Tool(format!(
                "Delegation depth limit reached ({}); cannot spawn further sub-agents.",
                delegation.max_depth
            )));
        }

        // The child shares this agent's tool registry (Arc clone below).
        let child_session = if let Some(ref store) = self.store {
            store
                .create_child_session(
                    &self.context.session_id,
                    "delegate",
                    Some(self.provider.model()),
                )
                .ok()
        } else {
            None
        };

        let mut child_context = ToolContext::new()
            .with_session_id(
                child_session
                    .clone()
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            )
            .with_home(self.context.home.clone())
            .with_config(self.context.config.clone())
            .with_provider(self.provider.clone())
            .with_delegate_depth(self.depth + 1);
        child_context.workdir = self.context.workdir.clone();
        if let Some(ref store) = self.store {
            child_context = child_context.with_store(store.clone());
        }
        if let Some(ref approve) = self.context.approve {
            child_context = child_context.with_approve(approve.clone());
        }
        // Sub-agents get no clarify (no user present) and no nested runners.
        let child = Agent {
            provider: self.provider.clone(),
            tools: self.tools.clone(),
            config: AgentConfig {
                max_iterations: delegation.child_max_iterations,
                max_tokens: self.config.max_tokens,
                temperature: self.config.temperature,
                system_prompt: Some(
                    "You are a focused sub-agent. Complete the delegated goal completely and \
                     report a single final answer. You cannot ask the user questions."
                        .to_string(),
                ),
                strip_thinking_blocks: true,
                model: self.config.model.clone(),
                concurrent_tool_execution: self.config.concurrent_tool_execution,
                max_concurrent_tools: self.config.max_concurrent_tools,
                approval: self.config.approval,
                context_budget_tokens: self.config.context_budget_tokens,
                persist: false,
                source: "delegate".to_string(),
                environment_probe: self.config.environment_probe,
                terminal_backend: self.config.terminal_backend.clone(),
            },
            callbacks: Arc::new(Mutex::new(AgentCallbacks::default())),
            context: Arc::new(child_context),
            store: None,
            depth: self.depth + 1,
            fallback_chain: Vec::new(),
            fallback_specs: Vec::new(),
            fallback_active: tokio::sync::Mutex::new(None),
            smart_denial_streak: std::sync::atomic::AtomicU64::new(0),
            steer_inboxes: tokio::sync::Mutex::new(std::collections::HashMap::new()),
        };
        // Children inherit the fallback chain configuration.
        let child = child.with_fallback_specs(&self.fallback_specs());

        let prompt = if context.is_empty() {
            goal.to_string()
        } else {
            format!("{}\n\nContext:\n{}", goal, context)
        };
        let result = child.run(&prompt, None).await?;
        if let (Some(store), Some(sid)) = (self.store.as_ref(), child_session.as_ref()) {
            store
                .append_message(
                    &sid,
                    &Message {
                        role: Role::Assistant,
                        content: Some(result.content.clone()),
                        tool_calls: None,
                        tool_call_id: None,
                        name: None,
                    },
                )
                .ok();
        }
        Ok(result.content)
    }
}

#[async_trait]
impl CronRunner for Agent {
    async fn run_prompt(&self, prompt: &str, skills: &[String]) -> Result<String> {
        // Load requested skills into the prompt preamble.
        let mut preamble = String::new();
        let skills_dir = self.context.home.join("skills");
        for name in skills {
            if let Some(skill) = crate::skills::find_skill(&skills_dir, name) {
                let content =
                    std::fs::read_to_string(skill.path.join("SKILL.md")).unwrap_or_default();
                preamble.push_str(&format!("## Skill: {}\n{}\n\n", skill.name, content));
            }
        }
        let full_prompt = if preamble.is_empty() {
            prompt.to_string()
        } else {
            format!("{}\n\n# Task\n{}", preamble, prompt)
        };

        let cron_agent = Agent {
            provider: self.provider.clone(),
            tools: self.tools.clone(),
            config: AgentConfig {
                max_iterations: self.config.max_iterations,
                system_prompt: Some(
                    "You are running as an autonomous cron job. No user is present: never ask \
                     questions, complete the task with your best judgment, and put the primary \
                     user-facing content in your final response. Do NOT schedule more cron jobs."
                        .to_string(),
                ),
                approval: self.config.approval,
                persist: true,
                source: "cron".to_string(),
                ..self.config.clone()
            },
            callbacks: Arc::new(Mutex::new(AgentCallbacks::default())),
            context: self.context.clone(),
            store: self.store.clone(),
            depth: self.depth,
            fallback_chain: Vec::new(),
            fallback_specs: Vec::new(),
            fallback_active: tokio::sync::Mutex::new(None),
            smart_denial_streak: std::sync::atomic::AtomicU64::new(0),
            steer_inboxes: tokio::sync::Mutex::new(std::collections::HashMap::new()),
        };
        let cron_agent = cron_agent.with_fallback_specs(&self.fallback_specs());
        // Hermes cron approval context: unattended run — the approval gate
        // applies `approvals.cron_mode` instead of waiting for a human.
        let result = cron_scope(true, cron_agent.run(&full_prompt, None)).await?;
        Ok(result.content)
    }
}

/// Strip thinking/reasoning blocks from text
pub fn strip_thinking_blocks(text: &str) -> String {
    let mut result = text.to_string();
    for (open, close) in [("<thinking>", "</thinking>"), ("<think>", "</think>")] {
        while let Some(start) = result.find(open) {
            if let Some(end) = result[start..].find(close) {
                result = format!(
                    "{}{}",
                    &result[..start],
                    &result[start + end + close.len()..]
                );
            } else {
                result = result[..start].to_string();
                break;
            }
        }
    }
    result.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn test_parse_fallback_spec() {
        assert_eq!(
            parse_fallback_spec("openai:gpt-5.2"),
            Some(("openai".into(), "gpt-5.2".into()))
        );
        // Models may contain ':' (ollama tags).
        assert_eq!(
            parse_fallback_spec("ollama:qwen3:1.7b"),
            Some(("ollama".into(), "qwen3:1.7b".into()))
        );
        assert_eq!(parse_fallback_spec("openai"), None);
        assert_eq!(parse_fallback_spec(":model"), None);
        assert_eq!(parse_fallback_spec("provider:"), None);
    }

    #[tokio::test]
    async fn steer_inbox_queues_and_drains_per_session() {
        // P680: /steer messages accumulate per session and drain
        // atomically (the active turn picks them up after a tool
        // batch).
        let (provider, _) = counting(Some("ok"));
        let agent = Agent::new(provider, ToolRegistry::new());
        assert_eq!(agent.steer_message("s1", "slow down".to_string()).await, 1);
        assert_eq!(
            agent
                .steer_message("s1", "also check tests".to_string())
                .await,
            2
        );
        assert_eq!(
            agent.steer_message("s2", "other session".to_string()).await,
            1
        );
        let drained = agent.drain_steer_messages("s1").await;
        assert_eq!(
            drained,
            vec!["slow down".to_string(), "also check tests".to_string()]
        );
        // Draining again yields nothing; s2 keeps its message.
        assert!(agent.drain_steer_messages("s1").await.is_empty());
        assert_eq!(agent.drain_steer_messages("s2").await.len(), 1);
    }

    struct CountingProvider {
        reply: Option<String>,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Provider for CountingProvider {
        async fn chat_completion(
            &self,
            _request: crate::provider::ProviderRequest,
        ) -> Result<crate::provider::ProviderResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match &self.reply {
                Some(text) => Ok(crate::provider::ProviderResponse {
                    content: Some(text.clone()),
                    tool_calls: vec![],
                    usage: None,
                    model: "counting".into(),
                    reasoning: None,
                    finish_reason: Some("stop".into()),
                }),
                None => Err(AgentError::provider("primary down")),
            }
        }
        fn model(&self) -> &str {
            "counting-model"
        }
        fn name(&self) -> &str {
            "counting"
        }
    }

    fn counting(reply: Option<&str>) -> (Arc<CountingProvider>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = Arc::new(CountingProvider {
            reply: reply.map(str::to_string),
            calls: calls.clone(),
        });
        (provider, calls)
    }

    /// End-to-end hook firing: pre_api_request / post_api_request /
    /// post_llm_call around a successful provider call, api_request_error
    /// on failure. NOTE: this is the ONLY lib test that initializes the
    /// plugin runtime (a process-wide OnceCell) — keep it that way.
    #[tokio::test]
    async fn agent_fires_api_and_turn_hooks() {
        if cfg!(windows) {
            return;
        }
        let dir = std::env::temp_dir().join(format!(
            "ulnclaw-agent-hooks-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("hook.log");
        // Append every payload the agent pipes in; each line is one JSON
        // payload, so the event name is recoverable from hook_event_name.
        let script = dir.join("hook.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\ncat >> {}\necho\n", log.display()),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut config = crate::config::UlncLawConfig::default();
        for event in [
            "pre_api_request",
            "post_api_request",
            "api_request_error",
            "post_llm_call",
            "pre_llm_call",
        ] {
            config
                .hooks
                .events
                .insert(event.to_string(), vec![script.display().to_string()]);
        }
        // Accept consent so build_runtime registers the callbacks.
        config.hooks.auto_accept = true;
        let _warnings = crate::plugins::init(&dir, &config).await;

        // Successful turn: pre/post_api_request + pre/post_llm_call fire.
        let (provider, calls) = counting(Some("hooked-reply"));
        let mut agent_config = AgentConfig::default();
        agent_config.persist = false;
        let agent = Agent::new(provider, ToolRegistry::new()).with_config(agent_config);
        let result = agent.run("hello hooks", None).await.expect("run succeeds");
        assert_eq!(result.content, "hooked-reply");
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Failed provider: api_request_error fires.
        let (broken, _) = counting(None);
        let mut agent_config = AgentConfig::default();
        agent_config.persist = false;
        let agent = Agent::new(broken, ToolRegistry::new()).with_config(agent_config);
        assert!(agent.run("will fail", None).await.is_err());

        let logged = std::fs::read_to_string(&log).unwrap_or_default();
        for event in [
            "pre_llm_call",
            "pre_api_request",
            "post_api_request",
            "post_llm_call",
            "api_request_error",
        ] {
            assert!(
                logged.contains(&format!("\"hook_event_name\":\"{event}\"")),
                "expected {event} in hook log, got: {logged}"
            );
        }
        // pre_llm_call injection path: a context-returning hook appends to
        // the user message. Verify via the payload shape only (the script
        // above is a sink); context_injections aggregation is unit-tested
        // in plugins.rs.
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn fallback_activates_and_restores_per_turn() {
        let (primary, primary_calls) = counting(None);
        let (fallback, fallback_calls) = counting(Some("from-fallback"));
        let agent = Agent::new(primary.clone(), ToolRegistry::new())
            .with_fallback_providers(vec![("openai:backup-model".into(), fallback.clone())]);
        let mut config = AgentConfig::default();
        config.persist = false;
        let agent = agent.with_config(config);

        let result = agent.run("hello", None).await.expect("run via fallback");
        assert_eq!(result.content, "from-fallback");
        assert_eq!(primary_calls.load(Ordering::SeqCst), 1);
        assert_eq!(fallback_calls.load(Ordering::SeqCst), 1);

        // Next turn restores the primary first (which fails again) and
        // falls back again.
        let result = agent.run("again", None).await.expect("second run");
        assert_eq!(result.content, "from-fallback");
        assert_eq!(primary_calls.load(Ordering::SeqCst), 2);
        assert_eq!(fallback_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn exhausted_chain_surfaces_last_error() {
        let (primary, _) = counting(None);
        let (fallback, _) = counting(None);
        let agent = Agent::new(primary.clone(), ToolRegistry::new())
            .with_fallback_providers(vec![("openai:backup".into(), fallback.clone())]);
        let mut config = AgentConfig::default();
        config.persist = false;
        let agent = agent.with_config(config);
        let error = agent.run("hello", None).await.err().expect("all fail");
        assert!(error.to_string().contains("primary down"));
    }

    #[tokio::test]
    async fn chain_skips_broken_entries() {
        let (primary, _) = counting(None);
        let (broken, _) = counting(None);
        let (working, _) = counting(Some("third-time"));
        let agent = Agent::new(primary.clone(), ToolRegistry::new()).with_fallback_providers(vec![
            ("openai:broken".into(), broken.clone()),
            ("ollama:working".into(), working.clone()),
        ]);
        let mut config = AgentConfig::default();
        config.persist = false;
        let agent = agent.with_config(config);
        let result = agent.run("hello", None).await.expect("third entry answers");
        assert_eq!(result.content, "third-time");
    }

    #[test]
    fn test_strip_thinking() {
        assert_eq!(
            strip_thinking_blocks("hello <thinking>hm</thinking> world"),
            "hello  world"
        );
        assert_eq!(strip_thinking_blocks("<think>x</think>done"), "done");
    }

    // ------------------------------------------------------------------
    // Approval gate layers: mode off / cron / smart (hermes parity)
    // ------------------------------------------------------------------

    fn approvals_cfg(mode: &str, cron_mode: &str) -> crate::config::ApprovalsConfig {
        crate::config::ApprovalsConfig {
            timeout: 300,
            mode: mode.to_string(),
            cron_mode: cron_mode.to_string(),
            smart_policy: String::new(),
            denial_breaker_threshold: 3,
            deny: Vec::new(),
            mcp_reload_confirm: true,
        }
    }

    fn gate_agent(
        reply: Option<&str>,
        approvals: crate::config::ApprovalsConfig,
        human: bool,
    ) -> Agent {
        let (provider, _) = counting(reply);
        let mut config = crate::config::UlncLawConfig::default();
        config.approvals = approvals;
        let mut context = ToolContext::new().with_config(config);
        if human {
            context =
                context.with_approve(Arc::new(|_reason, _command| Box::pin(async move { true })));
        }
        Agent::new(provider, crate::tools::ToolRegistry::new())
            .with_config(AgentConfig {
                approval: true,
                ..Default::default()
            })
            .with_tool_context(context)
    }

    #[tokio::test]
    async fn approval_mode_off_auto_approves_confirm() {
        let agent = gate_agent(None, approvals_cfg("off", "deny"), false);
        let blocked = agent
            .approval_check(
                "terminal",
                &serde_json::json!({"command": "rm -rf ./build"}),
            )
            .await;
        assert!(blocked.is_none());
        // Hardline floor still holds even with mode off.
        let blocked = agent
            .approval_check("terminal", &serde_json::json!({"command": "rm -rf /"}))
            .await;
        assert!(blocked.unwrap()["error"]
            .as_str()
            .unwrap()
            .contains("hardline"));
    }

    #[tokio::test]
    async fn cron_context_respects_cron_mode() {
        let agent = gate_agent(None, approvals_cfg("manual", "deny"), true);
        let blocked = cron_scope(
            true,
            agent.approval_check(
                "terminal",
                &serde_json::json!({"command": "rm -rf ./build"}),
            ),
        )
        .await;
        let error = blocked.unwrap()["error"].as_str().unwrap().to_string();
        assert!(error.contains("cron"), "{}", error);

        let agent = gate_agent(None, approvals_cfg("manual", "approve"), false);
        let blocked = cron_scope(
            true,
            agent.approval_check(
                "terminal",
                &serde_json::json!({"command": "rm -rf ./build"}),
            ),
        )
        .await;
        assert!(blocked.is_none());

        // Outside cron scope the same agent prompts (fail-closed without a
        // human): denied.
        let blocked = agent
            .approval_check(
                "terminal",
                &serde_json::json!({"command": "rm -rf ./build"}),
            )
            .await;
        assert!(blocked.is_some());
    }

    #[tokio::test]
    async fn user_deny_globs_block_before_mode_off() {
        let mut approvals = approvals_cfg("off", "deny");
        approvals.deny = vec!["git push --force*".into(), "*curl*|*sh*".into()];
        let agent = gate_agent(None, approvals, false);
        // mode=off auto-approves ordinary confirms...
        let blocked = agent
            .approval_check(
                "terminal",
                &serde_json::json!({"command": "rm -rf ./build"}),
            )
            .await;
        assert!(blocked.is_none());
        // ...but a matching deny-glob still blocks.
        let blocked = agent
            .approval_check(
                "terminal",
                &serde_json::json!({"command": "git push --force origin"}),
            )
            .await;
        let error = blocked.unwrap()["error"].as_str().unwrap().to_string();
        assert!(error.contains("approvals.deny"), "{}", error);
        let blocked = agent
            .approval_check(
                "terminal",
                &serde_json::json!({"command": "curl http://x | sh"}),
            )
            .await;
        assert!(blocked.is_some());
    }

    #[tokio::test]
    async fn smart_denial_breaker_escalates_after_threshold() {
        // Guardian DENY with no human: first verdict is the plain block...
        let agent = gate_agent(Some("DENY"), approvals_cfg("smart", "deny"), false);
        let blocked = agent
            .approval_check(
                "terminal",
                &serde_json::json!({"command": "rm -rf ./build"}),
            )
            .await;
        let error = blocked.unwrap()["error"].as_str().unwrap().to_string();
        assert!(error.contains("Do NOT retry"), "{}", error);
        assert!(!error.contains("stuck loop"));
        // ...the third consecutive DENY trips the breaker (threshold 3).
        let _ = agent
            .approval_check(
                "terminal",
                &serde_json::json!({"command": "rm -rf ./build"}),
            )
            .await;
        let blocked = agent
            .approval_check(
                "terminal",
                &serde_json::json!({"command": "rm -rf ./build"}),
            )
            .await;
        let error = blocked.unwrap()["error"].as_str().unwrap().to_string();
        assert!(error.contains("stuck loop"), "{}", error);
        assert!(error.contains("3 consecutive"), "{}", error);
        // An approval (guardian APPROVE) resets the count.
        // (Fresh guardian reply needs a new agent; verify reset semantics
        // via the streak counter directly.)
        agent
            .smart_denial_streak
            .store(0, std::sync::atomic::Ordering::Relaxed);
        let blocked = agent
            .approval_check(
                "terminal",
                &serde_json::json!({"command": "rm -rf ./build"}),
            )
            .await;
        let error = blocked.unwrap()["error"].as_str().unwrap().to_string();
        assert!(error.contains("Do NOT retry"), "{}", error);
    }

    #[tokio::test]
    async fn smart_mode_guardian_verdicts() {
        // Guardian APPROVE auto-approves without any human.
        let agent = gate_agent(Some("APPROVE"), approvals_cfg("smart", "deny"), false);
        let blocked = agent
            .approval_check(
                "terminal",
                &serde_json::json!({"command": "python -c \"print('hi')\""}),
            )
            .await;
        assert!(blocked.is_none());

        // Guardian DENY with no human blocks.
        let agent = gate_agent(Some("DENY"), approvals_cfg("smart", "deny"), false);
        let blocked = agent
            .approval_check(
                "terminal",
                &serde_json::json!({"command": "rm -rf ./build"}),
            )
            .await;
        let error = blocked.unwrap()["error"].as_str().unwrap().to_string();
        assert!(error.contains("smart approval"), "{}", error);

        // Guardian ESCALATE with a human falls through to the prompt
        // (test human approves).
        let agent = gate_agent(Some("ESCALATE"), approvals_cfg("smart", "deny"), true);
        let blocked = agent
            .approval_check(
                "terminal",
                &serde_json::json!({"command": "rm -rf ./build"}),
            )
            .await;
        assert!(blocked.is_none());

        // Guardian offline -> escalate -> no human -> fail-closed deny.
        let agent = gate_agent(None, approvals_cfg("smart", "deny"), false);
        let blocked = agent
            .approval_check(
                "terminal",
                &serde_json::json!({"command": "rm -rf ./build"}),
            )
            .await;
        assert!(blocked.is_some());
    }
}

//! ToolContext — runtime state passed to every tool handler.
//!
//! Port of the `**kw` context hermes passes to handlers (task_id,
//! session_id, config, stores) plus the interactive callbacks (clarify,
//! approval) and the sub-agent runner used by delegation/cron.

use crate::config::UlncLawConfig;
use crate::error::Result;
use crate::provider::Provider;
use crate::session::sqlite::SqliteSessionStore;
use async_trait::async_trait;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, RwLock};

/// Callback used by the `clarify` tool: (question, choices, multi_select) -> answer.
pub type ClarifyFn = Arc<
    dyn Fn(String, Vec<String>, bool) -> Pin<Box<dyn Future<Output = Result<String>> + Send>>
        + Send
        + Sync,
>;

/// Callback used by the approval system: (reason, command) -> approved?
pub type ApproveFn =
    Arc<dyn Fn(String, String) -> Pin<Box<dyn Future<Output = bool> + Send>> + Send + Sync>;

/// Backend that can run delegated sub-agents (implemented by `Agent`).
#[async_trait]
pub trait SubAgentRunner: Send + Sync {
    /// Run a sub-agent with the given goal/context; returns its final answer.
    async fn run_subagent(&self, goal: &str, context: &str) -> Result<String>;
}

/// Backend that can run a cron job immediately (`cronjob action=run`).
#[async_trait]
pub trait CronRunner: Send + Sync {
    async fn run_prompt(&self, prompt: &str, skills: &[String]) -> Result<String>;
}

/// Shared context handed to every tool invocation.
#[derive(Clone)]
pub struct ToolContext {
    /// Session id this run belongs to.
    pub session_id: String,
    /// Session working directory (terminal `cd` updates it).
    pub workdir: Arc<Mutex<PathBuf>>,
    /// ulnclaw home directory.
    pub home: PathBuf,
    /// Loaded configuration.
    pub config: UlncLawConfig,
    /// SQLite state store (session_search, persistence).
    pub store: Option<Arc<SqliteSessionStore>>,
    /// Clarify callback (interactive frontends set this).
    pub clarify: Option<ClarifyFn>,
    /// Approval callback for dangerous commands.
    pub approve: Option<ApproveFn>,
    /// Delegation backend (set by Agent via wire_runners).
    subagent_runner: Arc<RwLock<Option<Arc<dyn SubAgentRunner>>>>,
    /// Cron immediate-run backend (set by Agent/CLI via wire_runners).
    cron_runner: Arc<RwLock<Option<Arc<dyn CronRunner>>>>,
    /// Chat provider (vision, compression summaries).
    pub provider: Option<Arc<dyn Provider>>,
    /// Snapshot of tool definitions (for tool_search).
    tool_definitions: Arc<std::sync::RwLock<Vec<crate::tools::ToolDefinition>>>,
    /// Transparent filesystem checkpoints (lazily built from config).
    checkpoints: Arc<std::sync::RwLock<Option<Arc<crate::checkpoint::CheckpointManager>>>>,
    /// Env vars allowed through the sandbox scrub (skill declarations +
    /// `[terminal] env_passthrough`); see `env_guard`.
    env_passthrough: Arc<Mutex<std::collections::HashSet<String>>>,
    /// Whether this session can receive detached (background) delegation
    /// results after the current turn ends — interactive REPL / persistent
    /// gateway sessions set it; one-shot runners do not (hermes
    /// `async_delivery_supported`). `delegate_task background=true` falls
    /// back to synchronous execution when unsupported.
    async_delivery: Arc<std::sync::atomic::AtomicBool>,
    /// Delegation depth of the agent owning this context (0 = top-level).
    /// Top-level delegations run in the background; orchestrator children
    /// (depth > 0) stay synchronous (hermes `_model_background_value`).
    delegate_depth: usize,
}

impl Default for ToolContext {
    fn default() -> Self {
        Self {
            session_id: uuid::Uuid::new_v4().to_string(),
            workdir: Arc::new(Mutex::new(
                std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            )),
            home: crate::config::ulnclaw_home(),
            config: UlncLawConfig::default(),
            store: None,
            clarify: None,
            approve: None,
            subagent_runner: Arc::new(RwLock::new(None)),
            cron_runner: Arc::new(RwLock::new(None)),
            provider: None,
            tool_definitions: Arc::new(std::sync::RwLock::new(Vec::new())),
            checkpoints: Arc::new(std::sync::RwLock::new(None)),
            env_passthrough: Arc::new(Mutex::new(std::collections::HashSet::new())),
            async_delivery: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            delegate_depth: 0,
        }
    }
}

impl ToolContext {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_session_id(mut self, id: impl Into<String>) -> Self {
        self.session_id = id.into();
        self
    }

    pub fn with_workdir(mut self, path: impl Into<PathBuf>) -> Self {
        self.workdir = Arc::new(Mutex::new(path.into()));
        self
    }

    pub fn with_home(mut self, path: impl Into<PathBuf>) -> Self {
        self.home = path.into();
        self
    }

    pub fn with_config(mut self, config: UlncLawConfig) -> Self {
        self.config = config;
        self
    }

    pub fn with_store(mut self, store: Arc<SqliteSessionStore>) -> Self {
        self.store = Some(store);
        self
    }

    pub fn with_clarify(mut self, clarify: ClarifyFn) -> Self {
        self.clarify = Some(clarify);
        self
    }

    pub fn with_approve(mut self, approve: ApproveFn) -> Self {
        self.approve = Some(approve);
        self
    }

    pub fn with_subagent_runner(self, runner: Arc<dyn SubAgentRunner>) -> Self {
        self.set_subagent_runner(runner);
        self
    }

    pub fn with_cron_runner(self, runner: Arc<dyn CronRunner>) -> Self {
        self.set_cron_runner(runner);
        self
    }

    /// Set the delegation backend (interior mutability — can be wired after
    /// the context is shared).
    pub fn set_subagent_runner(&self, runner: Arc<dyn SubAgentRunner>) {
        if let Ok(mut guard) = self.subagent_runner.write() {
            *guard = Some(runner);
        }
    }

    /// Set the cron immediate-run backend.
    pub fn set_cron_runner(&self, runner: Arc<dyn CronRunner>) {
        if let Ok(mut guard) = self.cron_runner.write() {
            *guard = Some(runner);
        }
    }

    /// Get the delegation backend if wired.
    pub fn subagent_runner(&self) -> Option<Arc<dyn SubAgentRunner>> {
        self.subagent_runner.read().ok().and_then(|g| g.clone())
    }

    /// Get the cron runner if wired.
    pub fn cron_runner(&self) -> Option<Arc<dyn CronRunner>> {
        self.cron_runner.read().ok().and_then(|g| g.clone())
    }

    /// Seed the sandbox env-passthrough allowlist (user config
    /// `[terminal] env_passthrough`). Protected credentials are refused
    /// (hermes GHSA-rhgp-j443-p4rf).
    pub fn with_env_passthrough(self, names: &[String]) -> Self {
        self.register_env_passthrough(names);
        self
    }

    /// Register env vars as allowed in sandboxed child environments
    /// (skill `required_environment_variables` or user config). Returns
    /// the accepted names; provider credentials are refused.
    pub fn register_env_passthrough(&self, names: &[String]) -> Vec<String> {
        let mut guard = self.env_passthrough.lock().unwrap();
        crate::env_guard::register_env_passthrough(&mut guard, names)
    }

    /// Snapshot of the current passthrough allowlist.
    pub fn env_passthrough_snapshot(&self) -> std::collections::HashSet<String> {
        self.env_passthrough.lock().unwrap().clone()
    }

    /// Mark this session able to receive background-delegation results.
    pub fn with_async_delivery(mut self, enabled: bool) -> Self {
        self.async_delivery = Arc::new(std::sync::atomic::AtomicBool::new(enabled));
        self
    }

    /// Flip async-delivery support on a shared context (gateway startup).
    pub fn set_async_delivery(&self, enabled: bool) {
        self.async_delivery
            .store(enabled, std::sync::atomic::Ordering::SeqCst);
    }

    /// True when background delegations can deliver results to this session.
    pub fn async_delivery(&self) -> bool {
        self.async_delivery
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Set the delegation depth of the owning agent (children > 0).
    pub fn with_delegate_depth(mut self, depth: usize) -> Self {
        self.delegate_depth = depth;
        self
    }

    /// Delegation depth (0 = top-level agent).
    pub fn delegate_depth(&self) -> usize {
        self.delegate_depth
    }

    pub fn with_provider(mut self, provider: Arc<dyn Provider>) -> Self {
        self.provider = Some(provider);
        self
    }

    /// Refresh the tool-catalog snapshot (call after registration).
    pub fn set_tool_definitions(&self, defs: Vec<crate::tools::ToolDefinition>) {
        if let Ok(mut guard) = self.tool_definitions.write() {
            *guard = defs;
        }
    }

    /// Snapshot of registered tool definitions (for tool_search).
    pub fn tool_registry_snapshot(&self) -> Vec<crate::tools::ToolDefinition> {
        self.tool_definitions
            .read()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// Checkpoint manager for this context (lazily built from `home` +
    /// `[checkpoints]` config on first use).
    pub fn checkpoint_manager(&self) -> Arc<crate::checkpoint::CheckpointManager> {
        if let Some(manager) = self.checkpoints.read().ok().and_then(|g| g.clone()) {
            return manager;
        }
        let manager = Arc::new(crate::checkpoint::CheckpointManager::new(
            self.home.join("checkpoints"),
            &self.config.checkpoints,
        ));
        if let Ok(mut guard) = self.checkpoints.write() {
            *guard = Some(manager.clone());
        }
        manager
    }

    /// Current working directory snapshot.
    pub fn cwd(&self) -> PathBuf {
        self.workdir.lock().map(|g| g.clone()).unwrap_or_default()
    }

    /// Update the session working directory.
    pub fn set_cwd(&self, path: PathBuf) {
        if let Ok(mut guard) = self.workdir.lock() {
            *guard = path;
        }
    }

    /// Resolve a possibly-relative path against the session cwd.
    /// Also expands a leading `~` (hermes path convention).
    pub fn resolve_path(&self, raw: &str) -> PathBuf {
        let expanded = if let Some(stripped) = raw.strip_prefix("~/") {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(stripped)
        } else if raw == "~" {
            dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
        } else {
            PathBuf::from(raw)
        };
        if expanded.is_absolute() {
            expanded
        } else {
            self.cwd().join(expanded)
        }
    }
}

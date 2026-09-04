//! ulnclaw - A Rust-based AI agent engine inspired by Hermes Agent
//!
//! # Overview
//!
//! ulnclaw provides a complete agent loop with tool calling, multi-provider
//! support, session persistence, and context management. It's designed to be
//! embedded into applications or used as a library.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────┐
//! │                        Entry Points                              │
//! │  Agent::run()    Agent::chat()    Custom integration            │
//! └──────────┬──────────────┬───────────────────────┬───────────────┘
//!            │              │                       │
//!            ▼              ▼                       ▼
//! ┌─────────────────────────────────────────────────────────────────┐
//! │                     Agent (conversation loop)                    │
//! │                                                                  │
//! │  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐           │
//! │  │ Prompt       │  │ Provider     │  │ Tool         │           │
//! │  │ Builder      │  │ Resolution   │  │ Dispatch     │           │
//! │  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘           │
//! │         │                 │                 │                   │
//! │  ┌──────┴───────┐  ┌──────┴───────┐  ┌──────┴───────┐           │
//! │  │ Compression  │  │ OpenAI       │  │ Tool Registry│           │
//! │  │ & Caching    │  │ Compatible   │  │ (70+ tools)  │           │
//! │  └──────────────┘  └──────────────┘  └──────────────┘           │
//! └─────────┴─────────────────┴─────────────────┴───────────────────┘
//!            │                                    │
//!            ▼                                    ▼
//! ┌───────────────────┐              ┌──────────────────────┐
//! │ Session Storage   │              │ Tool Handlers         │
//! │ (In-memory/SQLite)│              │ Custom implementations│
//! └───────────────────┘              └──────────────────────┘
//! ```
//!
//! # Example
//!
//! ```rust,no_run
//! use ulnclaw::prelude::*;
//! use std::sync::Arc;
//!
//! #[tokio::main]
//! async fn main() -> Result<()> {
//!     // Create provider
//!     let provider = OpenAiProvider::builder()
//!         .endpoint("https://api.openai.com")
//!         .api_key("sk-...")
//!         .model("gpt-4o")
//!         .build()?;
//!
//!     // Create tool registry
//!     let mut tools = ToolRegistry::new();
//!     tools.register(tool("get_time")
//!         .description("Get current time")
//!         .handler(|_args, _ctx| async move { Ok(json!({"time": "now"})) })
//!         .build()?);
//!
//!     // Create agent
//!     let agent = Agent::new(Arc::new(provider), tools)
//!         .with_config(AgentConfig {
//!             system_prompt: Some("You are a helpful assistant.".to_string()),
//!             ..Default::default()
//!         });
//!
//!     // Run
//!     let response = agent.chat("What time is it?").await?;
//!     println!("{}", response);
//!     Ok(())
//! }
//! ```

pub mod a2a;
pub mod acp_adapter;
pub mod active_sessions;
pub mod agent;
pub mod agent_import;
pub mod ansi;
pub mod approval_gateway;
pub mod approvals_cmd;
pub mod async_delegation;
pub mod backup;
pub mod banner;
pub mod batch_runner;
pub mod binary_ext;
pub mod browser;
pub mod bundles;
pub mod buzz;
pub mod cgroup_cleanup;
pub mod channel_directory;
pub mod checkpoint;
pub mod clarify_gateway;
pub mod clipboard;
pub mod code_skew;
pub mod computer_use;
pub mod config;
pub mod config_cmd;
pub mod context;
pub mod credential_pool;
pub mod cron;
pub mod curator;
pub mod cwd_placeholder;
pub mod dead_targets;
pub mod debug_cmd;
pub mod delivery_ledger;
pub mod desktop;
pub mod desktop_bridge;
pub mod dingtalk;
pub mod discord_tool;
pub mod display_config;
pub mod doctor;
pub mod drain_control;
pub mod dump;
pub mod egress_cmd;
pub mod email_platform;
pub mod env_guard;
pub mod env_probe;
pub mod environments;
pub mod error;
pub mod event_hooks;
pub mod fallback;
pub mod feishu;
pub mod feishu_comment;
pub mod feishu_doc_tool;
pub mod feishu_meeting;
pub mod feishu_ws;
pub mod focus_view;
pub mod gateway;
pub mod gateway_pidfile;
pub mod gateway_ws;
pub mod git_diff;
pub mod goals;
pub mod google_chat;
pub mod google_chat_oauth;
pub mod gui_cmd;
pub mod hermes_time;
pub mod homeassistant;
pub mod hook_output_spill;
pub mod init_command;
pub mod insights;
pub mod irc;
pub mod iron_proxy;
pub mod kanban;
pub mod kanban_diagnostics;
pub mod kanban_triage;
pub mod learn_prompt;
pub mod learning_graph;
pub mod learning_graph_render;
pub mod learning_mutations;
pub mod lifecycle_ledger;
pub mod line;
pub mod logs;
pub mod managed_gateway;
pub mod matrix;
pub mod mattermost;
pub mod mcp;
pub mod mcp_catalog;
pub mod mcp_serve;
pub mod media_cache;
pub mod memory_cmd;
pub mod memory_monitor;
pub mod message_timestamps;
pub mod messaging;
pub mod migrate;
pub mod mirror;
pub mod moa;
pub mod model_cmd;
pub mod model_inventory;
pub mod models_dev;
pub mod monitoring;
pub mod nostr_auth;
pub mod ntfy;
pub mod oauth;
pub mod pairing;
pub mod pets;
pub mod pets_atlas;
pub mod pets_generate;
pub mod photon;
pub mod platform_slash;
pub mod plugins;
pub mod process_ctl;
pub mod profile_routing;
pub mod profiles_cmd;
pub mod projects_db;
pub mod projects_scan;
pub mod prompt_size;
pub mod prompt_stash;
pub mod provider;
pub mod proxy_cmd;
pub mod qqbot;
pub mod raft;
pub mod readiness;
pub mod redact;
pub mod response_filters;
pub mod restart_loop_guard;
pub mod runtime_footer;
pub mod secret_scope;
pub mod secrets;
pub mod secrets_cache;
pub mod secrets_cmd;
pub mod security_audit;
pub mod send_message_tool;
pub mod session;
pub mod session_activity;
pub mod session_export;
pub mod session_stall;
pub mod setup_cmd;
pub mod shutdown_flush;
pub mod shutdown_forensics;
pub mod shutdown_watchdog;
pub mod signal;
pub mod simplex;
pub mod skill_usage;
pub mod skills;
pub mod skills_sync;
pub mod skin;
pub mod slack_cli;
pub mod slash_access;
pub mod slash_confirm;
pub mod sms;
pub mod spotify_auth;
pub mod spotify_tool;
pub mod status;
pub mod status_phrases;
pub mod streaming_tts;
pub mod stt;
pub mod systemd_notify;
pub mod teams;
pub mod think_scrubber;
pub mod tips;
pub mod tirith;
pub mod title_generator;
pub mod tool_result_storage;
pub mod tools;
pub mod toolsets;
pub mod tts;
pub mod tui_text;
pub mod turn_lease;
pub mod uninstall;
pub mod update;
pub mod url_safety;
pub mod video_gen;
pub mod video_gen_backends;
pub mod video_gen_xai;
pub mod webhook_platforms;
pub mod webhook_subscriptions;
pub mod wecom;
pub mod weixin;
pub mod whatsapp;
pub mod whatsapp_bridge;
pub mod whatsapp_cloud_setup;
pub mod whatsapp_identity;
pub mod yuanbao;
pub mod yuanbao_proto;
pub mod yuanbao_sticker;
pub mod yuanbao_tool;

// Re-export core types for convenience
pub use agent::{Agent, AgentCallbacks, AgentConfig, RunResult, ToolCallRecord};
pub use config::UlncLawConfig;
pub use context::{ContextCompressor, PromptBuilder};
pub use error::{AgentError, Result};
pub use provider::openai::OpenAiProvider;
pub use provider::{
    FunctionCall, Message, Provider, ProviderConfig, ProviderKind, ProviderRequest,
    ProviderResponse, Role, ToolCall, Usage,
};
pub use session::sqlite::SqliteSessionStore;
pub use session::{MemorySessionStore, Session, SessionMetadata, SessionStore};
pub use tools::builtin::register_builtin_tools;
pub use tools::context::ToolContext;
pub use tools::{tool, Tool, ToolBuilder, ToolDefinition, ToolHandler, ToolRegistry, ToolResult};

/// Prelude module - convenient imports for common use cases
pub mod prelude {
    pub use crate::agent::{Agent, AgentConfig, RunResult};
    pub use crate::error::{AgentError, Result};
    pub use crate::provider::openai::OpenAiProvider;
    pub use crate::provider::{Message, Provider, Role};
    pub use crate::tools::{tool, ToolRegistry};
    pub use serde_json::json;
    pub use std::sync::Arc;
}

/// Version information
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const NAME: &str = env!("CARGO_PKG_NAME");

/// Get version string
pub fn version() -> &'static str {
    VERSION
}

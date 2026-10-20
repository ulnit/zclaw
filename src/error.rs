//! Error types for ulnclaw agent engine

use thiserror::Error;

/// Result type alias for ulnclaw operations
pub type Result<T> = std::result::Result<T, AgentError>;

/// Core error type for the agent engine
#[derive(Error, Debug)]
pub enum AgentError {
    /// Provider-related errors (API failures, auth, rate limits)
    #[error("Provider error: {0}")]
    Provider(String),

    /// The provider rejected the request because the context is too large
    /// (hermes `FailoverReason.context_overflow` — "Context too large —
    /// compress, not failover").
    ///
    /// 🔴 A distinct variant rather than a `Provider(String)` that the agent
    /// would have to string-match: the recovery is **compact and retry**, not
    /// burning a fallback provider. Keeping the classification on the error type
    /// is what lets the loop branch on it without re-parsing prose (P3-6).
    #[error("Context overflow: {0}")]
    ContextOverflow(String),

    /// Tool execution errors
    #[error("Tool error: {0}")]
    Tool(String),

    /// Tool not found in registry
    #[error("Tool not found: {0}")]
    ToolNotFound(String),

    /// Session persistence errors
    #[error("Session error: {0}")]
    Session(String),

    /// Context management errors
    #[error("Context error: {0}")]
    Context(String),

    /// Configuration errors
    #[error("Config error: {0}")]
    Config(String),

    /// Iteration budget exceeded
    #[error("Iteration limit exceeded: {0} iterations")]
    IterationLimit(usize),

    /// HTTP/network errors
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    /// JSON serialization errors
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// Generic internal errors
    #[error("Internal error: {0}")]
    Internal(String),
}

impl AgentError {
    pub fn provider(msg: impl Into<String>) -> Self {
        Self::Provider(msg.into())
    }

    pub fn tool(msg: impl Into<String>) -> Self {
        Self::Tool(msg.into())
    }

    pub fn session(msg: impl Into<String>) -> Self {
        Self::Session(msg.into())
    }

    pub fn config(msg: impl Into<String>) -> Self {
        Self::Config(msg.into())
    }

    /// Whether this error means "the context was too large — compact and retry",
    /// i.e. hermes `FailoverReason.context_overflow` (`should_compress=True`).
    /// The agent loop branches on this instead of failing over (P3-6).
    pub fn is_context_overflow(&self) -> bool {
        matches!(self, Self::ContextOverflow(_))
    }
}

impl From<std::io::Error> for AgentError {
    fn from(e: std::io::Error) -> Self {
        Self::Internal(e.to_string())
    }
}

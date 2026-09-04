//! Session storage - conversation persistence with lineage tracking
//!
//! Inspired by Hermes Agent's hermes_state.py - SQLite session storage
//! with FTS5 full-text search and session lineage.

pub mod export;
pub mod filters;
pub mod recap;
pub mod recovery;
pub mod repair;
pub mod retitle;
pub mod sqlite;

pub use sqlite::SqliteSessionStore;

use crate::error::{AgentError, Result};
use crate::provider::Message;
use serde::{Deserialize, Serialize};

/// A conversation session
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub conversation_id: String,
    pub messages: Vec<Message>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    /// Parent session ID (for compressed sessions)
    pub parent_id: Option<String>,
    /// Session metadata
    pub metadata: SessionMetadata,
}

/// Session metadata
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionMetadata {
    pub user_id: Option<String>,
    pub platform: Option<String>,
    pub model: Option<String>,
    pub total_tokens: Option<u32>,
    pub iteration_count: Option<u32>,
}

/// Session store trait - abstraction for persistence backends
pub trait SessionStore: Send + Sync {
    /// Save a session
    fn save_session(&self, session: &Session) -> Result<()>;

    /// Load a session by ID
    fn load_session(&self, session_id: &str) -> Result<Option<Session>>;

    /// List recent sessions
    fn list_sessions(&self, limit: usize) -> Result<Vec<Session>>;

    /// Delete a session
    fn delete_session(&self, session_id: &str) -> Result<()>;

    /// Search sessions by content (full-text search)
    fn search_sessions(&self, query: &str, limit: usize) -> Result<Vec<Session>>;
}

/// In-memory session store (for testing and simple use cases)
pub struct MemorySessionStore {
    sessions: std::sync::Mutex<std::collections::HashMap<String, Session>>,
}

impl Default for MemorySessionStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MemorySessionStore {
    pub fn new() -> Self {
        Self {
            sessions: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }
}

impl SessionStore for MemorySessionStore {
    fn save_session(&self, session: &Session) -> Result<()> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|e| AgentError::Session(format!("Failed to acquire lock: {}", e)))?;
        sessions.insert(session.id.clone(), session.clone());
        Ok(())
    }

    fn load_session(&self, session_id: &str) -> Result<Option<Session>> {
        let sessions = self
            .sessions
            .lock()
            .map_err(|e| AgentError::Session(format!("Failed to acquire lock: {}", e)))?;
        Ok(sessions.get(session_id).cloned())
    }

    fn list_sessions(&self, limit: usize) -> Result<Vec<Session>> {
        let sessions = self
            .sessions
            .lock()
            .map_err(|e| AgentError::Session(format!("Failed to acquire lock: {}", e)))?;
        let mut all: Vec<Session> = sessions.values().cloned().collect();
        all.sort_by(|a, b| b.updated_at_ms.cmp(&a.updated_at_ms));
        all.truncate(limit);
        Ok(all)
    }

    fn delete_session(&self, session_id: &str) -> Result<()> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|e| AgentError::Session(format!("Failed to acquire lock: {}", e)))?;
        sessions.remove(session_id);
        Ok(())
    }

    fn search_sessions(&self, query: &str, limit: usize) -> Result<Vec<Session>> {
        let sessions = self
            .sessions
            .lock()
            .map_err(|e| AgentError::Session(format!("Failed to acquire lock: {}", e)))?;
        let query_lower = query.to_lowercase();
        let mut results: Vec<Session> = sessions
            .values()
            .filter(|s| {
                s.messages.iter().any(|m| {
                    m.content
                        .as_ref()
                        .map(|c| c.to_lowercase().contains(&query_lower))
                        .unwrap_or(false)
                })
            })
            .cloned()
            .collect();
        results.sort_by(|a, b| b.updated_at_ms.cmp(&a.updated_at_ms));
        results.truncate(limit);
        Ok(results)
    }
}

/// Create a new session with a unique ID
pub fn new_session(conversation_id: &str) -> Session {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    Session {
        id: uuid::Uuid::new_v4().to_string(),
        conversation_id: conversation_id.to_string(),
        messages: Vec::new(),
        created_at_ms: now,
        updated_at_ms: now,
        parent_id: None,
        metadata: SessionMetadata::default(),
    }
}

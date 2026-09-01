//! SQLite persistence — zclaw_memory.db in workspace_dir.
//! Stores sessions and messages so getSessions/getMessages survive restarts.

use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::Mutex;

pub struct MemoryStore {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Session {
    pub id: String,
    pub title: String,
    pub model_name: String,
    pub updated_at: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Message {
    pub role: String,
    pub content: String,
    pub created_at: i64,
}

impl MemoryStore {
    pub fn open(workspace_dir: &str) -> anyhow::Result<Self> {
        std::fs::create_dir_all(workspace_dir).ok();
        let path = Path::new(workspace_dir).join("zclaw_memory.db");
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL DEFAULT '',
                model_name TEXT NOT NULL DEFAULT '',
                updated_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS messages (
                session_id TEXT NOT NULL,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                created_at INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id);
             CREATE TABLE IF NOT EXISTS memories (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                key TEXT UNIQUE NOT NULL,
                content TEXT NOT NULL,
                category TEXT NOT NULL DEFAULT 'core',
                session_id TEXT,
                created_at INTEGER NOT NULL
             );
             CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
                key, content, category
             );",
        )?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn now_ms() -> i64 {
        chrono::Utc::now().timestamp_millis()
    }

    /// Ensure a session exists; create with title from first user message if needed.
    pub fn touch_session(&self, session_id: &str, title_hint: &str, model: &str) {
        let conn = self.conn.lock().unwrap();
        let exists: bool = conn
            .query_row(
                "SELECT 1 FROM sessions WHERE id = ?1",
                params![session_id],
                |r| r.get::<_, i64>(0),
            )
            .optional()
            .unwrap_or(None)
            .is_some();
        let ts = Self::now_ms();
        if exists {
            conn.execute(
                "UPDATE sessions SET updated_at = ?2 WHERE id = ?1",
                params![session_id, ts],
            ).ok();
        } else {
            let title: String = if title_hint.is_empty() {
                "新对话".to_string()
            } else {
                title_hint.chars().take(30).collect()
            };
            conn.execute(
                "INSERT INTO sessions (id, title, model_name, updated_at) VALUES (?1, ?2, ?3, ?4)",
                params![session_id, title, model, ts],
            ).ok();
        }
    }

    pub fn save_message(&self, session_id: &str, role: &str, content: &str) {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO messages (session_id, role, content, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![session_id, role, content, Self::now_ms()],
        ).ok();
    }

    pub fn list_sessions(&self) -> Vec<Session> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT id, title, model_name, updated_at FROM sessions ORDER BY updated_at DESC LIMIT 50")
            .unwrap();
        stmt.query_map([], |r| {
            Ok(Session {
                id: r.get(0)?,
                title: r.get(1)?,
                model_name: r.get(2)?,
                updated_at: r.get(3)?,
            })
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    }

    pub fn list_messages(&self, session_id: &str) -> Vec<Message> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT role, content, created_at FROM messages WHERE session_id = ?1 ORDER BY created_at ASC")
            .unwrap();
        stmt.query_map(params![session_id], |r| {
            Ok(Message {
                role: r.get(0)?,
                content: r.get(1)?,
                created_at: r.get(2)?,
            })
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    }

    // ── Long-term memory (FTS5 + BM25, mirrors the original) ──

    pub fn store(&self, key: &str, content: &str, category: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO memories (key, content, category, created_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(key) DO UPDATE SET content = excluded.content, category = excluded.category, created_at = excluded.created_at",
            params![key, content, category, Self::now_ms()],
        )?;
        conn.execute(
            "INSERT INTO memories_fts (rowid, key, content, category) SELECT rowid, key, content, category FROM memories WHERE key = ?1
             ON CONFLICT(rowid) DO UPDATE SET content = excluded.content, category = excluded.category",
            params![key],
        ).ok();
        Ok(())
    }

    pub fn recall(&self, query: &str, limit: usize) -> Vec<MemoryHit> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT m.id, m.key, m.content, m.category, m.created_at, bm25(memories_fts) as score
             FROM memories_fts f JOIN memories m ON m.rowid = f.rowid
             WHERE memories_fts MATCH ?1 ORDER BY score LIMIT ?2",
        );
        let Ok(mut stmt) = stmt else { return Vec::new() };
        stmt.query_map(params![query, limit as i64], |r| {
            Ok(MemoryHit {
                id: r.get(0)?,
                key: r.get(1)?,
                content: r.get(2)?,
                category: r.get(3)?,
                created_at: r.get(4)?,
                score: r.get::<_, f64>(5).unwrap_or(0.0),
            })
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    }

    pub fn forget(&self, key: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM memories_fts WHERE rowid IN (SELECT rowid FROM memories WHERE key = ?1)", params![key]).ok();
        let n = conn.execute("DELETE FROM memories WHERE key = ?1", params![key])?;
        Ok(n > 0)
    }
}

#[derive(Debug, Clone)]
pub struct MemoryHit {
    pub id: i64,
    pub key: String,
    pub content: String,
    pub category: String,
    pub created_at: i64,
    pub score: f64,
}

//! SQLite session storage — aligned with hermes_state.py / hermes_state_common.py
//!
//! Schema follows the hermes SCHEMA_SQL (sessions + messages + system_prompts
//! + state_meta + async_delegations) with FTS5 full-text search over message
//! content and a LIKE fallback when FTS5 is unavailable.

use crate::error::{AgentError, Result};
use crate::provider::{Message, Role, ToolCall};
use crate::session::{Session, SessionMetadata, SessionStore};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Core schema — adapted from hermes_state_common.SCHEMA_SQL.
const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL);

CREATE TABLE IF NOT EXISTS system_prompts (
    hash TEXT PRIMARY KEY,
    prompt TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    source TEXT NOT NULL DEFAULT 'cli',
    user_id TEXT,
    session_key TEXT,
    model TEXT,
    system_prompt TEXT,
    system_prompt_hash TEXT,
    parent_session_id TEXT,
    started_at REAL NOT NULL,
    ended_at REAL,
    end_reason TEXT,
    message_count INTEGER DEFAULT 0,
    tool_call_count INTEGER DEFAULT 0,
    input_tokens INTEGER DEFAULT 0,
    output_tokens INTEGER DEFAULT 0,
    cwd TEXT,
    title TEXT,
    last_activity_at REAL,
    archived INTEGER NOT NULL DEFAULT 0,
    pinned INTEGER NOT NULL DEFAULT 0,
    model_config TEXT,
    compression_ineffective_count INTEGER NOT NULL DEFAULT 0,
    compression_recovery_deadline REAL,
    FOREIGN KEY (parent_session_id) REFERENCES sessions(id)
);

CREATE TABLE IF NOT EXISTS messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL REFERENCES sessions(id),
    role TEXT NOT NULL,
    content TEXT,
    tool_call_id TEXT,
    tool_calls TEXT,
    tool_name TEXT,
    timestamp REAL NOT NULL,
    token_count INTEGER,
    finish_reason TEXT,
    active INTEGER NOT NULL DEFAULT 1,
    display_metadata TEXT,
    reasoning TEXT
);

CREATE TABLE IF NOT EXISTS state_meta (
    key TEXT PRIMARY KEY,
    value TEXT
);

CREATE TABLE IF NOT EXISTS async_delegations (
    delegation_id TEXT PRIMARY KEY,
    origin_session TEXT NOT NULL,
    parent_session_id TEXT,
    state TEXT NOT NULL,
    dispatched_at REAL NOT NULL,
    completed_at REAL,
    updated_at REAL NOT NULL,
    result_json TEXT,
    task_json TEXT
);

CREATE TABLE IF NOT EXISTS delivery_obligations (
    obligation_id TEXT PRIMARY KEY,
    session_key TEXT NOT NULL,
    platform TEXT NOT NULL,
    chat_id TEXT NOT NULL,
    thread_id TEXT,
    content TEXT NOT NULL,
    state TEXT NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    created_at REAL NOT NULL,
    updated_at REAL NOT NULL,
    owner_pid INTEGER,
    owner_started_at INTEGER,
    last_error TEXT
);

CREATE INDEX IF NOT EXISTS idx_sessions_source ON sessions(source);
CREATE INDEX IF NOT EXISTS idx_sessions_parent ON sessions(parent_session_id);
CREATE INDEX IF NOT EXISTS idx_sessions_started ON sessions(started_at DESC);
CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id, timestamp);
CREATE INDEX IF NOT EXISTS idx_messages_session_id ON messages(session_id, id);
"#;

/// FTS5 virtual table (created when supported by the SQLite build).
const FTS_SQL: &str = r#"
CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
    content,
    content='messages',
    content_rowid='id',
    tokenize='unicode61'
);
"#;

/// One session row exposed by the gateway API.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionRow {
    pub id: String,
    pub source: String,
    pub model: Option<String>,
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub parent_session_id: Option<String>,
    pub started_at: f64,
    /// Last activity (falls back to started_at when never touched). The
    /// desktop sidebar sorts + groups on this field (P421 fix).
    pub last_activity_at: f64,
    pub ended_at: Option<f64>,
    pub end_reason: Option<String>,
    pub message_count: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    /// P519: the sessions-table `archived` column (set by the TUI F8
    /// archive flow; distinct from the `end_reason = "archived"` the
    /// desktop PATCH flow writes).
    pub archived: bool,
    /// hermes desktop parity: durable server-side pin flag — the sidebar
    /// Pinned section and the auto-archive sweep both honour it.
    pub pinned: bool,
    /// hermes desktop parity: tool-call tally surfaced on session rows.
    pub tool_call_count: i64,
}

/// Per-model usage aggregate (hermes `_get_models_analytics` row).
#[derive(Debug, Clone)]
pub struct ModelUsageRow {
    pub model: String,
    pub sessions: i64,
    pub messages: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub last_used_at: f64,
}

/// One gateway platform-conversation row for the MCP channel bridge
/// (hermes mcp_serve `_row_to_index_entry` input shape).
#[derive(Debug, Clone)]
pub struct PlatformSessionRow {
    pub id: String,
    pub source: String,
    pub user_id: Option<String>,
    pub session_key: String,
    pub title: Option<String>,
    pub started_at: f64,
    pub last_activity_at: Option<f64>,
    pub input_tokens: i64,
    pub output_tokens: i64,
}

/// One stored message with its row id + timestamp (hermes
/// `SessionDB.get_messages` dict shape for the MCP bridge).
#[derive(Debug, Clone)]
pub struct MessageRow {
    pub id: i64,
    pub role: String,
    pub content: String,
    pub timestamp: f64,
}

/// A SQLite-backed session store.
pub struct SqliteSessionStore {
    conn: Mutex<Connection>,
    path: PathBuf,
    has_fts: bool,
    /// Process-unique id in the live-store registry (slice3a). Registered on
    /// `open()`, deregistered in `Drop`, so the in-process arm of the VACUUM
    /// holders gate can answer "does THIS process hold another generation of
    /// this path?". 0 means "not registered" (only in a Default/partial state).
    instance_id: u64,
}

/// One skill-scaffolded session row (hermes
/// `list_skill_scaffolded_sessions` output shape).
#[derive(Debug, Clone, PartialEq)]
pub struct SkillScaffoldedRow {
    pub id: String,
    pub title: Option<String>,
    pub content: String,
}

/// One row of the interactive session browser (the hermes
/// `list_sessions_rich` fields the browse picker renders).
#[derive(Debug, Clone, PartialEq)]
pub struct BrowseRow {
    pub id: String,
    pub title: Option<String>,
    pub preview: Option<String>,
    pub source: String,
    pub last_active: f64,
    /// P562: session start — the details pane derives the duration from
    /// `last_active - started_at`.
    pub started_at: f64,
    /// Working directory of the session — the browse surfaces resolve it
    /// against `projects.db` to show the owning project (P165).
    pub cwd: Option<String>,
    /// P513: archived flag — rendered as a marker when the browser
    /// includes archived sessions (F4 toggle).
    pub archived: bool,
    /// P524: stored message count — surfaced in the details pane so a
    /// session's size is visible without opening it.
    pub message_count: i64,
    /// P528: the session's model — shown in the details pane and used
    /// by the TUI model quick-filter (F10 cycles distinct models).
    pub model: Option<String>,
    /// P542: recorded end reason (complete/branched/archived/…) for
    /// the TUI details pane; `None` while the session is open.
    pub end_reason: Option<String>,
    /// P546: stored token totals (input + output) for the TUI details
    /// pane — mirrors the desktop sidebar/usage badges.
    pub total_tokens: i64,
}

/// Maximum session title length in characters (hermes `MAX_TITLE_LENGTH`).
pub const MAX_TITLE_LENGTH: usize = 100;

/// VACUUM only rewrites the DB when the reclaimable fraction exceeds this
/// (hermes `AUTO_VACUUM_MIN_FREELIST_RATIO`, `hermes_state_common.py:290`). A
/// module constant, **not** a config key: upstream never exposes it in the
/// `[sessions]` block — it is the freelist-ratio leg of the VACUUM triple gate
/// (pruned>0 + `min_vacuum_interval_days` + this), so a small prune on a dense
/// multi-GB database never triggers a full rewrite (#54189).
pub const AUTO_VACUUM_MIN_FREELIST_RATIO: f64 = 0.25;

/// Sources that join automatic stale-open reconciliation (hermes
/// `_AUTO_PRUNE_STALE_OPEN_SOURCES`, `hermes_state.py:472`, #60609). Only
/// state-owned producers: messaging/UI sources have their own lifecycle owners,
/// and ending one of those triggers a routing loop. Unknown sources fail
/// closed (they are simply not swept).
///
/// `recovered` covers placeholders synthesized for orphaned messages (no live
/// owner, never stamped `ended_at`); without it they would be immortal.
pub const AUTO_PRUNE_STALE_OPEN_SOURCES: &[&str] = &[
    "cli",
    "cron",
    "kanban",
    "acp",
    "api_server",
    "subagent",
    "tool",
    "recovered",
];

/// Outcome of one `maybe_auto_prune_and_vacuum` pass (hermes returns the same
/// keys: `skipped`, `pruned`, `closed`, `vacuumed`, plus `freelist_ratio` when
/// a VACUUM was considered and `error` on failure). Never raised — callers on
/// the startup path log and continue.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AutoMaintenanceReport {
    /// A throttle/lock/guard decided not to run this pass.
    pub skipped: bool,
    /// ENDED sessions deleted.
    pub pruned: usize,
    /// Stale open rows closed (`startup_orphan_reap`), not deleted.
    pub closed: usize,
    /// Whether a VACUUM actually ran.
    pub vacuumed: bool,
    /// Freelist fraction observed when a VACUUM was considered.
    pub freelist_ratio: Option<f64>,
    /// Why the VACUUM leg did not run even though its gates passed.
    pub vacuum_skipped_reason: Option<String>,
    /// First failure message, if any (the pass still returns normally).
    pub error: Option<String>,
}

/// Age of a `state_meta` timestamp in seconds; `None` when unset **or corrupt**
/// (= no prior run → proceed). Mirrors hermes `_seconds_since`, which returns
/// None on any parse failure rather than skipping maintenance forever because
/// of one garbage row.
fn seconds_since(now: f64, raw: Option<String>) -> Option<f64> {
    let raw = raw?;
    if raw.is_empty() {
        return None;
    }
    raw.trim().parse::<f64>().ok().map(|ts| now - ts)
}

/// File name component as `&str`, falling back to an empty name.
fn file_name_str(path: &Path) -> &str {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
}

/// Non-blocking cross-process advisory lock on `<db>.auto-maintenance.lock`
/// (hermes `_try_acquire_auto_maintenance_lock`). Rationale upstream: without
/// it two concurrent startups both pass the interval check and the second
/// prunes a row the first has only just closed recoverably.
///
/// `Ok(Some(guard))` = held (guard releases on drop); `Ok(None)` = another
/// process holds it (skip the pass); `Err` = the lock file could not even be
/// opened (skip the pass, warn).
struct MaintenanceLock {
    file: std::fs::File,
}

impl MaintenanceLock {
    fn try_acquire(path: &Path) -> std::io::Result<Option<Self>> {
        use std::fs::OpenOptions;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(path)?;
        match crate::process_ctl::try_lock_exclusive(&file) {
            Ok(()) => Ok(Some(Self { file })),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e),
        }
    }
}

impl Drop for MaintenanceLock {
    fn drop(&mut self) {
        let _ = crate::process_ctl::unlock(&self.file);
    }
}

/// Clean a user-supplied session title (hermes `sanitize_title`): strip
/// ASCII/Unicode control characters, collapse whitespace runs, trim, and
/// normalize empty input to `None`. Errors when the cleaned title exceeds
/// [`MAX_TITLE_LENGTH`] characters.
pub fn sanitize_title(title: &str) -> std::result::Result<Option<String>, String> {
    let cleaned: String = title
        .chars()
        .filter(|c| {
            // Keep \t \n \r (whitespace collapsing handles them); drop the
            // remaining ASCII control chars and DEL.
            if c.is_ascii_control() && !matches!(c, '\t' | '\n' | '\r') {
                return false;
            }
            !matches!(c,
                '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2060}'..='\u{2069}'
                | '\u{FEFF}'
                | '\u{FFFC}'
                | '\u{FFF9}'..='\u{FFFB}'
            )
        })
        .collect();
    let mut collapsed = String::with_capacity(cleaned.len());
    let mut in_ws = false;
    for ch in cleaned.chars() {
        if ch.is_whitespace() {
            if !in_ws {
                collapsed.push(' ');
                in_ws = true;
            }
        } else {
            collapsed.push(ch);
            in_ws = false;
        }
    }
    let collapsed = collapsed.trim().to_string();
    if collapsed.is_empty() {
        return Ok(None);
    }
    if collapsed.chars().count() > MAX_TITLE_LENGTH {
        return Err(format!(
            "Title too long ({} chars, max {})",
            collapsed.chars().count(),
            MAX_TITLE_LENGTH
        ));
    }
    Ok(Some(collapsed))
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Initialize the current schema (and FTS5 when supported) on `conn`.
/// Returns whether the FTS virtual table is available. Shared by the store
/// and the offline session-recovery path.
pub fn initialize_schema(conn: &Connection) -> Result<bool> {
    conn.execute_batch(SCHEMA_SQL)
        .map_err(|e| AgentError::session(format!("schema: {}", e)))?;
    // FTS5 probe — same pattern as hermes_state_schema's _fts5_available.
    let has_fts = conn.execute_batch(FTS_SQL).is_ok();
    if has_fts {
        // The external-content index can lag the canonical messages table
        // (e.g. after `sessions repair` drops the FTS schema); rebuild it
        // from the content table when the row counts disagree.
        let fts_rows: Option<i64> = conn
            .query_row("SELECT COUNT(*) FROM messages_fts", [], |r| r.get(0))
            .ok();
        let msg_rows: Option<i64> = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .ok();
        if let (Some(fts), Some(msgs)) = (fts_rows, msg_rows) {
            if fts != msgs {
                conn.execute(
                    "INSERT INTO messages_fts(messages_fts) VALUES('rebuild')",
                    [],
                )
                .ok();
            }
        }
    }
    let version: Option<i64> = conn
        .query_row("SELECT version FROM schema_version LIMIT 1", [], |r| {
            r.get(0)
        })
        .optional()
        .map_err(|e| AgentError::session(e.to_string()))?;
    if version.is_none() {
        conn.execute(
            "INSERT INTO schema_version (version) VALUES (?1)",
            params![1],
        )
        .map_err(|e| AgentError::session(e.to_string()))?;
    }
    Ok(has_fts)
}

/// Add `messages.display_metadata` (JSON) on pre-existing databases —
/// reactions live inside it (hermes `display_metadata` semantics).
/// Add the durable delivery-claim columns to stores created before the
/// hermes delivery-attempts hardening (idempotent).
fn ensure_delegation_delivery_columns(conn: &Connection) -> Result<()> {
    let mut stmt = conn
        .prepare("PRAGMA table_info(async_delegations)")
        .map_err(|e| AgentError::session(e.to_string()))?;
    let columns: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| AgentError::session(e.to_string()))?
        .filter_map(|r| r.ok())
        .collect();
    let mut alters = Vec::new();
    if !columns.iter().any(|c| c == "delivery_attempts") {
        alters.push("ALTER TABLE async_delegations ADD COLUMN delivery_attempts INTEGER NOT NULL DEFAULT 0;");
    }
    if !columns.iter().any(|c| c == "delivery_claim") {
        alters.push("ALTER TABLE async_delegations ADD COLUMN delivery_claim TEXT;");
    }
    if !columns.iter().any(|c| c == "delivery_claimed_at") {
        alters.push("ALTER TABLE async_delegations ADD COLUMN delivery_claimed_at REAL;");
    }
    if !alters.is_empty() {
        conn.execute_batch(&alters.join(
            "
",
        ))
        .map_err(|e| AgentError::session(format!("add delegation delivery columns: {}", e)))?;
    }
    Ok(())
}

fn ensure_display_metadata_column(conn: &Connection) -> Result<()> {
    let mut stmt = conn
        .prepare("PRAGMA table_info(messages)")
        .map_err(|e| AgentError::session(e.to_string()))?;
    let has_column = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| AgentError::session(e.to_string()))?
        .filter_map(|r| r.ok())
        .any(|name| name == "display_metadata");
    if !has_column {
        conn.execute_batch("ALTER TABLE messages ADD COLUMN display_metadata TEXT;")
            .map_err(|e| AgentError::session(format!("add display_metadata: {}", e)))?;
    }
    Ok(())
}

/// Add `messages.reasoning` on pre-existing databases (idempotent).
///
/// Stores an assistant turn's reasoning/thinking text so it survives reload and
/// export instead of being discarded after streaming (hermes v0.22.2 replays
/// prior reasoning across resume/branch/compaction). 🔴 Storage only — the value
/// is deliberately NOT sent back on the provider wire: must-echo providers
/// (DeepSeek/Kimi/MiMo) 400 without a reasoning pad while strict ones
/// (Mistral/Groq/Cerebras) 400/422 with it, so replay requires hermes'
/// per-provider echo policy. Wiring it unconditionally would break providers
/// that work today; see `message_to_api` in `provider/openai.rs`.
fn ensure_reasoning_column(conn: &Connection) -> Result<()> {
    let mut stmt = conn
        .prepare("PRAGMA table_info(messages)")
        .map_err(|e| AgentError::session(e.to_string()))?;
    let has_column = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| AgentError::session(e.to_string()))?
        .filter_map(|r| r.ok())
        .any(|name| name == "reasoning");
    if !has_column {
        conn.execute_batch("ALTER TABLE messages ADD COLUMN reasoning TEXT;")
            .map_err(|e| AgentError::session(format!("add reasoning: {}", e)))?;
    }
    Ok(())
}

/// `model_config` key holding the durable rejected-reasoning-route table.
/// Same literal as hermes `_REJECTION_MODEL_CONFIG_KEY`.
pub const REJECTED_REASONING_KEY: &str = "reasoning_rejected_carriers";

/// Carrier names a persisted rejection may name (hermes `_ALL_CARRIERS`).
///
/// Read-side whitelist: upstream filters stored carriers through it
/// (`k for k in entry[3] if k in _ALL_CARRIERS`), so a corrupt or hand-edited
/// blob cannot inject an arbitrary string into the replay decision. ulnclaw's
/// wire only sends `reasoning_content`, but the upstream two are listed so the
/// table stays readable if the wire is ever widened.
pub const KNOWN_REASONING_CARRIERS: &[&str] =
    &["reasoning_content", "reasoning", "reasoning_details"];

/// One route whose schema rejected `reasoning_content`
/// (hermes' `(provider, host, model) -> carriers` map entry, flattened).
///
/// 🔴 The route key is `(provider, host, model)` — NOT just provider. Upstream is
/// explicit about why: "one gateway serves models on different upstream lanes, so
/// a rejection learned for one model must not strip carriers from its siblings".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectedReasoningRoute {
    pub provider: String,
    /// Base-URL host, as produced by `provider::reasoning_echo::host_of`.
    pub host: String,
    pub model: String,
    /// Rejected carriers. ulnclaw's wire has one (`reasoning_content`); the vec
    /// keeps upstream's shape so the extra carriers can be added without a
    /// storage migration.
    pub carriers: Vec<String>,
}

/// Tolerant `model_config` decode (hermes `_parse_model_config`,
/// `hermes_state_sessions.py`): JSON text → object; anything else → empty.
///
/// Must never fail: a corrupt or legacy blob in one row has to degrade to "no
/// durable state", not to a session-load error. Note the non-object cases —
/// a JSON array, a bare number, or truncated text all yield `{}`.
fn parse_model_config(raw: &str) -> Map<String, Value> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Map::new();
    }
    match serde_json::from_str::<Value>(trimmed) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

/// Idempotent migration for the `sessions.model_config` JSON column
/// (hermes `hermes_state_common.py:395 model_config TEXT`).
///
/// This is the per-session key/value store hermes hangs durable, cross-`--resume`
/// state on — `get_session_model_config_value` / `patch_session_model_config`
/// (`hermes_state_sessions.py:823,830`). ulnclaw needs the same substrate for two
/// ports that are otherwise process-lifetime-only:
/// * **P3-1e** — the reasoning-rejection learner, so a resumed process never
///   re-learns a 400 (hermes `record_reasoning_field_rejection` persists here);
/// * **P3-5d** — the compressor's usage anchor / anti-thrash strikes across turns.
///
/// Existing DBs (created before this column) get it added without touching rows,
/// same pattern as `ensure_reasoning_column` / `ensure_display_metadata_column`.
fn ensure_model_config_column(conn: &Connection) -> Result<()> {
    let mut stmt = conn
        .prepare("PRAGMA table_info(sessions)")
        .map_err(|e| AgentError::session(e.to_string()))?;
    let has_column = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| AgentError::session(e.to_string()))?
        .filter_map(|r| r.ok())
        .any(|name| name == "model_config");
    if !has_column {
        conn.execute_batch("ALTER TABLE sessions ADD COLUMN model_config TEXT;")
            .map_err(|e| AgentError::session(format!("add model_config: {}", e)))?;
    }
    Ok(())
}

/// Idempotent migration for the two durable anti-thrash columns (P3-5c,
/// hermes `hermes_state_common.py:433-434`).
///
/// These are **table columns**, not `model_config` keys — upstream stores them
/// as columns (`_write_session_column("compression_ineffective_count", …)`) and
/// they are read/written on every blocked evaluation, so a JSON blob would be
/// the wrong shape.
///
/// Both exist because the compressor is rebuilt (P3-5d's LRU eviction past 64
/// sessions, or a process restart) while the guard must NOT be disarmed by that
/// rebuild — #54923 "restart must not disarm", #69872 durable tripped counter,
/// #100185 durable recovery deadline.
fn ensure_compression_columns(conn: &Connection) -> Result<()> {
    let mut stmt = conn
        .prepare("PRAGMA table_info(sessions)")
        .map_err(|e| AgentError::session(e.to_string()))?;
    let existing: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| AgentError::session(e.to_string()))?
        .filter_map(|r| r.ok())
        .collect();
    drop(stmt);
    if !existing
        .iter()
        .any(|n| n == "compression_ineffective_count")
    {
        conn.execute_batch(
            "ALTER TABLE sessions ADD COLUMN compression_ineffective_count INTEGER NOT NULL DEFAULT 0;",
        )
        .map_err(|e| AgentError::session(format!("add compression_ineffective_count: {}", e)))?;
    }
    if !existing
        .iter()
        .any(|n| n == "compression_recovery_deadline")
    {
        conn.execute_batch("ALTER TABLE sessions ADD COLUMN compression_recovery_deadline REAL;")
            .map_err(|e| {
                AgentError::session(format!("add compression_recovery_deadline: {}", e))
            })?;
    }
    Ok(())
}

impl Drop for SqliteSessionStore {
    fn drop(&mut self) {
        // Deregister this generation so the in-process holders arm stops seeing
        // it. Idempotent + panic-free (slice3a): a double-drop or an unregistered
        // id is a no-op. Runs when the last Arc<SqliteSessionStore> drops, which
        // is exactly "no longer held".
        if self.instance_id != 0 {
            crate::state_holders::unregister_store(&self.path, self.instance_id);
        }
    }
}
impl SqliteSessionStore {
    /// Open (or create) the state database at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = Connection::open(&path)
            .map_err(|e| AgentError::session(format!("open {}: {}", path.display(), e)))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .map_err(|e| AgentError::session(format!("pragmas: {}", e)))?;
        let has_fts = initialize_schema(&conn)?;
        ensure_display_metadata_column(&conn)?;
        ensure_reasoning_column(&conn)?;
        ensure_model_config_column(&conn)?;
        ensure_compression_columns(&conn)?;
        ensure_delegation_delivery_columns(&conn)?;

        // Register BEFORE moving `path` into the struct; the id rides the store
        // and Drop deregisters it. If init_meta() fails, `store` is dropped and
        // the registration is undone — no leak on the error path.
        let instance_id = crate::state_holders::register_store(&path);
        let store = Self {
            conn: Mutex::new(conn),
            path,
            has_fts,
            instance_id,
        };
        store.init_meta()?;
        Ok(store)
    }

    /// Open the default state DB at `<home>/state.db`.
    pub fn open_default() -> Result<Self> {
        let path = crate::config::ulnclaw_home().join("state.db");
        Self::open(path)
    }

    fn init_meta(&self) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        let version: Option<i64> = conn
            .query_row("SELECT version FROM schema_version LIMIT 1", [], |r| {
                r.get(0)
            })
            .optional()
            .map_err(|e| AgentError::session(e.to_string()))?;
        if version.is_none() {
            conn.execute(
                "INSERT INTO schema_version (version) VALUES (?1)",
                params![1],
            )
            .map_err(|e| AgentError::session(e.to_string()))?;
        }
        Ok(())
    }

    /// Number of sessions in the store (metrics/observability).
    pub fn count_sessions(&self) -> Result<usize> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        conn.query_row("SELECT COUNT(*) FROM sessions", [], |row| {
            row.get::<_, i64>(0)
        })
        .map(|count| count as usize)
        .map_err(|e| AgentError::session(e.to_string()))
    }

    /// Number of stored messages (metrics/observability).
    pub fn count_messages(&self) -> Result<usize> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        conn.query_row("SELECT COUNT(*) FROM messages", [], |row| {
            row.get::<_, i64>(0)
        })
        .map(|count| count as usize)
        .map_err(|e| AgentError::session(e.to_string()))
    }

    /// Database file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether FTS5 full-text search is available.
    pub fn has_fts(&self) -> bool {
        self.has_fts
    }

    /// Create a new session row, returning its id.
    pub fn create_session(
        &self,
        source: &str,
        model: Option<&str>,
        cwd: Option<&str>,
    ) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        conn.execute(
            "INSERT INTO sessions (id, source, model, cwd, started_at, last_activity_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
            params![id, source, model, cwd, now_secs()],
        )
        .map_err(|e| AgentError::session(format!("create session: {}", e)))?;
        Ok(id)
    }

    /// Create a child session (delegation lineage).
    pub fn create_child_session(
        &self,
        parent_id: &str,
        source: &str,
        model: Option<&str>,
    ) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        conn.execute(
            "INSERT INTO sessions (id, source, model, parent_session_id, started_at, last_activity_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
            params![id, source, model, parent_id, now_secs()],
        )
        .map_err(|e| AgentError::session(format!("create child session: {}", e)))?;
        Ok(id)
    }

    /// Create a session with a caller-chosen id (fork API). `parent` links
    /// the new session into an existing lineage.
    pub fn create_named_session(
        &self,
        id: &str,
        source: &str,
        model: Option<&str>,
        parent: Option<&str>,
    ) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        conn.execute(
            "INSERT INTO sessions (id, source, model, parent_session_id, started_at, last_activity_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
            params![id, source, model, parent, now_secs()],
        )
        .map_err(|e| AgentError::session(format!("create named session: {}", e)))?;
        Ok(())
    }

    /// Insert a session row with an explicit id and metadata (session
    /// import; hermes `import_sessions`). Existing ids are left untouched
    /// — returns `false` when the id already exists. Live-runtime fields
    /// stay reset: `last_activity_at` mirrors `started_at` so imported
    /// history sorts at its original time without fabricating activity.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_imported_session(
        &self,
        id: &str,
        source: &str,
        model: Option<&str>,
        title: Option<&str>,
        cwd: Option<&str>,
        started_at: f64,
        ended_at: Option<f64>,
        end_reason: Option<&str>,
        archived: bool,
    ) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        let exists: i64 = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1)",
                params![id],
                |row| row.get(0),
            )
            .unwrap_or(0);
        if exists != 0 {
            return Ok(false);
        }
        conn.execute(
            "INSERT INTO sessions (id, source, model, title, cwd, started_at, ended_at, end_reason, archived, last_activity_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?6)",
            params![id, source, model, title, cwd, started_at, ended_at, end_reason, archived as i64],
        )
        .map_err(|e| AgentError::session(format!("import session: {}", e)))?;
        Ok(true)
    }

    /// Append one message to a session.
    pub fn append_message(&self, session_id: &str, message: &Message) -> Result<()> {
        self.append_message_at(session_id, message, now_secs())
    }

    /// Append a message with an explicit timestamp (session import keeps
    /// the original timeline; hermes `import_sessions`).
    pub fn append_message_at(
        &self,
        session_id: &str,
        message: &Message,
        timestamp: f64,
    ) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        let tool_calls = message
            .tool_calls
            .as_ref()
            .map(|calls| serde_json::to_string(calls))
            .transpose()
            .map_err(|e| AgentError::session(e.to_string()))?;
        let role = message.role.to_string();
        conn.execute(
            "INSERT INTO messages (session_id, role, content, tool_call_id, tool_calls, tool_name, timestamp, reasoning)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                session_id,
                role,
                message.content,
                message.tool_call_id,
                tool_calls,
                message.name,
                timestamp,
                message.reasoning,
            ],
        )
        .map_err(|e| AgentError::session(format!("append message: {}", e)))?;
        let msg_id: i64 = conn.last_insert_rowid();
        conn.execute(
            "UPDATE sessions SET message_count = message_count + 1, last_activity_at = ?2
             WHERE id = ?1",
            params![session_id, timestamp],
        )
        .ok();
        if self.has_fts {
            // External-content FTS: index the content manually.
            conn.execute(
                "INSERT INTO messages_fts (rowid, content) VALUES (?1, ?2)",
                params![msg_id, message.content],
            )
            .ok();
        }
        // P493: live transcript updates — announce every append on the
        // desktop bus (inert unless a desktop consumer is subscribed).
        crate::desktop_bridge::publish(
            session_id,
            "session.message",
            &serde_json::json!({
                "session_id": session_id,
                "role": role,
                "message_id": msg_id,
                "timestamp": timestamp,
            }),
        );
        Ok(())
    }

    /// Replace a session's whole transcript (P457: `/compress` manual
    /// compression). Deletes the session's rows and re-appends the given
    /// messages; timestamps re-anchor at now, order is preserved.
    pub fn replace_messages(&self, session_id: &str, messages: &[Message]) -> Result<()> {
        {
            let conn = self
                .conn
                .lock()
                .map_err(|e| AgentError::session(e.to_string()))?;
            if self.has_fts {
                conn.execute(
                    "DELETE FROM messages_fts WHERE rowid IN
                     (SELECT rowid FROM messages WHERE session_id = ?1)",
                    params![session_id],
                )
                .map_err(|e| AgentError::session(format!("replace messages fts: {}", e)))?;
            }
            conn.execute(
                "DELETE FROM messages WHERE session_id = ?1",
                params![session_id],
            )
            .map_err(|e| AgentError::session(format!("replace messages delete: {}", e)))?;
            conn.execute(
                "UPDATE sessions SET message_count = 0 WHERE id = ?1",
                params![session_id],
            )
            .ok();
        }
        for message in messages {
            self.append_message(session_id, message)?;
        }
        Ok(())
    }

    /// Row id of the most recent active message with `role` (hermes
    /// `latest_message_row_id`). `offset` steps to earlier turns (1 = the
    /// one before the latest). `require_text` skips empty-content rows so a
    /// reaction never lands on an invisible (tool-call-only) bubble.
    pub fn latest_message_row_id(
        &self,
        session_id: &str,
        role: &str,
        offset: i64,
        require_text: bool,
    ) -> Option<i64> {
        if session_id.is_empty() || !matches!(role, "user" | "assistant") || offset < 0 {
            return None;
        }
        let conn = self.conn.lock().ok()?;
        let text_filter = if require_text {
            "AND content IS NOT NULL AND TRIM(content) != '' "
        } else {
            ""
        };
        let sql = format!(
            "SELECT id FROM messages WHERE session_id = ?1 AND role = ?2
             AND active = 1 {text_filter}ORDER BY id DESC LIMIT 1 OFFSET ?3"
        );
        conn.query_row(&sql, params![session_id, role, offset], |row| row.get(0))
            .ok()
    }

    /// Role of the active message at `row_id` in `session_id` (hermes
    /// `get_message_role`).
    pub fn message_role(&self, session_id: &str, row_id: i64) -> Option<String> {
        if session_id.is_empty() {
            return None;
        }
        let conn = self.conn.lock().ok()?;
        conn.query_row(
            "SELECT role FROM messages WHERE id = ?1 AND session_id = ?2 AND active = 1",
            params![row_id, session_id],
            |row| row.get(0),
        )
        .ok()
    }

    /// Set (or with `emoji = None` clear) `author`'s reaction on one
    /// message — iOS Tapback semantics (hermes `set_message_reaction`):
    /// one reaction per author per message; re-sending the same emoji
    /// retracts it, a different emoji replaces it. Returns the full
    /// reaction list after the write, or `None` when the row does not
    /// belong to the session.
    pub fn set_message_reaction(
        &self,
        session_id: &str,
        row_id: i64,
        emoji: Option<&str>,
        author: &str,
    ) -> Option<Vec<serde_json::Value>> {
        if session_id.is_empty() {
            return None;
        }
        let conn = self.conn.lock().ok()?;
        let raw: Option<String> = conn
            .query_row(
                "SELECT display_metadata FROM messages WHERE id = ?1 AND session_id = ?2",
                params![row_id, session_id],
                |row| row.get(0),
            )
            .ok()?;

        let mut meta: serde_json::Value = raw
            .as_deref()
            .filter(|s| !s.is_empty())
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_else(|| serde_json::json!({}));

        let existing: Vec<serde_json::Value> = meta
            .get("reactions")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let previous = existing
            .iter()
            .find(|r| r.get("author").and_then(|a| a.as_str()) == Some(author));
        let toggling_off = emoji.map_or(false, |e| {
            previous.map_or(false, |p| {
                p.get("emoji").and_then(|x| x.as_str()) == Some(e)
            })
        });

        let mut reactions: Vec<serde_json::Value> = existing
            .into_iter()
            .filter(|r| r.get("author").and_then(|a| a.as_str()) != Some(author))
            .collect();
        if let Some(emoji) = emoji.filter(|e| !e.is_empty()) {
            if !toggling_off {
                reactions.push(serde_json::json!({
                    "emoji": emoji,
                    "author": author,
                    "at": now_secs(),
                }));
            }
        }

        if reactions.is_empty() {
            if let Some(obj) = meta.as_object_mut() {
                obj.remove("reactions");
            }
        } else {
            meta["reactions"] = serde_json::Value::Array(reactions.clone());
        }

        let encoded = if meta.as_object().map_or(true, |o| o.is_empty()) {
            None
        } else {
            Some(meta.to_string())
        };
        conn.execute(
            "UPDATE messages SET display_metadata = ?1 WHERE id = ?2",
            params![encoded, row_id],
        )
        .ok()?;
        Some(reactions)
    }

    // ── Durable background-delegation registry (hermes async_delegation
    //    durable store: dispatch/completion persisted so results survive
    //    process restarts) ─────────────────────────────────────────────

    /// Persist a background-delegation dispatch.
    pub fn persist_delegation_dispatch(
        &self,
        delegation_id: &str,
        origin_session: &str,
        tasks_json: &str,
    ) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        conn.execute(
            "INSERT OR REPLACE INTO async_delegations
             (delegation_id, origin_session, parent_session_id, state, dispatched_at, updated_at, task_json)
             VALUES (?1, ?2, ?2, 'running', ?3, ?3, ?4)",
            params![delegation_id, origin_session, now_secs(), tasks_json],
        )
        .map_err(|e| AgentError::session(format!("persist delegation: {}", e)))?;
        Ok(())
    }

    /// Record the consolidated result of a finished delegation
    /// (`state` = completed | failed).
    pub fn finish_delegation(
        &self,
        delegation_id: &str,
        state: &str,
        result_json: &str,
    ) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        conn.execute(
            "UPDATE async_delegations SET state = ?2, completed_at = ?3, updated_at = ?3, result_json = ?4
             WHERE delegation_id = ?1",
            params![delegation_id, state, now_secs(), result_json],
        )
        .map_err(|e| AgentError::session(format!("finish delegation: {}", e)))?;
        Ok(())
    }

    /// Mark a completed delegation as delivered to its session.
    pub fn mark_delegation_delivered(&self, delegation_id: &str) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        conn.execute(
            "UPDATE async_delegations SET state = 'delivered', updated_at = ?2
             WHERE delegation_id = ?1 AND state IN ('completed', 'unknown')",
            params![delegation_id, now_secs()],
        )
        .map_err(|e| AgentError::session(format!("deliver delegation: {}", e)))?;
        Ok(())
    }

    /// Hermes `_MAX_DELIVERY_ATTEMPTS`: after this many failed delivery
    /// attempts a completion converges to terminal `dropped` instead of
    /// replaying on every restart.
    pub const MAX_DELIVERY_ATTEMPTS: i64 = 8;
    /// Hermes claim TTL: a claim older than this is considered abandoned
    /// and may be taken over by another consumer.
    const CLAIM_STALE_SECS: f64 = 300.0;

    /// Claim one pending completion across competing consumers (hermes
    /// `claim_completion_delivery`). Increments `delivery_attempts`; stale
    /// claims (older than 300s) are re-claimable. Returns true when this
    /// caller now owns the delivery.
    pub fn claim_delegation_delivery(&self, delegation_id: &str, claim_id: &str) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        let now = now_secs();
        let rows = conn
            .execute(
                "UPDATE async_delegations
                 SET delivery_claim = ?2, delivery_claimed_at = ?3,
                     delivery_attempts = delivery_attempts + 1, updated_at = ?3
                 WHERE delegation_id = ?1 AND state IN ('completed', 'unknown')
                   AND (delivery_claim IS NULL OR delivery_claimed_at < ?4)",
                params![delegation_id, claim_id, now, now - Self::CLAIM_STALE_SECS],
            )
            .map_err(|e| AgentError::session(format!("claim delegation: {}", e)))?;
        Ok(rows == 1)
    }

    /// Acknowledge delivery for the consumer holding the claim (hermes
    /// `complete_completion_delivery`).
    pub fn complete_delegation_delivery(
        &self,
        delegation_id: &str,
        claim_id: &str,
    ) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        let now = now_secs();
        let rows = conn
            .execute(
                "UPDATE async_delegations
                 SET state = 'delivered', updated_at = ?3,
                     delivery_claim = NULL, delivery_claimed_at = NULL
                 WHERE delegation_id = ?1 AND state IN ('completed', 'unknown')
                   AND delivery_claim = ?2",
                params![delegation_id, claim_id, now],
            )
            .map_err(|e| AgentError::session(format!("complete delegation delivery: {}", e)))?;
        Ok(rows == 1)
    }

    /// Release a failed delivery claim (hermes `release_completion_delivery`).
    /// Attempts are counted at claim time, so once the budget is exhausted
    /// the row converges to terminal `dropped` — otherwise an undeliverable
    /// completion replays on every restart forever. Returns true when the
    /// row was terminally dropped.
    pub fn release_delegation_delivery(&self, delegation_id: &str, claim_id: &str) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        let now = now_secs();
        let capped = conn
            .execute(
                "UPDATE async_delegations
                 SET state = 'dropped', updated_at = ?3,
                     delivery_claim = NULL, delivery_claimed_at = NULL
                 WHERE delegation_id = ?1 AND state IN ('completed', 'unknown')
                   AND delivery_claim = ?2 AND delivery_attempts >= ?4",
                params![delegation_id, claim_id, now, Self::MAX_DELIVERY_ATTEMPTS],
            )
            .map_err(|e| AgentError::session(format!("drop delegation delivery: {}", e)))?;
        if capped == 1 {
            tracing::warn!(
                "delegation {delegation_id} exhausted {} delivery attempts; marking terminally dropped (result remains queryable)",
                Self::MAX_DELIVERY_ATTEMPTS
            );
            return Ok(true);
        }
        conn.execute(
            "UPDATE async_delegations
             SET delivery_claim = NULL, delivery_claimed_at = NULL, updated_at = ?3
             WHERE delegation_id = ?1 AND state IN ('completed', 'unknown')
               AND delivery_claim = ?2",
            params![delegation_id, claim_id, now],
        )
        .map_err(|e| AgentError::session(format!("release delegation delivery: {}", e)))?;
        Ok(false)
    }

    /// Terminally drop a claimed completion whose delivery target is
    /// permanently gone (hermes `drop_completion_delivery`). `dropped`
    /// (not `delivered`) keeps the ack honest and restart recovery from
    /// replaying it.
    pub fn drop_delegation_delivery(&self, delegation_id: &str, claim_id: &str) -> Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        let now = now_secs();
        let rows = conn
            .execute(
                "UPDATE async_delegations
                 SET state = 'dropped', updated_at = ?3,
                     delivery_claim = NULL, delivery_claimed_at = NULL
                 WHERE delegation_id = ?1 AND state IN ('completed', 'unknown')
                   AND delivery_claim = ?2",
                params![delegation_id, claim_id, now],
            )
            .map_err(|e| AgentError::session(format!("drop delegation delivery: {}", e)))?;
        Ok(rows == 1)
    }

    /// Delivery attempts recorded for a delegation (ops/test visibility).
    pub fn delegation_delivery_attempts(&self, delegation_id: &str) -> i64 {
        let Ok(conn) = self.conn.lock() else {
            return 0;
        };
        conn.query_row(
            "SELECT delivery_attempts FROM async_delegations WHERE delegation_id = ?1",
            params![delegation_id],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0)
    }

    // ------------------------------------------------------------------
    // Delivery obligations (P700; hermes gateway/delivery_ledger.py)
    // ------------------------------------------------------------------

    /// Record a pending outbound final response (hermes
    /// `record_obligation`); returns the obligation id.
    pub fn record_obligation(
        &self,
        obligation_id: &str,
        session_key: &str,
        platform: &str,
        chat_id: &str,
        thread_id: Option<&str>,
        content: &str,
        owner_pid: u32,
        owner_started_at: Option<u64>,
        now: f64,
    ) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        conn.execute(
            "INSERT OR REPLACE INTO delivery_obligations
             (obligation_id, session_key, platform, chat_id, thread_id, content,
              state, attempts, created_at, updated_at, owner_pid, owner_started_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', 0, ?7, ?7, ?8, ?9)",
            params![
                obligation_id,
                session_key,
                platform,
                chat_id,
                thread_id,
                content,
                now,
                owner_pid,
                owner_started_at.map(|v| v as i64),
            ],
        )
        .map_err(|e| AgentError::session(e.to_string()))?;
        Ok(())
    }

    /// Transition one obligation's state (hermes `mark_attempting` /
    /// `mark_delivered` / `mark_failed` / abandon).
    pub fn set_obligation_state(
        &self,
        obligation_id: &str,
        state: &str,
        last_error: Option<&str>,
        now: f64,
    ) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        conn.execute(
            "UPDATE delivery_obligations SET state = ?2, updated_at = ?3, last_error = ?4
             WHERE obligation_id = ?1",
            params![obligation_id, state, now, last_error],
        )
        .map_err(|e| AgentError::session(e.to_string()))?;
        Ok(())
    }

    /// Claim undelivered obligations owned by dead processes for
    /// redelivery (hermes `sweep_recoverable`): re-stamps the owner to
    /// this process and bumps attempts atomically; rows past the
    /// attempts cap or stale cutoff flip to 'abandoned'. Returns
    /// (obligation_id, platform, chat_id, thread_id, content,
    /// needs_marker, attempts).
    pub fn sweep_obligations(
        &self,
        deliverable_platforms: &[String],
        owner_pid: u32,
        owner_started_at: Option<u64>,
        liveness: &dyn Fn(Option<i64>, Option<i64>) -> bool,
        max_attempts: i64,
        stale_after_seconds: f64,
        now: f64,
    ) -> Vec<(String, String, String, Option<String>, String, bool, i64)> {
        let Ok(conn) = self.conn.lock() else {
            return Vec::new();
        };
        let mut claimed = Vec::new();
        let rows: Vec<(
            String,
            String,
            String,
            Option<String>,
            String,
            String,
            i64,
            f64,
            Option<i64>,
            Option<i64>,
        )> = {
            let mut stmt = match conn.prepare(
                "SELECT obligation_id, platform, chat_id, thread_id, content, state,
                        attempts, created_at, owner_pid, owner_started_at
                 FROM delivery_obligations
                 WHERE state IN ('pending', 'attempting', 'failed')",
            ) {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };
            let mapped = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, f64>(7)?,
                    row.get::<_, Option<i64>>(8)?,
                    row.get::<_, Option<i64>>(9)?,
                ))
            });
            let Ok(mapped) = mapped else {
                return Vec::new();
            };
            mapped.filter_map(|r| r.ok()).collect()
        };
        for (
            oid,
            platform,
            chat_id,
            thread_id,
            content,
            state,
            attempts,
            created_at,
            opid,
            ostarted,
        ) in rows
        {
            if liveness(opid, ostarted) {
                continue; // a live gateway still owns this row
            }
            if attempts >= max_attempts || (now - created_at) > stale_after_seconds {
                let _ = conn.execute(
                    "UPDATE delivery_obligations SET state = 'abandoned', updated_at = ?2 WHERE obligation_id = ?1",
                    params![oid, now],
                );
                continue;
            }
            if !deliverable_platforms.is_empty() && !deliverable_platforms.contains(&platform) {
                // No adapter for this platform this boot — claiming would
                // spend an attempt on a no-op (hermes semantics).
                continue;
            }
            let updated = conn
                .execute(
                    "UPDATE delivery_obligations
                 SET owner_pid = ?2, owner_started_at = ?3, attempts = attempts + 1, updated_at = ?4
                 WHERE obligation_id = ?1 AND (owner_pid IS ?5 OR owner_pid = ?5)",
                    params![
                        oid,
                        owner_pid,
                        owner_started_at.map(|v| v as i64),
                        now,
                        opid,
                    ],
                )
                .unwrap_or(0);
            if updated > 0 {
                claimed.push((
                    oid,
                    platform,
                    chat_id,
                    thread_id,
                    content,
                    state != "pending",
                    attempts + 1,
                ));
            }
        }
        claimed
    }

    /// Retention prune: drop delivered/abandoned rows older than the
    /// retention window and cap the total row count (hermes `_prune`).
    pub fn prune_obligations(
        &self,
        retention_seconds: f64,
        max_rows: usize,
        now: f64,
    ) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        let cutoff = now - retention_seconds;
        conn.execute(
            "DELETE FROM delivery_obligations WHERE state IN ('delivered', 'abandoned') AND updated_at < ?1",
            params![cutoff],
        )
        .map_err(|e| AgentError::session(e.to_string()))?;
        let total: i64 = conn
            .query_row("SELECT COUNT(*) FROM delivery_obligations", [], |r| {
                r.get(0)
            })
            .unwrap_or(0);
        let excess = total.saturating_sub(max_rows as i64);
        if excess > 0 {
            conn.execute(
                "DELETE FROM delivery_obligations WHERE obligation_id IN (
                   SELECT obligation_id FROM delivery_obligations
                   ORDER BY CASE state WHEN 'delivered' THEN 0 WHEN 'abandoned' THEN 1 ELSE 2 END,
                            updated_at ASC
                   LIMIT ?1)",
                params![excess],
            )
            .map_err(|e| AgentError::session(e.to_string()))?;
        }
        Ok(())
    }

    /// Recent obligation rows, newest first (P706 ops surface; hermes
    /// ledger inspection). Content is omitted — the surface reports
    /// state and counts, not message bodies.
    pub fn list_obligations(&self, limit: usize) -> Vec<serde_json::Value> {
        let Ok(conn) = self.conn.lock() else {
            return Vec::new();
        };
        let mut stmt = match conn.prepare(
            "SELECT obligation_id, session_key, platform, chat_id, state, attempts,
                    created_at, updated_at, last_error
             FROM delivery_obligations ORDER BY updated_at DESC LIMIT ?1",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        stmt.query_map(params![limit as i64], |row| {
            Ok(serde_json::json!({
                "obligation_id": row.get::<_, String>(0)?,
                "session_key": row.get::<_, String>(1)?,
                "platform": row.get::<_, String>(2)?,
                "chat_id": row.get::<_, String>(3)?,
                "state": row.get::<_, String>(4)?,
                "attempts": row.get::<_, i64>(5)?,
                "created_at": row.get::<_, f64>(6)?,
                "updated_at": row.get::<_, f64>(7)?,
                "last_error": row.get::<_, Option<String>>(8)?,
            }))
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    }

    /// Test/inspection hook: all obligation rows ordered oldest first.
    #[cfg(test)]
    pub fn obligation_rows(&self) -> Vec<(String, String, String, i64)> {
        let Ok(conn) = self.conn.lock() else {
            return Vec::new();
        };
        let mut stmt = match conn.prepare(
            "SELECT obligation_id, platform, state, attempts FROM delivery_obligations ORDER BY created_at",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    }

    /// Undelivered finished delegations: (id, origin_session, result_json).
    pub fn undelivered_delegations(&self) -> Vec<(String, String, String)> {
        let Ok(conn) = self.conn.lock() else {
            return Vec::new();
        };
        let mut stmt = match conn.prepare(
            "SELECT delegation_id, origin_session, result_json FROM async_delegations
             WHERE state IN ('completed', 'unknown') AND result_json IS NOT NULL
             ORDER BY completed_at",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = match stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        }) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        rows.filter_map(|r| r.ok()).collect()
    }

    /// Abandon delegations still `running` from a previous process (their
    /// workers died with the owner). Hermes `recover_abandoned_delegations`:
    /// each row gets a terminal `unknown` outcome whose consolidated-shaped
    /// result flows through the normal delivery claim, so the conversation
    /// learns "outcome unknown" instead of silently waiting forever.
    /// Returns (id, origin_session) pairs.
    pub fn abandon_running_delegations(&self) -> Vec<(String, String)> {
        let Ok(conn) = self.conn.lock() else {
            return Vec::new();
        };
        let mut stmt = match conn.prepare(
            "SELECT delegation_id, origin_session, COALESCE(task_json, '[]')
             FROM async_delegations WHERE state = 'running'",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows: Vec<(String, String, String)> = match stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        }) {
            Ok(r) => r.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        };
        drop(stmt);
        let mut abandoned = Vec::new();
        for (id, origin, task_json) in rows {
            let mut goals: Vec<String> = serde_json::from_str::<serde_json::Value>(&task_json)
                .ok()
                .and_then(|v| {
                    v.as_array().map(|arr| {
                        arr.iter()
                            .map(|t| t["goal"].as_str().unwrap_or("(unknown task)").to_string())
                            .collect()
                    })
                })
                .unwrap_or_default();
            if goals.is_empty() {
                goals.push("(unknown task)".to_string());
            }
            let results: Vec<serde_json::Value> = goals
                .iter()
                .map(|goal| {
                    serde_json::json!({
                        "task": goal,
                        "status": "error",
                        "error": "Delegation owner exited before recording a terminal result; outcome unknown.",
                    })
                })
                .collect();
            let result = serde_json::json!({
                "delegation_id": id,
                "status": "unknown",
                "subagents": results.len(),
                "failed": results.len(),
                "results": results,
            });
            let result_json = serde_json::to_string(&result).unwrap_or_else(|_| "{}".to_string());
            conn.execute(
                "UPDATE async_delegations
                 SET state = 'unknown', completed_at = ?2, updated_at = ?2, result_json = ?3
                 WHERE delegation_id = ?1 AND state = 'running'",
                params![id, now_secs(), result_json],
            )
            .ok();
            abandoned.push((id, origin));
        }
        abandoned
    }

    /// All persisted delegation rows (newest first), for the gateway
    /// registry endpoint across restarts.
    pub fn delegation_rows(
        &self,
        limit: usize,
    ) -> Vec<(
        String,
        String,
        String,
        f64,
        Option<f64>,
        Option<String>,
        i64,
    )> {
        let Ok(conn) = self.conn.lock() else {
            return Vec::new();
        };
        let mut stmt = match conn.prepare(
            "SELECT delegation_id, origin_session, state, dispatched_at, completed_at,
                    result_json, delivery_attempts
             FROM async_delegations ORDER BY dispatched_at DESC LIMIT ?1",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = match stmt.query_map(params![limit as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, f64>(3)?,
                row.get::<_, Option<f64>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, i64>(6)?,
            ))
        }) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        rows.filter_map(|r| r.ok()).collect()
    }

    /// Current reactions on a message (hermes `get_message_reactions`).
    pub fn get_message_reactions(&self, session_id: &str, row_id: i64) -> Vec<serde_json::Value> {
        let Some(conn) = self.conn.lock().ok() else {
            return Vec::new();
        };
        let raw: Option<String> = conn
            .query_row(
                "SELECT display_metadata FROM messages WHERE id = ?1 AND session_id = ?2",
                params![row_id, session_id],
                |row| row.get(0),
            )
            .ok()
            .flatten();
        raw.as_deref()
            .filter(|s| !s.is_empty())
            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
            .and_then(|meta| meta.get("reactions").and_then(|v| v.as_array()).cloned())
            .unwrap_or_default()
    }

    /// Load all active messages of a session as `Message` values.
    pub fn load_messages(&self, session_id: &str) -> Result<Vec<Message>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT role, content, tool_call_id, tool_calls, tool_name, reasoning
                 FROM messages WHERE session_id = ?1 AND active = 1 ORDER BY id",
            )
            .map_err(|e| AgentError::session(e.to_string()))?;
        let rows = stmt
            .query_map(params![session_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            })
            .map_err(|e| AgentError::session(e.to_string()))?;
        let mut messages = Vec::new();
        for row in rows {
            let (role, content, tool_call_id, tool_calls, name, reasoning) =
                row.map_err(|e| AgentError::session(e.to_string()))?;
            let role = match role.as_str() {
                "system" => Role::System,
                "user" => Role::User,
                "assistant" => Role::Assistant,
                _ => Role::Tool,
            };
            let tool_calls: Option<Vec<ToolCall>> = tool_calls
                .map(|s| serde_json::from_str(&s))
                .transpose()
                .map_err(|e| AgentError::session(e.to_string()))?;
            messages.push(Message {
                reasoning,
                role,
                content,
                tool_calls,
                tool_call_id,
                name,
            });
        }
        Ok(messages)
    }

    /// Load messages with their stored timestamps (session export).
    pub fn load_messages_with_timestamps(&self, session_id: &str) -> Result<Vec<(f64, Message)>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT timestamp, role, content, tool_call_id, tool_calls, tool_name, reasoning
                 FROM messages WHERE session_id = ?1 AND active = 1 ORDER BY id",
            )
            .map_err(|e| AgentError::session(e.to_string()))?;
        let rows = stmt
            .query_map(params![session_id], |row| {
                Ok((
                    row.get::<_, f64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                ))
            })
            .map_err(|e| AgentError::session(e.to_string()))?;
        let mut messages = Vec::new();
        for row in rows {
            let (timestamp, role, content, tool_call_id, tool_calls, name, reasoning) =
                row.map_err(|e| AgentError::session(e.to_string()))?;
            let role = match role.as_str() {
                "system" => Role::System,
                "user" => Role::User,
                "assistant" => Role::Assistant,
                _ => Role::Tool,
            };
            let tool_calls: Option<Vec<ToolCall>> = tool_calls
                .map(|s| serde_json::from_str(&s))
                .transpose()
                .map_err(|e| AgentError::session(e.to_string()))?;
            messages.push((
                timestamp,
                Message {
                    reasoning,
                    role,
                    content,
                    tool_calls,
                    tool_call_id,
                    name,
                },
            ));
        }
        Ok(messages)
    }
    /// Full-text search over all messages. Returns (session_id, snippet, rank).
    pub fn search_messages(&self, query: &str, limit: usize) -> Result<Vec<(String, String)>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        let mut results = Vec::new();
        if self.has_fts {
            let fts_query = query
                .split_whitespace()
                .map(|token| format!("\"{}\"", token.replace('"', "")))
                .collect::<Vec<_>>()
                .join(" OR ");
            let sql = format!(
                "SELECT m.session_id, snippet(messages_fts, 0, '[', ']', '…', 12)
                 FROM messages_fts
                 JOIN messages m ON m.id = messages_fts.rowid
                 WHERE messages_fts MATCH ?1
                 ORDER BY rank LIMIT ?2"
            );
            match conn.prepare(&sql) {
                Ok(mut stmt) => {
                    let rows = stmt.query_map(params![fts_query, limit as i64], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    });
                    if let Ok(rows) = rows {
                        for row in rows.flatten() {
                            results.push(row);
                        }
                        return Ok(results);
                    }
                }
                Err(_) => {}
            }
        }
        // LIKE fallback
        let pattern = format!("%{}%", query);
        let mut stmt = conn
            .prepare(
                "SELECT session_id, substr(content, 1, 200)
                 FROM messages WHERE content LIKE ?1 AND active = 1
                 ORDER BY timestamp DESC LIMIT ?2",
            )
            .map_err(|e| AgentError::session(e.to_string()))?;
        for row in stmt
            .query_map(params![pattern, limit as i64], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|e| AgentError::session(e.to_string()))?
        {
            if let Ok(pair) = row {
                results.push(pair);
            }
        }
        Ok(results)
    }

    /// Update session token/usage counters.
    pub fn update_usage(
        &self,
        session_id: &str,
        input_tokens: u32,
        output_tokens: u32,
        tool_calls: u32,
    ) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        conn.execute(
            "UPDATE sessions SET input_tokens = input_tokens + ?2,
                output_tokens = output_tokens + ?3,
                tool_call_count = tool_call_count + ?4
             WHERE id = ?1",
            params![session_id, input_tokens, output_tokens, tool_calls],
        )
        .map_err(|e| AgentError::session(e.to_string()))?;
        Ok(())
    }

    /// Read one key out of a session's `model_config` JSON
    /// (hermes `get_session_model_config_value`, `hermes_state_sessions.py:830`).
    ///
    /// Returns `None` when the session row is missing, the key is absent, the
    /// column is NULL/empty, or the stored JSON does not parse — hermes'
    /// `_parse_model_config` is deliberately tolerant ("JSON text or dict → dict
    /// copy; anything else → {}"), because a corrupt blob must not break session
    /// load. Callers supply their own default.
    pub fn get_session_model_config_value(
        &self,
        session_id: &str,
        key: &str,
    ) -> Result<Option<Value>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        // 🔴 The column is nullable, so the row type must be Option<String>:
        // reading NULL as String errors instead of yielding None. Outer Option =
        // row missing; inner = column NULL. Both mean "no value".
        let raw: Option<Option<String>> = conn
            .query_row(
                "SELECT model_config FROM sessions WHERE id = ?1",
                params![session_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(|e| AgentError::session(e.to_string()))?;
        let Some(raw) = raw.flatten() else {
            return Ok(None);
        };
        Ok(parse_model_config(&raw).get(key).cloned())
    }

    /// Merge `patch` into a session's `model_config` JSON
    /// (hermes `patch_session_model_config`, `hermes_state_sessions.py:823`).
    ///
    /// * A key mapped to [`Value::Null`] is **removed** (hermes: "`None` deletes
    ///   a key"), which is how callers clear durable state such as an armed
    ///   cooldown or a prune runway marker.
    /// * The read-merge-write happens under the same connection guard, so a
    ///   concurrent writer cannot publish a stale copy over this one — hermes
    ///   wraps the identical sequence in `_execute_write` for the same reason.
    /// * No-op when the session row does not exist or the patch is empty, again
    ///   matching upstream. An empty merge writes NULL rather than `"{}"`.
    pub fn patch_session_model_config(
        &self,
        session_id: &str,
        patch: &Map<String, Value>,
    ) -> Result<()> {
        if session_id.is_empty() || patch.is_empty() {
            return Ok(());
        }
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        let raw: Option<Option<String>> = conn
            .query_row(
                "SELECT model_config FROM sessions WHERE id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| AgentError::session(e.to_string()))?;
        let Some(raw) = raw else {
            return Ok(()); // row missing → no-op, never an error
        };
        let mut config = parse_model_config(raw.as_deref().unwrap_or(""));
        for (key, value) in patch {
            if value.is_null() {
                config.remove(key);
            } else {
                config.insert(key.clone(), value.clone());
            }
        }
        let serialized = if config.is_empty() {
            None
        } else {
            serde_json::to_string(&Value::Object(config)).ok()
        };
        conn.execute(
            "UPDATE sessions SET model_config = ?2 WHERE id = ?1",
            params![session_id, serialized],
        )
        .map_err(|e| AgentError::session(e.to_string()))?;
        Ok(())
    }

    /// Close session rows orphaned by a dead process
    /// (hermes `sweep_orphaned_sessions`, `hermes_state_maintenance.py:155`).
    ///
    /// cron/kanban/subagent/one-shot CLI rows never set `ended_at` when their
    /// process dies, and `prune_sessions` only deletes **ended** rows — so without
    /// this sweep those rows are immortal and accumulate forever. This closes them
    /// (`end_reason = 'startup_orphan_reap'`); it is **non-destructive**: messages
    /// are kept, the row stays resumable, and because the UPDATE re-asserts
    /// `ended_at IS NULL` an existing reason always wins (hermes: "first-reason-wins").
    ///
    /// 🔴 Both `started_at` and last-activity must be older than the cutoff. The
    /// `started_at` predicate is what protects a freshly created branch/compression
    /// child, whose copied activity timestamp is old (hermes calls this out
    /// explicitly). Dropping it would close brand-new sessions.
    ///
    /// `sources` is a **whitelist**: only state-owned producers join automatic
    /// reconciliation, because messaging/UI sources have their own lifecycle owners
    /// and ending one of those triggers a routing loop. Unknown sources fail closed
    /// (hermes `_AUTO_PRUNE_STALE_OPEN_SOURCES`, #60609).
    pub fn sweep_orphaned_sessions(
        &self,
        max_idle_seconds: f64,
        sources: &[&str],
        exclude_pinned: bool,
    ) -> Result<Vec<String>> {
        // Upstream returns [] on a non-positive window or an empty source list.
        if max_idle_seconds <= 0.0 || sources.is_empty() {
            return Ok(Vec::new());
        }
        let cutoff = now_secs() - max_idle_seconds;
        let placeholders: Vec<String> = (0..sources.len()).map(|i| format!("?{}", i + 1)).collect();
        let pin_clause = if exclude_pinned {
            " AND COALESCE(s.pinned, 0) = 0"
        } else {
            ""
        };
        // 🔴 The last-active expression is inlined rather than reusing
        // filters::LAST_ACTIVE_EXPR because that constant is written against the
        // alias `s` in a FROM-less context; here the same alias is bound, so the
        // text is identical and stays consistent with the prune path.
        let sql = format!(
            "SELECT s.id FROM sessions s
              WHERE s.ended_at IS NULL
                AND s.started_at < ?{n}
                AND {last_active} < ?{n}
                AND s.source IN ({ph}){pin}",
            n = sources.len() + 1,
            last_active = crate::session::filters::LAST_ACTIVE_EXPR,
            ph = placeholders.join(", "),
            pin = pin_clause
        );
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        for s in sources {
            params.push(Box::new((*s).to_string()));
        }
        params.push(Box::new(cutoff));
        let refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();

        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| AgentError::session(e.to_string()))?;
        let ids: Vec<String> = stmt
            .query_map(refs.as_slice(), |row| row.get::<_, String>(0))
            .map_err(|e| AgentError::session(e.to_string()))?
            .filter_map(|r| r.ok())
            .collect();
        drop(stmt);
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut closed = Vec::with_capacity(ids.len());
        for id in &ids {
            // Re-assert `ended_at IS NULL` per row so a reason set concurrently (or
            // by an earlier pass) is never overwritten.
            let n = conn
                .execute(
                    "UPDATE sessions SET ended_at = ?2, end_reason = 'startup_orphan_reap'
                      WHERE id = ?1 AND ended_at IS NULL",
                    params![id, now_secs()],
                )
                .map_err(|e| AgentError::session(e.to_string()))?;
            if n > 0 {
                closed.push(id.clone());
            }
        }
        Ok(closed)
    }

    /// Reclaimable fraction of the database, `freelist_count / page_count`
    /// (hermes `_freelist_ratio`). Gates VACUUM in auto-maintenance so a small
    /// prune on a dense multi-GB database never triggers a full rewrite (#54189).
    /// `None` when either pragma fails — callers then fall back to the time throttle.
    pub fn freelist_ratio(&self) -> Option<f64> {
        let conn = self.conn.lock().ok()?;
        let freelist: i64 = conn
            .query_row("PRAGMA freelist_count", [], |r| r.get(0))
            .ok()?;
        let pages: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0)).ok()?;
        if pages <= 0 || freelist < 0 {
            return None;
        }
        Some(freelist as f64 / pages as f64)
    }

    /// Idempotent startup auto-maintenance, **never fails the caller**
    /// (hermes `maybe_auto_prune_and_vacuum`, `hermes_state_maintenance.py:421`).
    ///
    /// Prune ENDED sessions inactive for `retention_days`, then close stale open
    /// state-owned rows, then optionally VACUUM. Runs at most once per
    /// `min_interval_hours` (throttle lives in `state_meta`, shared across
    /// processes); VACUUM has its own `min_vacuum_interval_days` throttle plus
    /// the freelist-ratio gate. Designed to be called once at startup from
    /// long-lived entrypoints (CLI, gateway, cron scheduler). See #54189.
    ///
    /// Faithful upstream semantics kept here:
    /// - Negative `retention_days` **skips** (it would build a future cutoff and
    ///   match every ended session; `auto_prune = false` is the disable switch,
    ///   not a negative bound).
    /// - A non-blocking cross-process lock (`<db>.auto-maintenance.lock`) gates
    ///   the whole pass: two concurrent startups would otherwise both pass the
    ///   interval check and the second would prune a row the first had only just
    ///   closed recoverably. Lock unavailable → skip the pass.
    /// - Corrupt/absent `state_meta` timestamps mean "no prior run" → proceed
    ///   (hermes `_seconds_since` returns None on garbage).
    /// - **Prune FIRST, sweep SECOND**: an orphan closed by the sweep must get a
    ///   full retention window before it is ever prunable.
    /// - `last_auto_prune` is recorded even when nothing was pruned, so the
    ///   throttle holds.
    /// - Any error is captured in the report and logged; maintenance must never
    ///   block startup or refuse a turn.
    ///
    /// 🔴 VACUUM leg — NOT YET PORTED, fail-closed: upstream only VACUUMs after
    /// proving no *other* process or in-process DB generation holds the store or
    /// a WAL sidecar (`hermes_state_holders`: Linux /proc fd scan, Windows
    /// Restart Manager, in-process generation registry). VACUUM + TRUNCATE
    /// checkpoint retire a WAL generation a sibling writer still holds, and that
    /// is exactly the state where every agent then refuses turns (#110054).
    /// Upstream treats an unprovable scan as "holders present" and SKIPS — this
    /// port has no scan at all, so it can never prove quiescence and therefore
    /// always skips the VACUUM leg, recording why. The gate structure
    /// (`vacuum` flag, pruned>0, interval throttle, freelist ratio) is fully
    /// wired; porting the holders scan activates it. Manual `optimize_storage`
    /// remains the explicit VACUUM path. Pruning itself reclaims nothing on
    /// disk (SQLite reuses freed pages), so the file stays large until a manual
    /// optimize — the same tradeoff upstream makes when it cannot prove
    /// quiescence.
    pub fn maybe_auto_prune_and_vacuum(
        &self,
        retention_days: i64,
        min_interval_hours: u64,
        vacuum: bool,
        min_vacuum_interval_days: u64,
        min_vacuum_freelist_ratio: f64,
    ) -> AutoMaintenanceReport {
        let mut report = AutoMaintenanceReport::default();
        // Negative retention would build a future cutoff matching every ended
        // session; auto_prune=false is the disable switch, not a negative bound.
        if retention_days < 0 {
            tracing::warn!(
                retention_days,
                "state.db auto-maintenance skipped: sessions.retention_days is outside the \
                 allowed range (a whole number of days >= 0); set sessions.auto_prune = false \
                 to disable pruning"
            );
            report.skipped = true;
            return report;
        }

        // Non-blocking cross-process lock for one pass. WouldBlock = another
        // startup is mid-maintenance → skip (never wait: this is startup-path).
        let lock_path = self.path.with_file_name(format!(
            "{}.auto-maintenance.lock",
            file_name_str(&self.path)
        ));
        let _guard = match MaintenanceLock::try_acquire(&lock_path) {
            Ok(Some(g)) => g,
            Ok(None) => {
                report.skipped = true;
                return report;
            }
            Err(e) => {
                // Upstream warns and proceeds without the lock only when the lock
                // file itself cannot be opened; a failed *lock attempt* skips.
                tracing::warn!(?lock_path, error = %e, "could not open auto-maintenance lock; skipping pass");
                report.skipped = true;
                return report;
            }
        };

        // Everything below must never propagate an error: maintenance failures
        // are logged and reported, startup continues.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.auto_maintenance_inner(
                &mut report,
                retention_days,
                min_interval_hours,
                vacuum,
                min_vacuum_interval_days,
                min_vacuum_freelist_ratio,
            )
        }));
        if outcome.is_err() {
            report.error = Some("auto-maintenance panicked".to_string());
            tracing::warn!("state.db auto-maintenance failed: internal panic");
        }
        report
    }

    /// Body of `maybe_auto_prune_and_vacuum` after the retention guard and the
    /// cross-process lock; errors land in `report.error` (never raised).
    fn auto_maintenance_inner(
        &self,
        report: &mut AutoMaintenanceReport,
        retention_days: i64,
        min_interval_hours: u64,
        vacuum: bool,
        min_vacuum_interval_days: u64,
        min_vacuum_freelist_ratio: f64,
    ) {
        let now = now_secs();
        // Throttle: at most one pass per min_interval_hours, tracked in
        // state_meta so it holds across processes. Corrupt/absent = no prior
        // run = proceed (hermes _seconds_since).
        let since_prune = seconds_since(now, self.get_meta("last_auto_prune").ok().flatten());
        if let Some(s) = since_prune {
            if s < min_interval_hours as f64 * 3600.0 {
                report.skipped = true;
                return;
            }
        }

        // Prune FIRST: orphans closed by the sweep below must get a full
        // retention window before they are ever prunable.
        let cutoff = now - retention_days as f64 * 86400.0;
        let filters = crate::session::filters::PruneFilters {
            last_active_before: Some(cutoff),
            // include_pinned defaults false → pinned sessions are spared (①).
            ..Default::default()
        };
        let pruned = match self.prune_sessions(&filters) {
            Ok(n) => n,
            Err(e) => {
                report.error = Some(format!("prune: {e}"));
                tracing::warn!(error = %e, "state.db auto-maintenance: prune failed");
                return;
            }
        };
        report.pruned = pruned;

        // Then close stale open rows from state-owned sources (cron/kanban/
        // subagent/one-shot CLI never stamp ended_at when their process dies).
        match self.sweep_orphaned_sessions(
            retention_days as f64 * 86400.0,
            AUTO_PRUNE_STALE_OPEN_SOURCES,
            true,
        ) {
            Ok(closed) => report.closed = closed.len(),
            Err(e) => {
                // A failed sweep must not lose the prune that already happened.
                report.error = Some(format!("sweep: {e}"));
                tracing::warn!(error = %e, "state.db auto-maintenance: sweep failed");
            }
        }

        // VACUUM triple gate: enabled + rows actually freed + interval elapsed,
        // then the freelist-ratio admission, then the holders proof.
        if vacuum && pruned > 0 {
            let since_vacuum = seconds_since(now, self.get_meta("last_vacuum").ok().flatten());
            let vacuum_due = since_vacuum.is_none()
                || since_vacuum.unwrap() >= min_vacuum_interval_days as f64 * 86400.0;
            if vacuum_due {
                let ratio = self.freelist_ratio();
                report.freelist_ratio = ratio;
                // 🔴 NOT YET PORTED (fail-closed): the holders scan
                // (hermes_state_holders) that proves no sibling process or
                // in-process DB generation holds this store or its WAL sidecar.
                // Upstream skips VACUUM whenever quiescence is unproven
                // (#110054); with no scan, it is always unproven here.
                report.vacuum_skipped_reason = Some(
                    "holder scan not ported; quiescence unproven (#110054 fail-closed)".into(),
                );
                if ratio.is_none() || ratio.unwrap() > min_vacuum_freelist_ratio {
                    // Ratio gate passed — would VACUUM here once the holders
                    // scan exists. Left unreachable-by-design, see above.
                    tracing::debug!(
                        ?ratio,
                        "state.db auto-maintenance: VACUUM eligible but skipped (no holders scan)"
                    );
                } else {
                    tracing::debug!(
                        ratio = ratio.unwrap_or(0.0) * 100.0,
                        threshold = min_vacuum_freelist_ratio * 100.0,
                        "state.db auto-maintenance: skipping VACUUM, too few pages reclaimable"
                    );
                }
            }
        }

        // Record even when pruned == 0 so the throttle holds.
        if let Err(e) = self.set_meta("last_auto_prune", &now.to_string()) {
            tracing::warn!(error = %e, "could not record last_auto_prune");
        }
        if report.closed > 0 || pruned > 0 {
            tracing::info!(
                closed = report.closed,
                pruned,
                retention_days,
                "state.db auto-maintenance: closed stale open sessions and pruned inactive ones"
            );
        }
    }

    /// Persisted ineffective-compaction strike count for one session (P3-5c,
    /// hermes `get_compression_ineffective_count`,
    /// `hermes_state_compression.py:463`).
    ///
    /// The durable half of the anti-thrash guard: a fresh compressor bound to a
    /// resumed session inherits an armed/tripped guard across restarts, so a
    /// rebuild never disarms it (#69872, #54923 "restart must not disarm").
    /// A missing row or a NULL/absent column reads as 0.
    pub fn get_compression_ineffective_count(&self, session_id: &str) -> i64 {
        let conn = match self.conn.lock() {
            Ok(c) => c,
            Err(_) => return 0,
        };
        conn.query_row(
            "SELECT compression_ineffective_count FROM sessions WHERE id = ?1",
            params![session_id],
            |row| row.get::<_, Option<i64>>(0),
        )
        .ok()
        .flatten()
        // Upstream clamps at write time (`max(0, int(count))`); clamp on read
        // too so a hand-edited negative row can never lower the trip threshold.
        .map(|n| n.max(0))
        .unwrap_or(0)
    }

    /// Persist the strike count (hermes `set_compression_ineffective_count`).
    /// Best-effort: a store failure must never fail a turn, so errors are
    /// logged rather than returned — upstream's `_durable_write` likewise only
    /// reports success/failure.
    pub fn set_compression_ineffective_count(&self, session_id: &str, count: i64) {
        if session_id.is_empty() {
            return;
        }
        let Ok(conn) = self.conn.lock() else { return };
        if let Err(e) = conn.execute(
            "UPDATE sessions SET compression_ineffective_count = ?2 WHERE id = ?1",
            params![session_id, count.max(0)],
        ) {
            tracing::debug!(error = %e, "could not persist compression ineffective count");
        }
    }

    /// Persisted anti-thrash recovery deadline as a **wall-clock epoch**
    /// (P3-5c, hermes `get_compression_recovery_deadline`,
    /// `hermes_state_compression.py:474`). `0.0` = not armed.
    ///
    /// 🔴 Wall clock, never monotonic, and durable: the compressor is rebuilt
    /// (P3-5d LRU eviction, or a restart), and a process-local deadline would
    /// restart the wait on every rebuild so a tripped session never earned its
    /// probe — the exact bug hermes fixed in #100185 ("a blocked messaging
    /// session never earned its probe and stayed blocked forever").
    pub fn get_compression_recovery_deadline(&self, session_id: &str) -> f64 {
        let conn = match self.conn.lock() {
            Ok(c) => c,
            Err(_) => return 0.0,
        };
        conn.query_row(
            "SELECT compression_recovery_deadline FROM sessions WHERE id = ?1",
            params![session_id],
            |row| row.get::<_, Option<f64>>(0),
        )
        .ok()
        .flatten()
        .unwrap_or(0.0)
        .max(0.0)
    }

    /// Persist (or disarm) the recovery deadline. `0.0` writes **NULL**, not
    /// zero — hermes `_write_session_column(…, normalized or None)` — so an
    /// unarmed state is indistinguishable from "never armed" on read.
    pub fn set_compression_recovery_deadline(&self, session_id: &str, deadline: f64) {
        if session_id.is_empty() {
            return;
        }
        let normalized = if deadline.is_finite() && deadline > 0.0 {
            Some(deadline)
        } else {
            None
        };
        let Ok(conn) = self.conn.lock() else { return };
        if let Err(e) = conn.execute(
            "UPDATE sessions SET compression_recovery_deadline = ?2 WHERE id = ?1",
            params![session_id, normalized],
        ) {
            tracing::debug!(error = %e, "could not persist compression recovery deadline");
        }
    }

    /// Durable list of reasoning-echo routes that have rejected
    /// `reasoning_content` (hermes `_REJECTION_MODEL_CONFIG_KEY`, read by
    /// `rejected_reasoning_carriers` / written by
    /// `record_reasoning_field_rejection`).
    ///
    /// This is what makes the P3-1d learner survive a `--resume` or a gateway
    /// restart instead of re-learning with another 400. The row shape matches
    /// upstream — `[provider, host, model, [carriers…]]` — so the 4-tuple stays
    /// available if the wire ever grows upstream's extra carriers.
    ///
    /// Malformed rows are dropped, never propagated: a corrupt blob must degrade
    /// to "nothing learned yet", not to a session-load failure.
    pub fn rejected_reasoning_routes(
        &self,
        session_id: &str,
    ) -> Result<Vec<RejectedReasoningRoute>> {
        let Some(value) =
            self.get_session_model_config_value(session_id, REJECTED_REASONING_KEY)?
        else {
            return Ok(Vec::new());
        };
        let Some(rows) = value.as_array() else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for row in rows {
            let Some(cols) = row.as_array() else { continue };
            // hermes validates `len(entry) == 4 and isinstance(entry[3], list)`.
            if cols.len() != 4 {
                continue;
            }
            let Some(carriers) = cols[3].as_array() else {
                continue;
            };
            // 🔴 A row whose key columns are not strings is corrupt, not
            // "empty": coercing with unwrap_or("") would fabricate a route
            // keyed ("","","") that could then match nothing — or worse, match
            // a real all-empty route. Drop the row.
            let (Some(provider), Some(host), Some(model)) =
                (cols[0].as_str(), cols[1].as_str(), cols[2].as_str())
            else {
                continue;
            };
            // 🔴 Filter to the known carrier whitelist, exactly as upstream does
            // (`update(k for k in entry[3] if k in _ALL_CARRIERS)`). A corrupt or
            // hand-edited blob must not be able to inject arbitrary strings into
            // the replay decision.
            let carriers: Vec<String> = carriers
                .iter()
                .filter_map(|c| c.as_str().map(str::to_string))
                .filter(|c| KNOWN_REASONING_CARRIERS.contains(&c.as_str()))
                .collect();
            out.push(RejectedReasoningRoute {
                provider: provider.to_string(),
                host: host.to_string(),
                model: model.to_string(),
                carriers,
            });
        }
        Ok(out)
    }

    /// Persist the rejected-route table (hermes' patcher call). Sorted by
    /// (provider, host, model) so rewriting the same set is byte-stable; an
    /// empty table writes `Null`, which removes the key and leaves the column
    /// NULL rather than `"[]"`.
    pub fn set_rejected_reasoning_routes(
        &self,
        session_id: &str,
        routes: &[RejectedReasoningRoute],
    ) -> Result<()> {
        let mut rows: Vec<&RejectedReasoningRoute> = routes.iter().collect();
        rows.sort_by(|a, b| {
            (&a.provider, &a.host, &a.model).cmp(&(&b.provider, &b.host, &b.model))
        });
        let value: Vec<Value> = rows
            .into_iter()
            .map(|r| {
                let mut carriers = r.carriers.clone();
                carriers.sort();
                Value::Array(vec![
                    Value::from(r.provider.clone()),
                    Value::from(r.host.clone()),
                    Value::from(r.model.clone()),
                    Value::Array(carriers.into_iter().map(Value::from).collect()),
                ])
            })
            .collect();
        let mut patch = Map::new();
        patch.insert(
            REJECTED_REASONING_KEY.to_string(),
            if value.is_empty() {
                Value::Null
            } else {
                Value::Array(value)
            },
        );
        self.patch_session_model_config(session_id, &patch)
    }

    /// Ensure a session row exists (create it if missing). Used by the
    /// gateway to resume a caller-supplied session id.
    pub fn ensure_session(
        &self,
        id: &str,
        source: &str,
        model: Option<&str>,
        cwd: Option<&str>,
    ) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        conn.execute(
            "INSERT INTO sessions (id, source, model, cwd, started_at, last_activity_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)
             ON CONFLICT(id) DO UPDATE SET last_activity_at = excluded.last_activity_at",
            params![id, source, model, cwd, now_secs()],
        )
        .map_err(|e| AgentError::session(format!("ensure session: {}", e)))?;
        Ok(())
    }

    /// One session row, if present.
    pub fn get_session_row(&self, session_id: &str) -> Result<Option<SessionRow>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        conn.query_row(
            "SELECT id, source, model, title, cwd, parent_session_id, started_at,
                    COALESCE(last_activity_at, started_at),
                    ended_at, end_reason, message_count, input_tokens, output_tokens,
                    archived, pinned, tool_call_count
             FROM sessions WHERE id = ?1",
            params![session_id],
            |row| {
                Ok(SessionRow {
                    id: row.get(0)?,
                    source: row.get(1)?,
                    model: row.get(2)?,
                    title: row.get(3)?,
                    cwd: row.get(4)?,
                    parent_session_id: row.get(5)?,
                    started_at: row.get(6)?,
                    last_activity_at: row.get(7)?,
                    ended_at: row.get(8)?,
                    end_reason: row.get(9)?,
                    message_count: row.get(10)?,
                    input_tokens: row.get(11)?,
                    output_tokens: row.get(12)?,
                    archived: row.get::<_, i64>(13)? != 0,
                    pinned: row.get::<_, i64>(14)? != 0,
                    tool_call_count: row.get(15)?,
                })
            },
        )
        .optional()
        .map_err(|e| AgentError::session(e.to_string()))
    }

    /// P553: ids of sessions forked from `parent_id`, oldest first —
    /// the gateway enriches single-session fetches with this lineage.
    pub fn child_session_ids(&self, parent_id: &str) -> Result<Vec<String>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        let mut stmt = match conn
            .prepare("SELECT id FROM sessions WHERE parent_session_id = ?1 ORDER BY started_at")
        {
            Ok(s) => s,
            Err(e) => return Err(AgentError::session(e.to_string())),
        };
        let rows = match stmt.query_map(params![parent_id], |row| row.get::<_, String>(0)) {
            Ok(r) => r,
            Err(e) => return Err(AgentError::session(e.to_string())),
        };
        rows.collect::<std::result::Result<Vec<String>, _>>()
            .map_err(|e| AgentError::session(e.to_string()))
    }

    /// Recent sessions, newest first.
    pub fn list_session_rows(&self, limit: usize) -> Result<Vec<SessionRow>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT id, source, model, title, cwd, parent_session_id, started_at,
                        COALESCE(last_activity_at, started_at),
                        ended_at, end_reason, message_count, input_tokens, output_tokens,
                        archived, pinned, tool_call_count
                 FROM sessions ORDER BY started_at DESC LIMIT ?1",
            )
            .map_err(|e| AgentError::session(e.to_string()))?;
        let rows = stmt
            .query_map(params![limit as i64], |row| {
                Ok(SessionRow {
                    id: row.get(0)?,
                    source: row.get(1)?,
                    model: row.get(2)?,
                    title: row.get(3)?,
                    cwd: row.get(4)?,
                    parent_session_id: row.get(5)?,
                    started_at: row.get(6)?,
                    last_activity_at: row.get(7)?,
                    ended_at: row.get(8)?,
                    end_reason: row.get(9)?,
                    message_count: row.get(10)?,
                    input_tokens: row.get(11)?,
                    output_tokens: row.get(12)?,
                    archived: row.get::<_, i64>(13)? != 0,
                    pinned: row.get::<_, i64>(14)? != 0,
                    tool_call_count: row.get(15)?,
                })
            })
            .map_err(|e| AgentError::session(e.to_string()))?;
        let mut sessions = Vec::new();
        for row in rows {
            sessions.push(row.map_err(|e| AgentError::session(e.to_string()))?);
        }
        Ok(sessions)
    }

    /// P509: last non-empty active message preview per session, batched
    /// in one query for list-row snippets. Previews are whitespace-
    /// trimmed, newline-collapsed and truncated to `max_chars` with an
    /// ellipsis; sessions without messages are omitted.
    pub fn last_message_previews(
        &self,
        session_ids: &[String],
        max_chars: usize,
    ) -> Result<HashMap<String, String>> {
        let mut previews = HashMap::new();
        if session_ids.is_empty() || max_chars == 0 {
            return Ok(previews);
        }
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        let placeholders = vec!["?"; session_ids.len()].join(",");
        let sql = format!(
            "SELECT m.session_id, m.content FROM messages m
             JOIN (SELECT session_id, MAX(id) AS max_id FROM messages
                   WHERE session_id IN ({placeholders}) AND active = 1
                         AND content IS NOT NULL AND TRIM(content) <> ''
                   GROUP BY session_id) latest
             ON m.session_id = latest.session_id AND m.id = latest.max_id"
        );
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| AgentError::session(e.to_string()))?;
        let params: Vec<&dyn rusqlite::ToSql> = session_ids
            .iter()
            .map(|id| id as &dyn rusqlite::ToSql)
            .collect();
        let rows = stmt
            .query_map(params.as_slice(), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|e| AgentError::session(e.to_string()))?;
        for row in rows {
            let (session_id, content) = row.map_err(|e| AgentError::session(e.to_string()))?;
            let collapsed: String = content.split_whitespace().collect::<Vec<_>>().join(" ");
            let char_count = collapsed.chars().count();
            let mut preview: String = collapsed.chars().take(max_chars).collect();
            if char_count > max_chars {
                preview.push('…');
            }
            previews.insert(session_id, preview);
        }
        Ok(previews)
    }

    /// Per-model usage aggregation since a UNIX cutoff (hermes
    /// `_get_models_analytics` grouping): sessions, messages, tokens and
    /// last use per model, largest footprint first.
    pub fn model_usage_since(&self, cutoff: f64) -> Result<Vec<ModelUsageRow>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT model,
                        COUNT(*) AS sessions,
                        SUM(COALESCE(message_count, 0)) AS messages,
                        SUM(COALESCE(input_tokens, 0)) AS input_tokens,
                        SUM(COALESCE(output_tokens, 0)) AS output_tokens,
                        MAX(started_at) AS last_used_at
                 FROM sessions
                 WHERE started_at > ?1 AND model IS NOT NULL AND model != ''
                 GROUP BY model
                 ORDER BY SUM(COALESCE(input_tokens, 0)) + SUM(COALESCE(output_tokens, 0)) DESC",
            )
            .map_err(|e| AgentError::session(e.to_string()))?;
        let rows = stmt
            .query_map(params![cutoff], |row| {
                Ok(ModelUsageRow {
                    model: row.get(0)?,
                    sessions: row.get(1)?,
                    messages: row.get(2)?,
                    input_tokens: row.get(3)?,
                    output_tokens: row.get(4)?,
                    last_used_at: row.get(5)?,
                })
            })
            .map_err(|e| AgentError::session(e.to_string()))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| AgentError::session(e.to_string()))?);
        }
        Ok(out)
    }

    /// Gateway platform conversations for the MCP channel bridge (hermes
    /// mcp_serve `_load_sessions_index_from_db`): rows whose routing key
    /// marks a platform chat (`platform-<name>-<chat>`). ulnclaw stores
    /// the key as the row id (session_key stays NULL on legacy rows), so
    /// both columns are considered. Newest activity first.
    pub fn list_platform_sessions(&self, limit: usize) -> Result<Vec<PlatformSessionRow>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT id, source, user_id, session_key, title, started_at,
                        last_activity_at, input_tokens, output_tokens
                 FROM sessions
                 WHERE COALESCE(session_key, id) LIKE 'platform-%' AND archived = 0
                 ORDER BY COALESCE(last_activity_at, started_at) DESC
                 LIMIT ?1",
            )
            .map_err(|e| AgentError::session(e.to_string()))?;
        let rows = stmt
            .query_map(params![limit as i64], |row| {
                let id: String = row.get(0)?;
                let session_key: Option<String> = row.get(3)?;
                Ok(PlatformSessionRow {
                    id: id.clone(),
                    source: row.get(1)?,
                    user_id: row.get(2)?,
                    session_key: session_key.unwrap_or(id),
                    title: row.get(4)?,
                    started_at: row.get(5)?,
                    last_activity_at: row.get(6)?,
                    input_tokens: row.get(7)?,
                    output_tokens: row.get(8)?,
                })
            })
            .map_err(|e| AgentError::session(e.to_string()))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| AgentError::session(e.to_string()))?);
        }
        Ok(out)
    }

    /// Messages of a session with row ids and timestamps (hermes
    /// `get_messages` for the MCP bridge): chronological order, every
    /// role included.
    pub fn load_message_rows(&self, session_id: &str) -> Result<Vec<MessageRow>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT id, role, COALESCE(content, ''), timestamp
                 FROM messages WHERE session_id = ?1 AND active = 1
                 ORDER BY id ASC",
            )
            .map_err(|e| AgentError::session(e.to_string()))?;
        let rows = stmt
            .query_map(params![session_id], |row| {
                Ok(MessageRow {
                    id: row.get(0)?,
                    role: row.get(1)?,
                    content: row.get(2)?,
                    timestamp: row.get(3)?,
                })
            })
            .map_err(|e| AgentError::session(e.to_string()))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| AgentError::session(e.to_string()))?);
        }
        Ok(out)
    }

    /// All-time totals across every stored session:
    /// `(session_count, input_tokens, output_tokens)`.
    pub fn token_totals(&self) -> Result<(i64, i64, i64)> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0)
             FROM sessions",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(|e| AgentError::session(e.to_string()))
    }

    /// Mark a session ended.
    pub fn end_session(&self, session_id: &str, reason: &str) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        conn.execute(
            "UPDATE sessions SET ended_at = ?2, end_reason = ?3 WHERE id = ?1",
            params![session_id, now_secs(), reason],
        )
        .map_err(|e| AgentError::session(e.to_string()))?;
        Ok(())
    }

    /// Clear a previously recorded end (unarchive — desktop P414): drops
    /// `ended_at`/`end_reason` so the session reads as open again.
    pub fn clear_session_end(&self, session_id: &str) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        conn.execute(
            "UPDATE sessions SET ended_at = NULL, end_reason = NULL WHERE id = ?1",
            params![session_id],
        )
        .map_err(|e| AgentError::session(e.to_string()))?;
        Ok(())
    }

    /// Set (or clear, with an empty string) a session title. Titles must not
    /// contain newlines or NUL bytes.
    /// Set or change a session's title (hermes `set_session_title`).
    /// Titles are sanitized and must be unique across sessions; an
    /// empty/whitespace-only title clears it. Errors when the session
    /// does not exist.
    pub fn set_session_title(&self, session_id: &str, title: &str) -> Result<()> {
        let cleaned = sanitize_title(title).map_err(AgentError::session)?;
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        if let Some(cleaned) = &cleaned {
            let conflict: Option<String> = conn
                .query_row(
                    "SELECT id FROM sessions WHERE title = ?1 AND id != ?2",
                    params![cleaned, session_id],
                    |r| r.get(0),
                )
                .ok();
            if let Some(conflict_id) = conflict {
                return Err(AgentError::session(format!(
                    "Title '{cleaned}' is already in use by session {conflict_id}"
                )));
            }
        }
        let changed = conn
            .execute(
                "UPDATE sessions SET title = ?2 WHERE id = ?1",
                params![session_id, cleaned],
            )
            .map_err(|e| AgentError::session(e.to_string()))?;
        if changed == 0 {
            return Err(AgentError::session(format!(
                "session '{session_id}' not found"
            )));
        }
        Ok(())
    }

    /// Rows for the interactive session browser (hermes
    /// `list_sessions_rich` browse surface): newest activity first,
    /// optional exact-source filter and source exclusion list, with the
    /// first non-empty user message as preview.
    pub fn list_sessions_for_browse(
        &self,
        limit: usize,
        source: Option<&str>,
        exclude_sources: &[&str],
        include_archived: bool,
    ) -> Result<Vec<BrowseRow>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        let mut sql = String::from(
            "SELECT s.id, s.title, s.source,
                    COALESCE(s.last_activity_at, s.started_at),
                    (SELECT m.content FROM messages m
                     WHERE m.session_id = s.id AND m.role = 'user'
                       AND m.content IS NOT NULL AND m.content != ''
                     ORDER BY m.id LIMIT 1),
                    s.cwd,
                    s.archived,
                    s.message_count,
                    s.model,
                    s.end_reason,
                    COALESCE(s.input_tokens, 0) + COALESCE(s.output_tokens, 0),
                    s.started_at
             FROM sessions s WHERE 1=1",
        );
        if !include_archived {
            sql.push_str(" AND s.archived = 0");
        }
        let mut values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(source) = source {
            sql.push_str(" AND s.source = ?");
            values.push(Box::new(source.to_string()));
        }
        for excluded in exclude_sources {
            sql.push_str(" AND s.source != ?");
            values.push(Box::new((*excluded).to_string()));
        }
        sql.push_str(" ORDER BY COALESCE(s.last_activity_at, s.started_at) DESC LIMIT ?");
        values.push(Box::new(limit as i64));
        let params: Vec<&dyn rusqlite::ToSql> = values.iter().map(|p| p.as_ref()).collect();
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| AgentError::session(e.to_string()))?;
        let rows = stmt
            .query_map(params.as_slice(), |row| {
                Ok(BrowseRow {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    source: row.get(2)?,
                    last_active: row.get(3)?,
                    preview: row.get(4)?,
                    cwd: row.get(5)?,
                    archived: row.get::<_, i64>(6)? != 0,
                    message_count: row.get(7)?,
                    model: row.get(8)?,
                    end_reason: row.get(9)?,
                    total_tokens: row.get(10)?,
                    started_at: row.get(11)?,
                })
            })
            .map_err(|e| AgentError::session(e.to_string()))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| AgentError::session(e.to_string()))
    }

    /// Titled sessions whose first user turn was a `/skill` invocation
    /// (hermes `list_skill_scaffolded_sessions`), newest first. Those
    /// titles were generated from the expanded scaffold, so they describe
    /// the skill rather than the request.
    pub fn list_skill_scaffolded_sessions(&self, limit: usize) -> Result<Vec<SkillScaffoldedRow>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT s.id, s.title, m.content
                 FROM sessions s
                 JOIN messages m ON m.id = (
                     SELECT m2.id FROM messages m2
                     WHERE m2.session_id = s.id AND m2.role = 'user'
                       AND m2.content IS NOT NULL
                     ORDER BY m2.timestamp, m2.id LIMIT 1
                 )
                 WHERE s.title IS NOT NULL
                   AND m.content LIKE ?1
                 ORDER BY s.started_at DESC
                 LIMIT ?2",
            )
            .map_err(|e| AgentError::session(e.to_string()))?;
        let rows = stmt
            .query_map(
                params![
                    crate::session::retitle::SKILL_SCAFFOLD_SQL_LIKE,
                    limit as i64
                ],
                |row| {
                    Ok(SkillScaffoldedRow {
                        id: row.get(0)?,
                        title: row.get(1)?,
                        content: row.get(2)?,
                    })
                },
            )
            .map_err(|e| AgentError::session(e.to_string()))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| AgentError::session(e.to_string()))
    }

    /// The session's first assistant reply as plain text, empty when none
    /// (hermes `get_first_assistant_text`).
    pub fn get_first_assistant_text(&self, session_id: &str) -> Result<String> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        conn.query_row(
            "SELECT content FROM messages
             WHERE session_id = ?1 AND role = 'assistant' AND content IS NOT NULL
             ORDER BY timestamp, id LIMIT 1",
            params![session_id],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .map(|v| v.unwrap_or_default())
        .map_err(|e| AgentError::session(e.to_string()))
    }

    /// P559: first user message text of a session (empty string when the
    /// session has none) — the single-session retitler's raw material.
    pub fn get_first_user_text(&self, session_id: &str) -> Result<String> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        conn.query_row(
            "SELECT content FROM messages
             WHERE session_id = ?1 AND role = 'user' AND content IS NOT NULL
             ORDER BY timestamp, id LIMIT 1",
            params![session_id],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .map(|v| v.unwrap_or_default())
        .map_err(|e| AgentError::session(e.to_string()))
    }

    /// P561: per-role message counts for a session (GROUP BY role) —
    /// the gateway enriches single-session fetches with this census.
    pub fn message_role_counts(&self, session_id: &str) -> Result<Vec<(String, i64)>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        let mut stmt = match conn.prepare(
            "SELECT role, COUNT(*) FROM messages WHERE session_id = ?1 GROUP BY role ORDER BY role",
        ) {
            Ok(s) => s,
            Err(e) => return Err(AgentError::session(e.to_string())),
        };
        let rows = match stmt.query_map(params![session_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        }) {
            Ok(r) => r,
            Err(e) => return Err(AgentError::session(e.to_string())),
        };
        rows.collect::<std::result::Result<Vec<(String, i64)>, _>>()
            .map_err(|e| AgentError::session(e.to_string()))
    }

    /// P564: text of the session's most recent message with content
    /// (any role; empty string when the session has none).
    pub fn get_last_message_text(&self, session_id: &str) -> Result<String> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        conn.query_row(
            "SELECT content FROM messages
             WHERE session_id = ?1 AND content IS NOT NULL
             ORDER BY timestamp DESC, id DESC LIMIT 1",
            params![session_id],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .map(|v| v.unwrap_or_default())
        .map_err(|e| AgentError::session(e.to_string()))
    }

    /// Next free title in a lineage: `"my session"` → `"my session #2"`
    /// when the base is taken (hermes `get_next_title_in_lineage`).
    pub fn get_next_title_in_lineage(&self, base_title: &str) -> Result<String> {
        // Strip an existing " #N" suffix to find the true base.
        let base = match base_title.rfind(" #") {
            Some(idx)
                if base_title[idx + 2..].chars().all(|c| c.is_ascii_digit())
                    && !base_title[idx + 2..].is_empty() =>
            {
                &base_title[..idx]
            }
            _ => base_title,
        };
        let escaped = base
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        let mut stmt = conn
            .prepare("SELECT title FROM sessions WHERE title = ?1 OR title LIKE ?2 ESCAPE '\\'")
            .map_err(|e| AgentError::session(e.to_string()))?;
        let titles: Vec<String> = stmt
            .query_map(params![base, format!("{escaped} #%")], |r| {
                r.get::<_, String>(0)
            })
            .map_err(|e| AgentError::session(e.to_string()))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| AgentError::session(e.to_string()))?;
        if titles.is_empty() {
            return Ok(base.to_string());
        }
        // The unnumbered original counts as #1.
        let mut max_num: u64 = 1;
        for title in &titles {
            if let Some(idx) = title.rfind(" #") {
                if let Ok(n) = title[idx + 2..].parse::<u64>() {
                    max_num = max_num.max(n);
                }
            }
        }
        Ok(format!("{base} #{}", max_num + 1))
    }

    /// Most recent non-archived session id by last activity (hermes
    /// `--continue` target selection). `None` when the store is empty.
    pub fn latest_session_id(&self) -> Result<Option<String>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        conn.query_row(
            "SELECT id FROM sessions WHERE archived = 0
             ORDER BY COALESCE(last_activity_at, started_at) DESC LIMIT 1",
            [],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .map_err(|e| AgentError::session(e.to_string()))
    }

    /// Resolve an exact session id or a uniquely prefixed prefix to the
    /// full id (hermes `resolve_session_id`). Returns `None` for no
    /// match or an ambiguous prefix.
    pub fn resolve_session_id(&self, id_or_prefix: &str) -> Result<Option<String>> {
        if self.get_session_row(id_or_prefix)?.is_some() {
            return Ok(Some(id_or_prefix.to_string()));
        }
        let escaped = id_or_prefix
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT id FROM sessions WHERE id LIKE ?1 ESCAPE '\\'
                 ORDER BY started_at DESC LIMIT 2",
            )
            .map_err(|e| AgentError::session(e.to_string()))?;
        let rows = stmt
            .query_map(params![format!("{escaped}%")], |row| {
                row.get::<_, String>(0)
            })
            .map_err(|e| AgentError::session(e.to_string()))?;
        let matches: Vec<String> = rows
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| AgentError::session(e.to_string()))?;
        match matches.len() {
            1 => Ok(Some(matches.into_iter().next().unwrap())),
            _ => Ok(None),
        }
    }

    /// Reclaim disk space: FTS5 segment merge + VACUUM, no data change
    /// (hermes `sessions optimize`). Returns the number of FTS indexes
    /// merged.
    pub fn optimize_storage(&self) -> Result<usize> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        let mut optimized = 0usize;
        if self.has_fts
            && conn
                .execute(
                    "INSERT INTO messages_fts(messages_fts) VALUES('optimize')",
                    [],
                )
                .is_ok()
        {
            optimized = 1;
        }
        // Best-effort WAL checkpoint first so VACUUM rewrites a folded DB.
        conn.execute("PRAGMA wal_checkpoint(TRUNCATE)", []).ok();
        conn.execute("VACUUM", [])
            .map_err(|e| AgentError::session(e.to_string()))?;
        Ok(optimized)
    }

    /// Database size in bytes as SQLite itself accounts for it
    /// (`page_count * page_size`) — the size the main file will have once
    /// the WAL is checkpointed back. Prefer this over the on-disk file
    /// size when reporting VACUUM wins (hermes `logical_size_bytes`).
    pub fn logical_size_bytes(&self) -> Option<u64> {
        let conn = self.conn.lock().ok()?;
        let page_count: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0)).ok()?;
        let page_size: i64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0)).ok()?;
        if page_count < 0 || page_size < 0 {
            return None;
        }
        Some(page_count as u64 * page_size as u64)
    }

    /// Current title of a session (`None` when the session exists but has
    /// no title, or when the session id is unknown).
    pub fn get_session_title(&self, session_id: &str) -> Result<Option<String>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        let mut stmt = conn
            .prepare("SELECT title FROM sessions WHERE id = ?1")
            .map_err(|e| AgentError::session(e.to_string()))?;
        let mut rows = stmt
            .query(params![session_id])
            .map_err(|e| AgentError::session(e.to_string()))?;
        match rows
            .next()
            .map_err(|e| AgentError::session(e.to_string()))?
        {
            Some(row) => row
                .get::<_, Option<String>>(0)
                .map_err(|e| AgentError::session(e.to_string())),
            None => Ok(None),
        }
    }

    /// Resolve a session title to a session id, preferring the latest in
    /// a lineage (hermes `resolve_session_by_title`): when numbered
    /// variants ("title #2", "title #3", ...) exist, return the newest
    /// one; otherwise fall back to the exact-title session. Archived
    /// sessions are skipped. LIKE wildcards in the query title are
    /// escaped so `%`/`_` titles match literally.
    pub fn resolve_session_by_title(&self, title: &str) -> Result<Option<String>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        let escaped = title
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let mut stmt = conn
            .prepare(
                "SELECT id FROM sessions WHERE title LIKE ?1 ESCAPE '\\' AND archived = 0
                 ORDER BY started_at DESC",
            )
            .map_err(|e| AgentError::session(e.to_string()))?;
        let numbered: Vec<String> = stmt
            .query_map(params![format!("{escaped} #%")], |row| {
                row.get::<_, String>(0)
            })
            .map_err(|e| AgentError::session(e.to_string()))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| AgentError::session(e.to_string()))?;
        if let Some(id) = numbered.into_iter().next() {
            return Ok(Some(id));
        }
        let mut stmt = conn
            .prepare(
                "SELECT id FROM sessions WHERE title = ?1 AND archived = 0
                 ORDER BY started_at DESC",
            )
            .map_err(|e| AgentError::session(e.to_string()))?;
        let mut rows = stmt
            .query(params![title])
            .map_err(|e| AgentError::session(e.to_string()))?;
        match rows
            .next()
            .map_err(|e| AgentError::session(e.to_string()))?
        {
            Some(row) => row
                .get::<_, String>(0)
                .map(Some)
                .map_err(|e| AgentError::session(e.to_string())),
            None => Ok(None),
        }
    }

    /// Follow the compression chain forward to the live tip (hermes
    /// `get_compression_tip`): while a session ended with reason
    /// `compression` and has a continuation child, move to the newest
    /// child. Returns the tip id, or `None` when `session_id` is unknown.
    pub fn compression_tip(&self, session_id: &str) -> Result<Option<String>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        let mut current = session_id.to_string();
        for _ in 0..1000 {
            let (end_reason, exists): (Option<String>, bool) = conn
                .query_row(
                    "SELECT end_reason, 1 FROM sessions WHERE id = ?1",
                    params![current],
                    |row| Ok((row.get::<_, Option<String>>(0)?, true)),
                )
                .optional()
                .map_err(|e| AgentError::session(e.to_string()))?
                .unwrap_or((None, false));
            if !exists {
                return Ok(None);
            }
            if end_reason.as_deref() != Some("compression") {
                return Ok(Some(current));
            }
            let child: Option<String> = conn
                .query_row(
                    "SELECT id FROM sessions WHERE parent_session_id = ?1
                     ORDER BY started_at DESC LIMIT 1",
                    params![current],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(|e| AgentError::session(e.to_string()))?;
            match child {
                Some(id) => current = id,
                None => return Ok(Some(current)),
            }
        }
        Ok(Some(current))
    }

    /// Atomic "set the title only if it is still empty" (hermes
    /// `set_auto_title_if_empty`): predicate + write in one statement, so a
    /// manual title set while auto-generation was in flight is never
    /// overwritten. Returns true when the title was written.
    pub fn set_auto_title_if_empty(&self, session_id: &str, title: &str) -> Result<bool> {
        if title.contains(['\r', '\n', '\0']) {
            return Err(AgentError::session("invalid session title"));
        }
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        let changed = conn
            .execute(
                "UPDATE sessions SET title = ?2 WHERE id = ?1 AND (title IS NULL OR title = '')",
                params![session_id, title],
            )
            .map_err(|e| AgentError::session(e.to_string()))?;
        Ok(changed > 0)
    }

    /// Lock a session to a specific model (gateway model-lock API). The
    /// locked model is inherited by forks (via the `model` column) and
    /// survives `ensure_session` resumes.
    pub fn set_session_model(&self, session_id: &str, model: &str) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        conn.execute(
            "UPDATE sessions SET model = ?2 WHERE id = ?1",
            params![session_id, model],
        )
        .map_err(|e| AgentError::session(e.to_string()))?;
        Ok(())
    }

    /// Set/get state metadata.
    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        conn.execute(
            "INSERT INTO state_meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )
        .map_err(|e| AgentError::session(e.to_string()))?;
        Ok(())
    }

    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        conn.query_row(
            "SELECT value FROM state_meta WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| AgentError::session(e.to_string()))
    }

    // --- prune / archive (hermes sessions prune) -------------------------

    /// Sessions a matching `prune_sessions` / `archive_sessions` call would
    /// touch, oldest activity first (hermes `list_prune_candidates`).
    pub fn list_prune_candidates(
        &self,
        filters: &crate::session::filters::PruneFilters,
    ) -> Result<Vec<PruneCandidate>> {
        let (where_clause, params) = filters.where_clause();
        let sql = format!(
            "SELECT s.id, s.source, s.title, s.model, s.started_at,
                    {} AS last_active,
                    s.message_count, s.archived
             FROM sessions s WHERE {}
             ORDER BY last_active ASC, s.started_at ASC",
            crate::session::filters::LAST_ACTIVE_EXPR,
            where_clause
        );
        let values = filter_params_to_values(&params);
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| AgentError::session(e.to_string()))?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(values), |row| {
                Ok(PruneCandidate {
                    id: row.get(0)?,
                    source: row.get(1)?,
                    title: row.get(2)?,
                    model: row.get(3)?,
                    started_at: row.get(4)?,
                    last_active: row.get(5)?,
                    message_count: row.get(6)?,
                    archived: row.get::<_, i64>(7)? != 0,
                })
            })
            .map_err(|e| AgentError::session(e.to_string()))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| AgentError::session(e.to_string()))
    }

    /// Delete every session matching the filters (messages + FTS rows
    /// first). Only ENDED sessions are ever candidates. Returns the number
    /// of sessions deleted (hermes `prune_sessions`).
    pub fn prune_sessions(&self, filters: &crate::session::filters::PruneFilters) -> Result<usize> {
        let candidates = self.list_prune_candidates(filters)?;
        for candidate in &candidates {
            self.delete_session(&candidate.id)?;
        }
        Ok(candidates.len())
    }

    /// Soft-hide every session matching the filters by flipping
    /// `archived = 1` — nothing is deleted; repeat runs are idempotent
    /// (hermes `archive_sessions`).
    pub fn archive_sessions(
        &self,
        filters: &crate::session::filters::PruneFilters,
    ) -> Result<usize> {
        let mut filters = filters.clone();
        if filters.archived.is_none() {
            filters.archived = Some(false); // only not-yet-archived rows
        }
        let candidates = self.list_prune_candidates(&filters)?;
        for candidate in &candidates {
            self.set_session_archived(&candidate.id, true)?;
        }
        Ok(candidates.len())
    }

    /// Flip the archived flag on one session (hermes `set_session_archived`).
    pub fn set_session_archived(&self, session_id: &str, archived: bool) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        conn.execute(
            "UPDATE sessions SET archived = ?2 WHERE id = ?1",
            params![session_id, if archived { 1 } else { 0 }],
        )
        .map_err(|e| AgentError::session(e.to_string()))?;
        Ok(())
    }

    /// Session counts grouped by source, most numerous first (hermes
    /// `sessions stats`).
    pub fn session_count_by_source(&self) -> Result<Vec<(String, i64)>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        let mut stmt = conn
            .prepare("SELECT source, COUNT(*) FROM sessions GROUP BY source ORDER BY COUNT(*) DESC")
            .map_err(|e| AgentError::session(e.to_string()))?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .map_err(|e| AgentError::session(e.to_string()))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| AgentError::session(e.to_string()))
    }
}

/// One prune/archive candidate row (hermes `list_prune_candidates` shape).
#[derive(Debug, Clone)]
pub struct PruneCandidate {
    pub id: String,
    pub source: String,
    pub title: Option<String>,
    pub model: Option<String>,
    pub started_at: f64,
    /// Latest message timestamp, falling back to `started_at`.
    pub last_active: f64,
    pub message_count: i64,
    pub archived: bool,
}

fn filter_params_to_values(
    params: &[crate::session::filters::FilterParam],
) -> Vec<rusqlite::types::Value> {
    params
        .iter()
        .map(|param| match param {
            crate::session::filters::FilterParam::Real(v) => rusqlite::types::Value::Real(*v),
            crate::session::filters::FilterParam::Int(v) => rusqlite::types::Value::Integer(*v),
            crate::session::filters::FilterParam::Text(s) => {
                rusqlite::types::Value::Text(s.clone())
            }
        })
        .collect()
}

impl SessionStore for SqliteSessionStore {
    fn save_session(&self, session: &Session) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        conn.execute(
            "INSERT INTO sessions (id, source, model, parent_session_id, started_at, last_activity_at, title)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET
                last_activity_at = excluded.last_activity_at,
                title = COALESCE(excluded.title, sessions.title)",
            params![
                session.id,
                session.metadata.platform.as_deref().unwrap_or("cli"),
                session.metadata.model,
                session.parent_id,
                session.created_at_ms as f64 / 1000.0,
                session.updated_at_ms as f64 / 1000.0,
                serde_json::to_value(&session.metadata)
                    .ok()
                    .and_then(|v| v.get("title").and_then(|t| t.as_str().map(String::from))),
            ],
        )
        .map_err(|e| AgentError::session(format!("save session: {}", e)))?;
        drop(conn);
        // Persist messages that are not stored yet (naive full rewrite for the
        // generic SessionStore API; append_message is the fast path).
        let existing = self.load_messages(&session.id)?.len();
        for message in session.messages.iter().skip(existing) {
            self.append_message(&session.id, message)?;
        }
        Ok(())
    }

    fn load_session(&self, session_id: &str) -> Result<Option<Session>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        let row = conn
            .query_row(
                "SELECT id, source, model, parent_session_id, started_at, last_activity_at
                 FROM sessions WHERE id = ?1",
                params![session_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, f64>(4)?,
                        row.get::<_, Option<f64>>(5)?,
                    ))
                },
            )
            .optional()
            .map_err(|e| AgentError::session(e.to_string()))?;
        drop(conn);
        let Some((id, source, model, parent, started, activity)) = row else {
            return Ok(None);
        };
        let messages = self.load_messages(session_id)?;
        Ok(Some(Session {
            id,
            conversation_id: session_id.to_string(),
            messages,
            created_at_ms: (started * 1000.0) as i64,
            updated_at_ms: (activity.unwrap_or(started) * 1000.0) as i64,
            parent_id: parent,
            metadata: SessionMetadata {
                platform: Some(source),
                model,
                ..Default::default()
            },
        }))
    }

    fn list_sessions(&self, limit: usize) -> Result<Vec<Session>> {
        let mut ids: Vec<String> = Vec::new();
        {
            let conn = self
                .conn
                .lock()
                .map_err(|e| AgentError::session(e.to_string()))?;
            let mut stmt = conn
                .prepare("SELECT id FROM sessions ORDER BY last_activity_at DESC LIMIT ?1")
                .map_err(|e| AgentError::session(e.to_string()))?;
            let rows = stmt
                .query_map(params![limit as i64], |row| row.get::<_, String>(0))
                .map_err(|e| AgentError::session(e.to_string()))?;
            for row in rows {
                if let Ok(id) = row {
                    ids.push(id);
                }
            }
        }
        let mut sessions = Vec::new();
        for id in ids {
            if let Some(session) = self.load_session(&id)? {
                sessions.push(session);
            }
        }
        Ok(sessions)
    }

    fn delete_session(&self, session_id: &str) -> Result<()> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| AgentError::session(e.to_string()))?;
        if self.has_fts {
            conn.execute(
                "DELETE FROM messages_fts WHERE rowid IN
                 (SELECT id FROM messages WHERE session_id = ?1)",
                params![session_id],
            )
            .ok();
        }
        conn.execute(
            "DELETE FROM messages WHERE session_id = ?1",
            params![session_id],
        )
        .ok();
        conn.execute("DELETE FROM sessions WHERE id = ?1", params![session_id])
            .map_err(|e| AgentError::session(e.to_string()))?;
        Ok(())
    }

    fn search_sessions(&self, query: &str, limit: usize) -> Result<Vec<Session>> {
        let hits = self.search_messages(query, limit * 3)?;
        let mut seen = std::collections::HashSet::new();
        let mut sessions = Vec::new();
        for (session_id, _) in hits {
            if seen.insert(session_id.clone()) {
                if let Some(session) = self.load_session(&session_id)? {
                    sessions.push(session);
                    if sessions.len() >= limit {
                        break;
                    }
                }
            }
        }
        Ok(sessions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with(dir: &std::path::Path) -> SqliteSessionStore {
        SqliteSessionStore::open(dir.join("state.db")).unwrap()
    }

    fn user_msg(text: &str) -> Message {
        Message {
            reasoning: None,
            role: Role::User,
            content: Some(text.into()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }

    #[test]
    fn latest_message_row_id_role_offset_and_text_filter() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with(dir.path());
        let sid = store.create_session("cli", Some("m"), None).unwrap();
        store.append_message(&sid, &user_msg("first")).unwrap();
        store
            .append_message(
                &sid,
                &Message {
                    reasoning: None,
                    role: Role::Assistant,
                    content: Some("reply".into()),
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                },
            )
            .unwrap();
        store.append_message(&sid, &user_msg("second")).unwrap();
        // Tool-call-only assistant row: no text, must be skipped.
        store
            .append_message(
                &sid,
                &Message {
                    reasoning: None,
                    role: Role::Assistant,
                    content: None,
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                },
            )
            .unwrap();

        let latest_user = store.latest_message_row_id(&sid, "user", 0, true).unwrap();
        let previous_user = store.latest_message_row_id(&sid, "user", 1, true).unwrap();
        assert!(latest_user > previous_user);
        assert!(store.latest_message_row_id(&sid, "user", 2, true).is_none());
        // The latest assistant row with text is the "reply" row.
        let assistant = store
            .latest_message_row_id(&sid, "assistant", 0, true)
            .unwrap();
        assert_eq!(
            store.message_role(&sid, assistant).as_deref(),
            Some("assistant")
        );
        assert_eq!(
            store.message_role(&sid, latest_user).as_deref(),
            Some("user")
        );
        // Bad inputs.
        assert!(store.latest_message_row_id(&sid, "tool", 0, true).is_none());
        assert!(store
            .latest_message_row_id(&sid, "user", -1, true)
            .is_none());
        assert!(store.latest_message_row_id("", "user", 0, true).is_none());
    }

    #[test]
    fn reactions_tapback_semantics() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with(dir.path());
        let sid = store.create_session("cli", Some("m"), None).unwrap();
        store.append_message(&sid, &user_msg("hello")).unwrap();
        let row = store.latest_message_row_id(&sid, "user", 0, true).unwrap();

        // Agent sets a reaction.
        let reactions = store
            .set_message_reaction(&sid, row, Some("👍"), "agent")
            .unwrap();
        assert_eq!(reactions.len(), 1);
        assert_eq!(reactions[0]["emoji"], "👍");
        assert_eq!(reactions[0]["author"], "agent");

        // User adds theirs — both coexist (one per author).
        let reactions = store
            .set_message_reaction(&sid, row, Some("❤️"), "user")
            .unwrap();
        assert_eq!(reactions.len(), 2);

        // Same emoji again retracts (tapback toggle).
        let reactions = store
            .set_message_reaction(&sid, row, Some("👍"), "agent")
            .unwrap();
        assert_eq!(reactions.len(), 1);
        assert_eq!(reactions[0]["author"], "user");

        // A different emoji replaces.
        let reactions = store
            .set_message_reaction(&sid, row, Some("😂"), "agent")
            .unwrap();
        assert_eq!(reactions.len(), 2);
        let agent = reactions.iter().find(|r| r["author"] == "agent").unwrap();
        assert_eq!(agent["emoji"], "😂");

        // Empty emoji retracts.
        let reactions = store
            .set_message_reaction(&sid, row, Some(""), "agent")
            .unwrap();
        assert_eq!(reactions.len(), 1);
        // None clears explicitly.
        let reactions = store.set_message_reaction(&sid, row, None, "user").unwrap();
        assert!(reactions.is_empty());
        assert!(store.get_message_reactions(&sid, row).is_empty());

        // Row outside the session → None.
        assert!(store
            .set_message_reaction("other-session", row, Some("👍"), "agent")
            .is_none());
        assert!(store
            .set_message_reaction(&sid, row + 999, Some("👍"), "agent")
            .is_none());
        assert!(store.message_role(&sid, row + 999).is_none());
    }

    /// P3-1: assistant 的 reasoning 落库并在两条读回路径上都还原。
    #[test]
    fn reasoning_persists_and_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let sid = {
            let store = SqliteSessionStore::open(&path).unwrap();
            let sid = store.create_session("cli", Some("m"), None).unwrap();
            store.append_message(&sid, &user_msg("think hard")).unwrap();
            store
                .append_message(
                    &sid,
                    &Message {
                        reasoning: Some("step 1 -> step 2".to_string()),
                        role: Role::Assistant,
                        content: Some("done".into()),
                        tool_calls: None,
                        tool_call_id: None,
                        name: None,
                    },
                )
                .unwrap();
            sid
        };

        // 重开（走真实读回路径，不是内存残留）
        let store = SqliteSessionStore::open(&path).unwrap();
        let msgs = store.load_messages(&sid).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].reasoning, None, "user turn carries no reasoning");
        assert_eq!(
            msgs[1].reasoning.as_deref(),
            Some("step 1 -> step 2"),
            "assistant reasoning must survive a reopen"
        );

        // 带时间戳的读回路径（导出/recap 用）同样要还原
        let with_ts = store.load_messages_with_timestamps(&sid).unwrap();
        assert_eq!(with_ts.len(), 2);
        assert_eq!(with_ts[1].1.reasoning.as_deref(), Some("step 1 -> step 2"));
    }

    /// P3-1: 老库（messages 表无 reasoning 列）打开时幂等补列，且不丢既有行。
    /// 这是升级路径——已装用户的 state.db 必须无损迁移。
    ///
    /// 造"旧库"的方式：先用真实 store 建库（拿到**当前完整 schema**，避免手写
    /// 旧 schema 与真实结构漂移导致测试因无关原因失败），再 DROP reasoning 列
    /// 模拟升级前的库，然后重开验证迁移补列 + 旧行无损。
    #[test]
    fn reasoning_column_migrates_from_pre_existing_db() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");

        // 1) 用真实 store 建库并塞一行（此时 schema 已含 reasoning 列）
        let sid = {
            let store = SqliteSessionStore::open(&path).unwrap();
            let sid = store.create_session("cli", Some("m"), None).unwrap();
            store.append_message(&sid, &user_msg("kept")).unwrap();
            sid
        };

        // 2) 模拟"升级前的旧库"：删掉 reasoning 列（SQLite 3.35+ 支持 DROP COLUMN；
        //    reasoning 是普通可空 TEXT、无索引/约束/FTS 依赖，可安全删除）
        {
            use rusqlite::Connection;
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("ALTER TABLE messages DROP COLUMN reasoning")
                .expect("DROP COLUMN reasoning (simulate pre-migration DB)");
            // 确认前提：此刻确实没有 reasoning 列
            let cols: Vec<String> = {
                let mut st = conn.prepare("PRAGMA table_info(messages)").unwrap();
                st.query_map([], |r| r.get::<_, String>(1))
                    .unwrap()
                    .filter_map(|r| r.ok())
                    .collect()
            };
            assert!(
                !cols.iter().any(|c| c == "reasoning"),
                "fixture must lack the column after DROP"
            );
        }

        // 3) 用新版 store 打开 -> 迁移应补列并保留旧行
        let store = SqliteSessionStore::open(&path).unwrap();
        let msgs = store.load_messages(&sid).unwrap();
        assert_eq!(msgs.len(), 1, "pre-existing row must survive migration");
        assert_eq!(msgs[0].content.as_deref(), Some("kept"));
        assert_eq!(msgs[0].reasoning, None, "old rows have no reasoning");

        // 4) 迁移后写入/读回 reasoning 正常
        store
            .append_message(
                &sid,
                &Message {
                    reasoning: Some("post-migration".into()),
                    role: Role::Assistant,
                    content: Some("new".into()),
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                },
            )
            .unwrap();
        let msgs = store.load_messages(&sid).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1].reasoning.as_deref(), Some("post-migration"));

        // 5) 幂等：再开一次不报错、不重复加列
        let store2 = SqliteSessionStore::open(&path).unwrap();
        assert_eq!(store2.load_messages(&sid).unwrap().len(), 2);
    }

    #[test]
    fn reactions_survive_reopen_and_migration() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let sid = {
            let store = SqliteSessionStore::open(&path).unwrap();
            let sid = store.create_session("cli", Some("m"), None).unwrap();
            store.append_message(&sid, &user_msg("persist me")).unwrap();
            let row = store.latest_message_row_id(&sid, "user", 0, true).unwrap();
            store
                .set_message_reaction(&sid, row, Some("🎉"), "agent")
                .unwrap();
            sid
        };
        let store = SqliteSessionStore::open(&path).unwrap();
        let row = store.latest_message_row_id(&sid, "user", 0, true).unwrap();
        let reactions = store.get_message_reactions(&sid, row);
        assert_eq!(reactions.len(), 1);
        assert_eq!(reactions[0]["emoji"], "🎉");
    }

    #[test]
    fn test_sqlite_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open(dir.path().join("state.db")).unwrap();
        let sid = store
            .create_session("cli", Some("test-model"), Some("/tmp"))
            .unwrap();
        store
            .append_message(
                &sid,
                &Message {
                    reasoning: None,
                    role: Role::User,
                    content: Some("hello world from ulnclaw".into()),
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                },
            )
            .unwrap();
        let messages = store.load_messages(&sid).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0].content.as_deref(),
            Some("hello world from ulnclaw")
        );

        let results = store.search_messages("ulnclaw", 5).unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0].0, sid);

        let session = store.load_session(&sid).unwrap().unwrap();
        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.metadata.model.as_deref(), Some("test-model"));
    }

    #[test]
    fn test_lineage() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open(dir.path().join("state.db")).unwrap();
        let parent = store.create_session("cli", None, None).unwrap();
        let child = store
            .create_child_session(&parent, "delegate", None)
            .unwrap();
        let child_session = store.load_session(&child).unwrap().unwrap();
        assert_eq!(child_session.parent_id.as_deref(), Some(parent.as_str()));
    }

    #[test]
    fn resolve_by_title_prefers_latest_numbered_variant() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open(dir.path().join("state.db")).unwrap();
        let base = store.create_session("cli", None, None).unwrap();
        store.set_session_title(&base, "digest").unwrap();
        // Simulate an older start for the base row.
        {
            let conn = store.conn.lock().unwrap();
            conn.execute(
                "UPDATE sessions SET started_at = ?2 WHERE id = ?1",
                params![base, now() - 100.0],
            )
            .unwrap();
        }
        let v2 = store.create_session("cli", None, None).unwrap();
        store.set_session_title(&v2, "digest #2").unwrap();
        let v3 = store.create_session("cli", None, None).unwrap();
        store.set_session_title(&v3, "digest #3").unwrap();
        {
            let conn = store.conn.lock().unwrap();
            conn.execute(
                "UPDATE sessions SET started_at = ?2 WHERE id = ?1",
                params![v3, now() - 50.0],
            )
            .unwrap();
        }
        // Newest numbered variant wins over the exact title.
        assert_eq!(
            store.resolve_session_by_title("digest").unwrap().as_deref(),
            Some(v2.as_str())
        );
        // Exact-title fallback when no numbered variants exist.
        let lone = store.create_session("cli", None, None).unwrap();
        store.set_session_title(&lone, "standalone").unwrap();
        assert_eq!(
            store
                .resolve_session_by_title("standalone")
                .unwrap()
                .as_deref(),
            Some(lone.as_str())
        );
        assert_eq!(store.resolve_session_by_title("nope").unwrap(), None);
    }

    #[test]
    fn resolve_by_title_skips_archived_and_escapes_wildcards() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open(dir.path().join("state.db")).unwrap();
        let sid = store.create_session("cli", None, None).unwrap();
        store.set_session_title(&sid, "wild%card_x").unwrap();
        assert_eq!(
            store
                .resolve_session_by_title("wild%card_x")
                .unwrap()
                .as_deref(),
            Some(sid.as_str())
        );
        // The unescaped pattern must not match a different literal title.
        let other = store.create_session("cli", None, None).unwrap();
        store.set_session_title(&other, "wildXcard y").unwrap();
        assert_eq!(store.resolve_session_by_title("wild%card y").unwrap(), None);
        // Archived sessions are not resolvable by title.
        store.set_session_archived(&sid, true).unwrap();
        assert_eq!(store.resolve_session_by_title("wild%card_x").unwrap(), None);
    }

    #[test]
    fn compression_tip_walks_chain_to_live_session() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteSessionStore::open(dir.path().join("state.db")).unwrap();
        let root = store.create_session("cli", None, None).unwrap();
        store.end_session(&root, "compression").unwrap();
        let mid = store.create_child_session(&root, "cli", None).unwrap();
        store.end_session(&mid, "compression").unwrap();
        let tip = store.create_child_session(&mid, "cli", None).unwrap();
        // The tip is live (no end_reason) — the chain projects to it.
        assert_eq!(
            store.compression_tip(&root).unwrap().as_deref(),
            Some(tip.as_str())
        );
        assert_eq!(
            store.compression_tip(&mid).unwrap().as_deref(),
            Some(tip.as_str())
        );
        assert_eq!(
            store.compression_tip(&tip).unwrap().as_deref(),
            Some(tip.as_str())
        );
        // Unknown session resolves to None.
        assert_eq!(store.compression_tip("missing").unwrap(), None);
        // A compressed session without children stays where it is.
        let orphan = store.create_session("cli", None, None).unwrap();
        store.end_session(&orphan, "compression").unwrap();
        assert_eq!(
            store.compression_tip(&orphan).unwrap().as_deref(),
            Some(orphan.as_str())
        );
    }

    // --- prune / archive -------------------------------------------------

    use crate::session::filters::PruneFilters;

    fn now() -> f64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64()
    }

    #[test]
    fn prune_candidates_only_ended_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with(dir.path());
        let ended_cli = store.create_session("cli", Some("model-a"), None).unwrap();
        store
            .append_message(&ended_cli, &user_msg("hello"))
            .unwrap();
        store.end_session(&ended_cli, "ended").unwrap();
        let ended_cron = store.create_session("cron", Some("model-a"), None).unwrap();
        store.end_session(&ended_cron, "ended").unwrap();
        let live = store.create_session("cli", Some("model-b"), None).unwrap();
        store.append_message(&live, &user_msg("busy")).unwrap();

        // Source filter + ended-only policy: the live cli session is never
        // a candidate.
        let filters = PruneFilters {
            source: Some("cli".into()),
            ..Default::default()
        };
        let candidates = store.list_prune_candidates(&filters).unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].id, ended_cli);
        assert_eq!(candidates[0].source, "cli");
        assert!(!candidates[0].archived);

        // No filters → both ended sessions, oldest activity first.
        let all = store
            .list_prune_candidates(&PruneFilters::default())
            .unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn archive_is_soft_hide_and_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with(dir.path());
        let ended = store.create_session("cli", Some("model-a"), None).unwrap();
        store.append_message(&ended, &user_msg("hello")).unwrap();
        store.end_session(&ended, "ended").unwrap();

        let filters = PruneFilters {
            source: Some("cli".into()),
            ..Default::default()
        };
        assert_eq!(store.archive_sessions(&filters).unwrap(), 1);
        // Idempotent — already-archived rows are skipped.
        assert_eq!(store.archive_sessions(&filters).unwrap(), 0);
        // Messages survive; the row is just hidden.
        assert!(store.load_session(&ended).unwrap().is_some());

        // Prune with the CLI default (archived=false) skips archived rows…
        let mut unarchived_only = filters.clone();
        unarchived_only.archived = Some(false);
        assert_eq!(store.prune_sessions(&unarchived_only).unwrap(), 0);
        // …until archived=None (both), the --include-archived equivalent.
        assert_eq!(store.prune_sessions(&filters).unwrap(), 1);
        assert!(store.load_session(&ended).unwrap().is_none());
    }

    #[test]
    fn prune_time_title_and_message_filters() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with(dir.path());
        let titled = store.create_session("cli", Some("gpt-x"), None).unwrap();
        store.append_message(&titled, &user_msg("one")).unwrap();
        store.append_message(&titled, &user_msg("two")).unwrap();
        store.set_session_title(&titled, "Fix the parser").unwrap();
        store.end_session(&titled, "ended").unwrap();
        let untitled = store.create_session("cli", Some("gpt-y"), None).unwrap();
        store.end_session(&untitled, "compression").unwrap();

        // Last-active window: future bound matches, past bound doesn't.
        let mut filters = PruneFilters::default();
        filters.last_active_before = Some(now() + 60.0);
        assert_eq!(store.list_prune_candidates(&filters).unwrap().len(), 2);
        filters.last_active_before = Some(now() - 60.0);
        assert_eq!(store.list_prune_candidates(&filters).unwrap().len(), 0);

        // Title substring (case-insensitive).
        let mut filters = PruneFilters::default();
        filters.title_like = Some("PARSER".into());
        let candidates = store.list_prune_candidates(&filters).unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].id, titled);

        // Model substring + end_reason exact.
        let mut filters = PruneFilters::default();
        filters.model_like = Some("gpt".into());
        assert_eq!(store.list_prune_candidates(&filters).unwrap().len(), 2);
        let mut filters = PruneFilters::default();
        filters.end_reason = Some("compression".into());
        let candidates = store.list_prune_candidates(&filters).unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].id, untitled);

        // Message-count bounds.
        let mut filters = PruneFilters::default();
        filters.min_messages = Some(2);
        assert_eq!(store.list_prune_candidates(&filters).unwrap().len(), 1);
        let mut filters = PruneFilters::default();
        filters.max_messages = Some(0);
        assert_eq!(store.list_prune_candidates(&filters).unwrap().len(), 1);
    }

    #[test]
    fn session_count_by_source_groups() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with(dir.path());
        store.create_session("cli", None, None).unwrap();
        store.create_session("cli", None, None).unwrap();
        store.create_session("cron", None, None).unwrap();
        let counts = store.session_count_by_source().unwrap();
        assert_eq!(counts[0], ("cli".to_string(), 2));
        assert_eq!(counts[1], ("cron".to_string(), 1));
    }

    #[test]
    fn skill_scaffolded_listing_and_retitle_helpers() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with(dir.path());

        // Session opened via a /skill scaffold, already (mis)titled.
        let scaffolded = store.create_session("cli", None, None).unwrap();
        store
            .append_message(
                &scaffolded,
                &user_msg(&format!(
                    "{}\"work\" skill bundle, loading 1 skills together.\nUser instruction: fix the leak\n\n[Loaded as part of the \"work\" skill bundle.]",
                    crate::session::retitle::SKILL_INVOCATION_PREFIX
                )),
            )
            .unwrap();
        store
            .append_message(
                &scaffolded,
                &Message {
                    reasoning: None,
                    role: Role::Assistant,
                    content: Some("done, leak fixed".into()),
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                },
            )
            .unwrap();
        store
            .set_session_title(&scaffolded, "Work skill bundle helper")
            .unwrap();

        // Plain session — must not be listed.
        let plain = store.create_session("cli", None, None).unwrap();
        store
            .append_message(&plain, &user_msg("ordinary question"))
            .unwrap();
        store.set_session_title(&plain, "Ordinary").unwrap();
        // Scaffolded but untitled — skipped (nothing to repair).
        let untitled = store.create_session("cli", None, None).unwrap();
        store
            .append_message(
                &untitled,
                &user_msg(&format!(
                    "{}\"x\" skill bundle,",
                    crate::session::retitle::SKILL_INVOCATION_PREFIX
                )),
            )
            .unwrap();

        let rows = store.list_skill_scaffolded_sessions(200).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, scaffolded);
        assert_eq!(rows[0].title.as_deref(), Some("Work skill bundle helper"));

        assert_eq!(
            store.get_first_assistant_text(&scaffolded).unwrap(),
            "done, leak fixed"
        );
        assert_eq!(store.get_first_assistant_text(&plain).unwrap(), "");

        // Title lineage dedup.
        assert_eq!(
            store.get_next_title_in_lineage("Fresh name").unwrap(),
            "Fresh name",
            "free base returns itself"
        );
        assert_eq!(
            store.get_next_title_in_lineage("Ordinary").unwrap(),
            "Ordinary #2"
        );
        // Take #2 via the untitled session, then ask again.
        store.set_session_title(&untitled, "Ordinary #2").unwrap();
        assert_eq!(
            store.get_next_title_in_lineage("Ordinary").unwrap(),
            "Ordinary #3"
        );
        assert_eq!(
            store.get_next_title_in_lineage("Ordinary #2").unwrap(),
            "Ordinary #3",
            "existing suffix stripped before numbering"
        );
    }

    #[test]
    fn browse_rows_order_filter_and_preview() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with(dir.path());
        let first = store.create_session("cli", None, None).unwrap();
        store
            .append_message(&first, &user_msg("first user line"))
            .unwrap();
        store
            .append_message(
                &first,
                &Message {
                    reasoning: None,
                    role: Role::Assistant,
                    content: Some("reply".into()),
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                },
            )
            .unwrap();
        let second = store.create_session("cron", None, None).unwrap();
        store
            .append_message(&second, &user_msg("cron line"))
            .unwrap();
        let tool_session = store.create_session("tool", None, None).unwrap();

        // Default browse excludes nothing on the store side (CLI passes the
        // exclusion list); newest first, preview = first user message.
        let rows = store
            .list_sessions_for_browse(100, None, &[], false)
            .unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].id, tool_session); // newest, no messages
        assert_eq!(rows[0].preview, None);
        let cron_row = rows.iter().find(|r| r.id == second).unwrap();
        assert_eq!(cron_row.preview.as_deref(), Some("cron line"));
        // P524: stored message counts ride along for the details pane.
        assert_eq!(cron_row.message_count, 1);

        // Excluding tool sources (hermes default browse behavior).
        let rows = store
            .list_sessions_for_browse(100, None, &["tool"], false)
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.source != "tool"));

        // Source filter.
        let rows = store
            .list_sessions_for_browse(100, Some("cron"), &[], false)
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, second);

        // Archived sessions disappear by default…
        store.set_session_archived(&first, true).unwrap();
        let rows = store
            .list_sessions_for_browse(100, Some("cli"), &[], false)
            .unwrap();
        assert!(rows.is_empty());
        // …but surface (flagged) when include_archived is set (P513).
        let rows = store
            .list_sessions_for_browse(100, Some("cli"), &[], true)
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].archived);

        // The session cwd rides along for project resolution (P165).
        let with_cwd = store
            .create_session("cli", None, Some("/work/repo"))
            .unwrap();
        let rows = store
            .list_sessions_for_browse(100, Some("cli"), &[], false)
            .unwrap();
        let row = rows.iter().find(|r| r.id == with_cwd).unwrap();
        assert_eq!(row.cwd.as_deref(), Some("/work/repo"));
    }

    #[test]
    fn latest_session_id_prefers_last_activity() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with(dir.path());
        assert_eq!(store.latest_session_id().unwrap(), None);
        let first = store.create_session("cli", None, None).unwrap();
        let second = store.create_session("cli", None, None).unwrap();
        // Newest by started_at wins when no activity is recorded.
        assert_eq!(
            store.latest_session_id().unwrap().as_deref(),
            Some(second.as_str())
        );
        // Activity on the older session promotes it.
        let conn = store.conn.lock().unwrap();
        conn.execute(
            "UPDATE sessions SET last_activity_at = ?2 WHERE id = ?1",
            params![first, now_secs() + 10.0],
        )
        .unwrap();
        drop(conn);
        assert_eq!(
            store.latest_session_id().unwrap().as_deref(),
            Some(first.as_str())
        );
        // Archived sessions are skipped.
        store.set_session_archived(&first, true).unwrap();
        assert_eq!(
            store.latest_session_id().unwrap().as_deref(),
            Some(second.as_str())
        );
    }

    #[test]
    fn sanitize_title_cleans_collapses_and_validates() {
        // Control chars dropped, whitespace collapsed, trimmed.
        assert_eq!(
            sanitize_title("  hello\u{0007} \t  world  ").unwrap(),
            Some("hello world".to_string())
        );
        // Zero-width and bidi overrides dropped.
        assert_eq!(
            sanitize_title("ab\u{200B}cd\u{202E}e\u{FEFF}").unwrap(),
            Some("abcde".to_string())
        );
        // Newlines collapse to a single space.
        assert_eq!(
            sanitize_title("line1\n\n  line2").unwrap(),
            Some("line1 line2".to_string())
        );
        // Empty / whitespace-only / invisible-only all normalize to None.
        assert_eq!(sanitize_title("").unwrap(), None);
        assert_eq!(sanitize_title("   \t  ").unwrap(), None);
        assert_eq!(sanitize_title("\u{200B}\u{FEFF}").unwrap(), None);
        // Length limit enforced on cleaned char count.
        let long = "x".repeat(MAX_TITLE_LENGTH + 1);
        let err = sanitize_title(&long).unwrap_err();
        assert!(err.contains("Title too long"), "got: {err}");
        assert_eq!(
            sanitize_title(&"x".repeat(MAX_TITLE_LENGTH))
                .unwrap()
                .unwrap()
                .len(),
            MAX_TITLE_LENGTH
        );
    }

    #[test]
    fn set_session_title_rename_uniqueness_and_clear() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with(dir.path());
        let first = store.create_session("cli", None, None).unwrap();
        let second = store.create_session("cli", None, None).unwrap();

        // Basic rename round-trip (sanitization applied).
        store
            .set_session_title(&first, "  Fix   the\tparser ")
            .unwrap();
        assert_eq!(
            store.get_session_title(&first).unwrap().as_deref(),
            Some("Fix the parser")
        );

        // Same session may keep its own title.
        store.set_session_title(&first, "Fix the parser").unwrap();

        // A different session cannot take the same title.
        let err = store
            .set_session_title(&second, "Fix the parser")
            .unwrap_err();
        assert!(err.to_string().contains("already in use"), "got: {err}");

        // Whitespace-only clears the title.
        store.set_session_title(&first, "   ").unwrap();
        assert_eq!(store.get_session_title(&first).unwrap(), None);

        // Unknown session id errors instead of silently succeeding.
        assert!(store.set_session_title("missing", "t").is_err());
    }

    #[test]
    fn resolve_session_id_exact_prefix_and_ambiguous() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with(dir.path());
        {
            let conn = store.conn.lock().unwrap();
            for (id, started) in [("aaa111", 1.0), ("aaa222", 2.0), ("bbb111", 3.0)] {
                conn.execute(
                    "INSERT INTO sessions (id, source, started_at) VALUES (?1, 'cli', ?2)",
                    params![id, started],
                )
                .unwrap();
            }
        }
        // Exact id wins.
        assert_eq!(
            store.resolve_session_id("aaa111").unwrap().as_deref(),
            Some("aaa111")
        );
        // Unique prefix resolves (LIKE wildcards in input are escaped).
        assert_eq!(
            store.resolve_session_id("bbb").unwrap().as_deref(),
            Some("bbb111")
        );
        // Ambiguous prefix -> None.
        assert_eq!(store.resolve_session_id("aaa").unwrap(), None);
        // No match -> None.
        assert_eq!(store.resolve_session_id("zzz").unwrap(), None);
        assert_eq!(store.resolve_session_id("a%1").unwrap(), None);
    }

    #[test]
    fn delete_session_removes_session_and_messages() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with(dir.path());
        let sid = store.create_session("cli", None, None).unwrap();
        store
            .append_message(&sid, &user_msg("to be deleted"))
            .unwrap();
        assert_eq!(store.count_messages().unwrap(), 1);

        store.delete_session(&sid).unwrap();
        assert!(store.get_session_row(&sid).unwrap().is_none());
        assert_eq!(store.count_messages().unwrap(), 0);
        // Deleting again is a no-op, not an error.
        store.delete_session(&sid).unwrap();
    }

    #[test]
    fn optimize_storage_and_logical_size() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with(dir.path());
        let sid = store.create_session("cli", None, None).unwrap();
        for i in 0..50 {
            store
                .append_message(&sid, &user_msg(&format!("message {i}")))
                .unwrap();
        }
        let merged = store.optimize_storage().unwrap();
        assert_eq!(merged, usize::from(store.has_fts));
        let logical = store.logical_size_bytes().expect("pragmas readable");
        assert!(logical > 0);
        // Store remains fully usable after VACUUM.
        assert_eq!(store.count_messages().unwrap(), 50);
    }

    // ---------------------------------------------------------------
    // P3-kv: sessions.model_config JSON KV (hermes get/patch parity)
    // ---------------------------------------------------------------

    fn obj(pairs: &[(&str, Value)]) -> Map<String, Value> {
        let mut m = Map::new();
        for (k, v) in pairs {
            m.insert((*k).to_string(), v.clone());
        }
        m
    }

    #[test]
    fn model_config_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with(dir.path());
        let sid = store.create_session("cli", Some("m"), None).unwrap();
        assert_eq!(
            store.get_session_model_config_value(&sid, "k").unwrap(),
            None,
            "fresh session has no keys"
        );
        store
            .patch_session_model_config(&sid, &obj(&[("k", Value::from(42))]))
            .unwrap();
        assert_eq!(
            store.get_session_model_config_value(&sid, "k").unwrap(),
            Some(Value::from(42))
        );
    }

    #[test]
    fn model_config_patch_merges_not_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with(dir.path());
        let sid = store.create_session("cli", Some("m"), None).unwrap();
        store
            .patch_session_model_config(&sid, &obj(&[("a", Value::from(1))]))
            .unwrap();
        store
            .patch_session_model_config(&sid, &obj(&[("b", Value::from(2))]))
            .unwrap();
        assert_eq!(
            store.get_session_model_config_value(&sid, "a").unwrap(),
            Some(Value::from(1)),
            "a second patch must not clobber the first key"
        );
        assert_eq!(
            store.get_session_model_config_value(&sid, "b").unwrap(),
            Some(Value::from(2))
        );
    }

    /// hermes: a value of None deletes the key. Callers use this to clear armed
    /// durable state (a cooldown, a prune runway marker).
    #[test]
    fn model_config_null_removes_a_key_but_keeps_siblings() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with(dir.path());
        let sid = store.create_session("cli", Some("m"), None).unwrap();
        store
            .patch_session_model_config(
                &sid,
                &obj(&[("keep", Value::from("x")), ("drop", Value::from("y"))]),
            )
            .unwrap();
        store
            .patch_session_model_config(&sid, &obj(&[("drop", Value::Null)]))
            .unwrap();
        assert_eq!(
            store.get_session_model_config_value(&sid, "drop").unwrap(),
            None,
            "a Null patch value removes the key"
        );
        assert_eq!(
            store.get_session_model_config_value(&sid, "keep").unwrap(),
            Some(Value::from("x")),
            "removing one key must not disturb the others"
        );
    }

    /// Removing the last key leaves the column NULL, not "{}" (hermes serializes
    /// None when the merged dict is empty).
    #[test]
    fn model_config_emptying_writes_null_not_empty_object() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with(dir.path());
        let sid = store.create_session("cli", Some("m"), None).unwrap();
        store
            .patch_session_model_config(&sid, &obj(&[("only", Value::from(1))]))
            .unwrap();
        store
            .patch_session_model_config(&sid, &obj(&[("only", Value::Null)]))
            .unwrap();
        assert_eq!(
            store.get_session_model_config_value(&sid, "only").unwrap(),
            None
        );
        // Re-open and confirm the row still loads cleanly.
        drop(store);
        let store = store_with(dir.path());
        assert_eq!(
            store.get_session_model_config_value(&sid, "only").unwrap(),
            None
        );
    }

    /// hermes: patch/get on a missing row is a no-op, never an error.
    #[test]
    fn model_config_missing_session_is_noop_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with(dir.path());
        store
            .patch_session_model_config("no-such-session", &obj(&[("k", Value::from(1))]))
            .unwrap(); // must not Err
        assert_eq!(
            store
                .get_session_model_config_value("no-such-session", "k")
                .unwrap(),
            None
        );
        // empty session id / empty patch are likewise no-ops
        let sid = store.create_session("cli", Some("m"), None).unwrap();
        store
            .patch_session_model_config("", &obj(&[("k", Value::from(1))]))
            .unwrap();
        store.patch_session_model_config(&sid, &Map::new()).unwrap();
        assert_eq!(
            store.get_session_model_config_value(&sid, "k").unwrap(),
            None
        );
    }

    /// Durability: the whole point of the store is surviving a reopen / --resume.
    #[test]
    fn model_config_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let sid = {
            let store = store_with(dir.path());
            let sid = store.create_session("cli", Some("m"), None).unwrap();
            store
                .patch_session_model_config(
                    &sid,
                    &obj(&[("reasoning_rejected", Value::from(true))]),
                )
                .unwrap();
            sid
        };
        let store = store_with(dir.path());
        assert_eq!(
            store
                .get_session_model_config_value(&sid, "reasoning_rejected")
                .unwrap(),
            Some(Value::from(true)),
            "value must persist across a fresh open"
        );
    }

    /// The migration path: a DB created before the column existed gains it on
    /// open, keeping existing rows (same fixture technique as the reasoning test —
    /// build with the real store, then DROP COLUMN to simulate the old schema).
    #[test]
    fn model_config_column_migrates_from_pre_existing_db() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let sid = {
            let store = SqliteSessionStore::open(&path).unwrap();
            let sid = store.create_session("cli", Some("m"), None).unwrap();
            store.append_message(&sid, &user_msg("kept")).unwrap();
            sid
        };
        {
            use rusqlite::Connection;
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("ALTER TABLE sessions DROP COLUMN model_config")
                .expect("DROP COLUMN model_config (simulate pre-migration DB)");
            let cols: Vec<String> = {
                let mut st = conn.prepare("PRAGMA table_info(sessions)").unwrap();
                st.query_map([], |r| r.get::<_, String>(1))
                    .unwrap()
                    .filter_map(|r| r.ok())
                    .collect()
            };
            assert!(
                !cols.iter().any(|c| c == "model_config"),
                "fixture must lack the column after DROP"
            );
        }
        let store = SqliteSessionStore::open(&path).unwrap();
        // Pre-existing row survived the migration.
        let msgs = store.load_messages(&sid).unwrap();
        assert_eq!(msgs.len(), 1, "pre-existing row must survive migration");
        // And the column is usable post-migration.
        store
            .patch_session_model_config(&sid, &obj(&[("k", Value::from(7))]))
            .unwrap();
        assert_eq!(
            store.get_session_model_config_value(&sid, "k").unwrap(),
            Some(Value::from(7))
        );
    }

    /// Backdate a session's started_at + last_activity so sweep/prune cutoffs
    /// can see it as old. Uses a separate connection on the same file (the same
    /// technique the migration tests use) because `create_session` stamps now().
    fn backdate(path: &std::path::Path, session_id: &str, seconds_ago: f64) {
        use rusqlite::Connection;
        let conn = Connection::open(path).unwrap();
        let ts = now_secs() - seconds_ago;
        conn.execute(
            "UPDATE sessions SET started_at = ?2, last_activity_at = ?2 WHERE id = ?1",
            rusqlite::params![session_id, ts],
        )
        .unwrap();
    }

    /// Set `pinned = 1` directly. There is no session-level pin setter in this
    /// port yet (the `sessions.pinned` column is written only by paths that have
    /// not been ported), so the sweep tests stamp the flag the same way `backdate`
    /// stamps timestamps.
    fn pin_via_sql(path: &std::path::Path, session_id: &str) {
        use rusqlite::Connection;
        let conn = Connection::open(path).unwrap();
        conn.execute(
            "UPDATE sessions SET pinned = 1 WHERE id = ?1",
            rusqlite::params![session_id],
        )
        .unwrap();
    }

    /// The core P3-3 need: open rows from state-owned sources whose process died
    /// get CLOSED (not deleted) so they can age out and be pruned later.
    #[test]
    fn sweep_closes_stale_open_rows_from_whitelisted_sources() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let store = SqliteSessionStore::open(&path).unwrap();
        let cli_old = store.create_session("cli", Some("m"), None).unwrap();
        let cli_new = store.create_session("cli", Some("m"), None).unwrap();
        backdate(&path, &cli_old, 100.0 * 86400.0);

        let closed = store
            .sweep_orphaned_sessions(90.0 * 86400.0, &["cli"], true)
            .unwrap();
        assert_eq!(closed, vec![cli_old.clone()], "only the stale row closes");

        let row = store.get_session_row(&cli_old).unwrap().unwrap();
        assert!(row.ended_at.is_some(), "row is closed, not deleted");
        assert_eq!(row.end_reason.as_deref(), Some("startup_orphan_reap"));
        // Non-destructive: the row and its messages survive.
        assert!(store.get_session_row(&cli_new).unwrap().is_some());
        assert!(
            store
                .get_session_row(&cli_new)
                .unwrap()
                .unwrap()
                .ended_at
                .is_none(),
            "a fresh session must not be swept"
        );
    }

    /// 🔴 `started_at` must gate the sweep, not just last-activity: a freshly
    /// created branch/compression child carries a COPIED (old) activity timestamp.
    /// Without this predicate the sweep would close brand-new sessions.
    #[test]
    fn sweep_spares_a_fresh_row_whose_activity_timestamp_is_old() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let store = SqliteSessionStore::open(&path).unwrap();
        let sid = store.create_session("cli", Some("m"), None).unwrap();
        // started_at stays NOW, but last_activity_at is backdated — the branch-child
        // shape upstream warns about.
        {
            use rusqlite::Connection;
            let conn = Connection::open(&path).unwrap();
            conn.execute(
                "UPDATE sessions SET last_activity_at = ?2 WHERE id = ?1",
                rusqlite::params![sid, now_secs() - 200.0 * 86400.0],
            )
            .unwrap();
        }
        let closed = store
            .sweep_orphaned_sessions(90.0 * 86400.0, &["cli"], true)
            .unwrap();
        assert!(closed.is_empty(), "a fresh started_at must protect the row");
        assert!(store
            .get_session_row(&sid)
            .unwrap()
            .unwrap()
            .ended_at
            .is_none());
    }

    /// sources is a WHITELIST: messaging/UI sources own their lifecycle and
    /// closing one triggers a routing loop, so they must never be swept.
    #[test]
    fn sweep_only_touches_whitelisted_sources() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let store = SqliteSessionStore::open(&path).unwrap();
        let tg = store.create_session("telegram", Some("m"), None).unwrap();
        let cli = store.create_session("cli", Some("m"), None).unwrap();
        backdate(&path, &tg, 100.0 * 86400.0);
        backdate(&path, &cli, 100.0 * 86400.0);

        let closed = store
            .sweep_orphaned_sessions(90.0 * 86400.0, &["cli", "cron"], true)
            .unwrap();
        assert_eq!(closed, vec![cli.clone()]);
        assert!(
            store
                .get_session_row(&tg)
                .unwrap()
                .unwrap()
                .ended_at
                .is_none(),
            "a non-whitelisted source must fail closed (untouched)"
        );
    }

    /// Never destructive to pinned rows, and first-reason-wins: an already-ended
    /// row keeps its original reason even if it also matches the sweep.
    #[test]
    fn sweep_spares_pinned_and_preserves_an_existing_end_reason() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let store = SqliteSessionStore::open(&path).unwrap();
        let pinned = store.create_session("cli", Some("m"), None).unwrap();
        let ended = store.create_session("cli", Some("m"), None).unwrap();
        backdate(&path, &pinned, 100.0 * 86400.0);
        backdate(&path, &ended, 100.0 * 86400.0);
        pin_via_sql(&path, &pinned);
        store.end_session(&ended, "user_exit").unwrap();

        let closed = store
            .sweep_orphaned_sessions(90.0 * 86400.0, &["cli"], true)
            .unwrap();
        assert!(
            closed.is_empty(),
            "pinned and already-ended rows are not swept"
        );
        assert!(
            store
                .get_session_row(&pinned)
                .unwrap()
                .unwrap()
                .ended_at
                .is_none(),
            "pinned stays open"
        );
        assert_eq!(
            store
                .get_session_row(&ended)
                .unwrap()
                .unwrap()
                .end_reason
                .as_deref(),
            Some("user_exit"),
            "the original reason must win (first-reason-wins)"
        );

        // With exclude_pinned = false the pinned row IS eligible.
        let closed = store
            .sweep_orphaned_sessions(90.0 * 86400.0, &["cli"], false)
            .unwrap();
        assert_eq!(closed, vec![pinned.clone()]);
    }

    /// Empty source list / non-positive window are no-ops (upstream returns []).
    #[test]
    fn sweep_noops_on_empty_sources_or_nonpositive_window() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let store = SqliteSessionStore::open(&path).unwrap();
        let sid = store.create_session("cli", Some("m"), None).unwrap();
        backdate(&path, &sid, 100.0 * 86400.0);
        assert!(store
            .sweep_orphaned_sessions(90.0 * 86400.0, &[], true)
            .unwrap()
            .is_empty());
        assert!(store
            .sweep_orphaned_sessions(0.0, &["cli"], true)
            .unwrap()
            .is_empty());
        assert!(store
            .sweep_orphaned_sessions(-5.0, &["cli"], true)
            .unwrap()
            .is_empty());
        assert!(store
            .get_session_row(&sid)
            .unwrap()
            .unwrap()
            .ended_at
            .is_none());
    }

    #[test]
    fn freelist_ratio_is_a_fraction_between_zero_and_one() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with(dir.path());
        let ratio = store.freelist_ratio();
        assert!(ratio.is_some(), "pragmas must resolve on a real DB");
        let r = ratio.unwrap();
        assert!((0.0..=1.0).contains(&r), "ratio out of range: {}", r);
    }

    // ---- maybe_auto_prune_and_vacuum (P3-3 ⑥ orchestration) ---------------

    /// One pass prunes an old ENDED row, closes a stale OPEN whitelisted row,
    /// records the throttle even on success, and a second immediate call skips.
    #[test]
    fn auto_maintenance_prunes_sweeps_and_throttles() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let store = SqliteSessionStore::open(&path).unwrap();
        let ended_old = store.create_session("cli", Some("m"), None).unwrap();
        let open_stale = store.create_session("cron", Some("m"), None).unwrap();
        let open_fresh = store.create_session("cli", Some("m"), None).unwrap();
        backdate(&path, &ended_old, 100.0 * 86400.0);
        backdate(&path, &open_stale, 100.0 * 86400.0);
        store.end_session(&ended_old, "user_exit").unwrap();

        let report = store.maybe_auto_prune_and_vacuum(90, 24, true, 30, 0.25);
        assert!(!report.skipped);
        assert_eq!(report.pruned, 1, "the old ended row is deleted");
        assert!(store.get_session_row(&ended_old).unwrap().is_none());
        assert_eq!(report.closed, 1, "the stale cron row is closed");
        assert_eq!(
            store
                .get_session_row(&open_stale)
                .unwrap()
                .unwrap()
                .end_reason
                .as_deref(),
            Some("startup_orphan_reap")
        );
        assert!(
            store
                .get_session_row(&open_fresh)
                .unwrap()
                .unwrap()
                .ended_at
                .is_none(),
            "a fresh open session is untouched"
        );
        assert!(report.error.is_none());
        // Throttle recorded even though this pass did work.
        assert!(store.get_meta("last_auto_prune").unwrap().is_some());

        // Second immediate call: inside the 24h window → skip, no further work.
        let again = store.maybe_auto_prune_and_vacuum(90, 24, true, 30, 0.25);
        assert!(again.skipped);
        assert_eq!(again.pruned, 0);
    }

    /// 🔴 The sweep runs AFTER the prune: a just-closed orphan must get a full
    /// retention window, so it is never deleted in the same pass that closed it.
    #[test]
    fn auto_maintenance_never_deletes_a_row_it_just_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let store = SqliteSessionStore::open(&path).unwrap();
        let orphan = store.create_session("kanban", Some("m"), None).unwrap();
        backdate(&path, &orphan, 400.0 * 86400.0);

        let report = store.maybe_auto_prune_and_vacuum(90, 24, false, 30, 0.25);
        assert_eq!(report.closed, 1);
        assert_eq!(report.pruned, 0, "the prune ran before the sweep closed it");
        let row = store.get_session_row(&orphan).unwrap().unwrap();
        assert!(row.ended_at.is_some(), "row survives, resumable");
    }

    /// Pinned sessions and messaging-source rows are never touched by a pass.
    #[test]
    fn auto_maintenance_spares_pinned_and_non_whitelisted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let store = SqliteSessionStore::open(&path).unwrap();
        let pinned_ended = store.create_session("cli", Some("m"), None).unwrap();
        let telegram_open = store.create_session("telegram", Some("m"), None).unwrap();
        backdate(&path, &pinned_ended, 100.0 * 86400.0);
        backdate(&path, &telegram_open, 100.0 * 86400.0);
        pin_via_sql(&path, &pinned_ended);
        store.end_session(&pinned_ended, "user_exit").unwrap();

        let report = store.maybe_auto_prune_and_vacuum(90, 24, true, 30, 0.25);
        assert_eq!(report.pruned, 0, "pinned ended row survives the prune");
        assert_eq!(report.closed, 0, "telegram is not a state-owned source");
        assert!(store.get_session_row(&pinned_ended).unwrap().is_some());
        assert!(store
            .get_session_row(&telegram_open)
            .unwrap()
            .unwrap()
            .ended_at
            .is_none());
    }

    /// Negative retention skips (future cutoff would match every ended row);
    /// corrupt throttle meta is treated as "no prior run" and proceeds.
    #[test]
    fn auto_maintenance_guards_negative_retention_and_corrupt_meta() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let store = SqliteSessionStore::open(&path).unwrap();
        let sid = store.create_session("cli", Some("m"), None).unwrap();
        backdate(&path, &sid, 100.0 * 86400.0);
        store.end_session(&sid, "user_exit").unwrap();

        let report = store.maybe_auto_prune_and_vacuum(-1, 24, true, 30, 0.25);
        assert!(report.skipped);
        assert_eq!(report.pruned, 0);
        assert!(store.get_session_row(&sid).unwrap().is_some());

        // Corrupt throttle: proceeds rather than skipping forever.
        store.set_meta("last_auto_prune", "not-a-number").unwrap();
        let report = store.maybe_auto_prune_and_vacuum(90, 24, false, 30, 0.25);
        assert!(!report.skipped);
        assert_eq!(report.pruned, 1);
    }

    /// slice3b-1: opening a store registers it in the live-store registry and
    /// dropping it deregisters it — the data the in-process holders arm reads.
    /// Path-keyed, so this test is isolated from other tests' stores.
    #[test]
    fn open_registers_and_drop_unregisters_the_live_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        // Fresh path: nothing registered yet.
        assert!(crate::state_holders::in_process_state_db_holders(&path, None).is_empty());

        let store = SqliteSessionStore::open(&path).unwrap();
        assert_ne!(store.instance_id, 0, "open() must assign a registry id");
        let holders = crate::state_holders::in_process_state_db_holders(&path, None);
        assert_eq!(holders.len(), 1, "one live generation after open");
        assert!(holders[0].1.contains("in-process"), "{:?}", holders[0].1);

        // The VACUUM caller excludes itself: with self excluded, no sibling.
        assert!(
            crate::state_holders::in_process_state_db_holders(&path, Some(store.instance_id),)
                .is_empty()
        );

        drop(store);
        assert!(
            crate::state_holders::in_process_state_db_holders(&path, None).is_empty(),
            "drop must deregister — no leak"
        );
    }
    /// 🔴 The VACUUM leg fails closed while the holders scan is unported: even
    /// with every gate passing (vacuum=true, pruned>0, no prior vacuum), no
    /// rewrite happens and the report says why.
    #[test]
    fn auto_maintenance_vacuum_leg_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let store = SqliteSessionStore::open(&path).unwrap();
        let sid = store.create_session("cli", Some("m"), None).unwrap();
        backdate(&path, &sid, 100.0 * 86400.0);
        store.end_session(&sid, "user_exit").unwrap();

        let report = store.maybe_auto_prune_and_vacuum(90, 24, true, 30, 0.25);
        assert_eq!(report.pruned, 1);
        assert!(
            !report.vacuumed,
            "VACUUM must not run without a holders proof"
        );
        assert!(
            report.vacuum_skipped_reason.is_some(),
            "the skip must be recorded, not silent"
        );
        assert!(store.get_meta("last_vacuum").unwrap().is_none());
    }

    /// A held lock makes a concurrent pass skip instead of waiting or pruning
    /// concurrently (two startups racing the same interval check).
    #[test]
    fn auto_maintenance_skips_while_the_lock_is_held() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let store = SqliteSessionStore::open(&path).unwrap();
        let lock_path =
            path.with_file_name(format!("{}.auto-maintenance.lock", file_name_str(&path)));
        let _held = MaintenanceLock::try_acquire(&lock_path)
            .unwrap()
            .expect("first acquisition succeeds");
        let report = store.maybe_auto_prune_and_vacuum(90, 24, true, 30, 0.25);
        assert!(report.skipped);
        assert_eq!(report.pruned, 0);
        assert!(store.get_meta("last_auto_prune").unwrap().is_none());
        drop(_held);
        // After release the pass runs.
        let report = store.maybe_auto_prune_and_vacuum(90, 24, true, 30, 0.25);
        assert!(!report.skipped);
    }

    /// The two durable anti-thrash columns round-trip, and the deadline's
    /// disarm semantics match upstream (`normalized or None` → NULL, not 0).
    #[test]
    fn compression_guards_roundtrip_and_disarm_to_null() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let store = SqliteSessionStore::open(&path).unwrap();
        let sid = store.create_session("cli", Some("m"), None).unwrap();

        // Defaults: 0 strikes, disarmed deadline.
        assert_eq!(store.get_compression_ineffective_count(&sid), 0);
        assert_eq!(store.get_compression_recovery_deadline(&sid), 0.0);

        store.set_compression_ineffective_count(&sid, 2);
        assert_eq!(store.get_compression_ineffective_count(&sid), 2);

        store.set_compression_recovery_deadline(&sid, 1_800_000_000.5);
        assert_eq!(
            store.get_compression_recovery_deadline(&sid),
            1_800_000_000.5
        );

        // Disarm, then 🔴 verify it stored NULL rather than 0.0 — checked
        // through the raw column so the assertion is about storage, not about
        // the getter's `unwrap_or(0.0)` masking the difference.
        store.set_compression_recovery_deadline(&sid, 0.0);
        assert_eq!(store.get_compression_recovery_deadline(&sid), 0.0);
        {
            use rusqlite::Connection;
            let conn = Connection::open(&path).unwrap();
            let is_null: bool = conn
                .query_row(
                    "SELECT compression_recovery_deadline IS NULL FROM sessions WHERE id = ?1",
                    rusqlite::params![sid],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(is_null, "disarm must store NULL (hermes `or None`)");
        }
    }

    /// Negative counts are clamped (upstream `max(0, int(count))`) on both
    /// write and read, so a bad row can never lower the trip threshold.
    #[test]
    fn compression_ineffective_count_is_clamped() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with(dir.path());
        let sid = store.create_session("cli", Some("m"), None).unwrap();
        store.set_compression_ineffective_count(&sid, -5);
        assert_eq!(store.get_compression_ineffective_count(&sid), 0);
        // A hand-edited negative row also reads back clamped.
        store
            .conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE sessions SET compression_ineffective_count = -3 WHERE id = ?1",
                params![sid],
            )
            .unwrap();
        assert_eq!(store.get_compression_ineffective_count(&sid), 0);
    }

    /// An empty session id never writes (upstream `if session_id:` guards), and
    /// reads of an unknown session degrade to the defaults instead of failing.
    #[test]
    fn compression_guards_ignore_empty_and_unknown_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with(dir.path());
        store.set_compression_ineffective_count("", 5);
        store.set_compression_recovery_deadline("", 1.0);
        assert_eq!(store.get_compression_ineffective_count("nope"), 0);
        assert_eq!(store.get_compression_recovery_deadline("nope"), 0.0);
    }

    /// 🔴 A DB created before these columns existed gains them losslessly —
    /// existing rows read as 0/disarmed rather than erroring.
    #[test]
    fn compression_columns_migrate_from_pre_existing_db() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        // Build a real DB, keep a row, then strip the columns to simulate an
        // older schema (the technique used for the reasoning-column migration).
        {
            let store = SqliteSessionStore::open(&path).unwrap();
            store.create_session("cli", Some("m"), None).unwrap();
        }
        {
            use rusqlite::Connection;
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "ALTER TABLE sessions DROP COLUMN compression_ineffective_count;
                 ALTER TABLE sessions DROP COLUMN compression_recovery_deadline;",
            )
            .unwrap();
        }
        let store = SqliteSessionStore::open(&path).unwrap();
        let rows = store.list_sessions(8).unwrap();
        assert!(!rows.is_empty(), "the pre-existing row survived");
        let sid = rows[0].id.clone();
        assert_eq!(store.get_compression_ineffective_count(&sid), 0);
        assert_eq!(store.get_compression_recovery_deadline(&sid), 0.0);
        // And they are writable after migration.
        store.set_compression_ineffective_count(&sid, 2);
        assert_eq!(store.get_compression_ineffective_count(&sid), 2);
    }

    #[test]
    fn parse_model_config_is_tolerant() {
        // valid object
        let m = parse_model_config(r#"{"a":1,"b":"x"}"#);
        assert_eq!(m.get("a"), Some(&Value::from(1)));
        assert_eq!(m.get("b"), Some(&Value::from("x")));
        // empty / whitespace → {}
        assert!(parse_model_config("").is_empty());
        assert!(parse_model_config("   ").is_empty());
        // non-object JSON → {} (array, number, string, bool, null)
        assert!(parse_model_config("[1,2,3]").is_empty());
        assert!(parse_model_config("42").is_empty());
        assert!(parse_model_config("\"str\"").is_empty());
        assert!(parse_model_config("null").is_empty());
        // truncated / garbage → {} (must not panic or error)
        assert!(parse_model_config("{not json").is_empty());
        assert!(parse_model_config(r#"{"a":1,"#).is_empty());
    }

    // ---------------------------------------------------------------
    // P3-1e: durable rejected-reasoning-route table
    // ---------------------------------------------------------------

    fn route(p: &str, h: &str, m: &str) -> RejectedReasoningRoute {
        RejectedReasoningRoute {
            provider: p.into(),
            host: h.into(),
            model: m.into(),
            carriers: vec!["reasoning_content".into()],
        }
    }

    #[test]
    fn rejected_routes_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with(dir.path());
        let sid = store.create_session("cli", Some("m"), None).unwrap();
        assert!(
            store.rejected_reasoning_routes(&sid).unwrap().is_empty(),
            "a fresh session has learned nothing"
        );
        let routes = vec![route("mistral", "api.mistral.ai", "mistral-large")];
        store.set_rejected_reasoning_routes(&sid, &routes).unwrap();
        assert_eq!(store.rejected_reasoning_routes(&sid).unwrap(), routes);
    }

    /// The key is per-(provider, host, model): one gateway serves models on
    /// different upstream lanes, so a rejection learned for one model must not
    /// strip carriers from its siblings (upstream's stated reason).
    #[test]
    fn rejected_routes_keep_siblings_distinct() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with(dir.path());
        let sid = store.create_session("cli", Some("m"), None).unwrap();
        let routes = vec![
            route("openrouter", "openrouter.ai", "model-a"),
            route("openrouter", "openrouter.ai", "model-b"),
        ];
        store.set_rejected_reasoning_routes(&sid, &routes).unwrap();
        let got = store.rejected_reasoning_routes(&sid).unwrap();
        assert_eq!(got.len(), 2, "both sibling models must survive");
        assert!(got.contains(&routes[0]) && got.contains(&routes[1]));
    }

    /// Survives a reopen — the entire point of persisting it.
    #[test]
    fn rejected_routes_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let sid = {
            let store = store_with(dir.path());
            let sid = store.create_session("cli", Some("m"), None).unwrap();
            store
                .set_rejected_reasoning_routes(&sid, &[route("groq", "groq.com", "llama")])
                .unwrap();
            sid
        };
        let store = store_with(dir.path());
        let got = store.rejected_reasoning_routes(&sid).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].model, "llama");
        assert_eq!(got[0].carriers, vec!["reasoning_content".to_string()]);
    }

    /// An empty table removes the key (column back to NULL, not "[]"), and the
    /// blob is sorted so rewriting the same set is byte-stable.
    #[test]
    fn rejected_routes_empty_clears_and_sorting_is_stable() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with(dir.path());
        let sid = store.create_session("cli", Some("m"), None).unwrap();
        store
            .set_rejected_reasoning_routes(&sid, &[route("a", "h", "m")])
            .unwrap();
        store.set_rejected_reasoning_routes(&sid, &[]).unwrap();
        assert!(store.rejected_reasoning_routes(&sid).unwrap().is_empty());
        assert_eq!(
            store
                .get_session_model_config_value(&sid, REJECTED_REASONING_KEY)
                .unwrap(),
            None,
            "clearing must leave the key absent, not an empty array"
        );
        store
            .set_rejected_reasoning_routes(
                &sid,
                &[route("zeta", "z", "m"), route("alpha", "a", "m")],
            )
            .unwrap();
        let got = store.rejected_reasoning_routes(&sid).unwrap();
        assert_eq!(got[0].provider, "alpha");
        assert_eq!(got[1].provider, "zeta");
    }

    /// Malformed rows are dropped, never propagated — a corrupt blob degrades to
    /// "nothing learned", it must not break session load.
    #[test]
    fn rejected_routes_tolerate_malformed_blobs() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with(dir.path());
        let sid = store.create_session("cli", Some("m"), None).unwrap();
        // Write garbage directly through the KV layer, bypassing the typed setter.
        let mut patch = Map::new();
        patch.insert(
            REJECTED_REASONING_KEY.to_string(),
            serde_json::json!([
                ["ok", "host", "model", ["reasoning_content"]],
                ["too", "few"],
                "not-an-array",
                ["bad-carriers", "h", "m", "not-a-list"],
                [1, 2, 3, []]
            ]),
        );
        store.patch_session_model_config(&sid, &patch).unwrap();
        let got = store.rejected_reasoning_routes(&sid).unwrap();
        assert_eq!(got.len(), 1, "only the well-formed row survives: {:?}", got);
        assert_eq!(got[0].provider, "ok");
        assert_eq!(got[0].carriers, vec!["reasoning_content".to_string()]);

        // A wholly non-array value yields an empty table, not an error.
        let mut patch = Map::new();
        patch.insert(
            REJECTED_REASONING_KEY.to_string(),
            serde_json::json!({"not": "an array"}),
        );
        store.patch_session_model_config(&sid, &patch).unwrap();
        assert!(store.rejected_reasoning_routes(&sid).unwrap().is_empty());
    }
}

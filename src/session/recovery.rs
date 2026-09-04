//! Offline, non-destructive session-database recovery — port of hermes'
//! `hermes_cli/session_recovery.py`.
//!
//! The recovery path deliberately avoids in-place repair:
//! - the supplied source database is never opened in place (it is copied,
//!   together with any WAL/SHM/rollback-journal sidecars, into a disposable
//!   working directory first);
//! - canonical rows are copied into a newly initialized current-schema
//!   database;
//! - the derived FTS index is rebuilt, not copied;
//! - the recovered database is never installed over the active database.

use crate::error::{AgentError, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Tables whose rows are copied into the recovered database (hermes
/// `_CANONICAL_TABLES`, adapted to the ulnclaw schema).
const CANONICAL_TABLES: &[&str] = &[
    "system_prompts",
    "sessions",
    "messages",
    "state_meta",
    "async_delegations",
    "cron_jobs",
    "boards",
    "tasks",
    "task_links",
    "task_comments",
    "task_events",
    "task_attachments",
];

/// state_meta keys describing derived state; the destination regenerates
/// them (hermes `_GENERATED_META_KEYS`).
fn is_generated_meta_key(key: &str) -> bool {
    key.starts_with("fts_")
}

/// Per-table copy statistics.
#[derive(Debug, Clone, Default, Serialize)]
pub struct TableReport {
    /// Rows copied into the recovered database.
    pub copied: usize,
    /// Rows skipped (unreadable/corrupt or filtered).
    pub skipped: usize,
    /// True when the bulk copy failed and per-row salvage was used.
    pub salvaged: bool,
}

/// Outcome of one recovery run.
#[derive(Debug, Clone, Serialize)]
pub struct RecoveryReport {
    pub source: String,
    pub output: String,
    pub tables: BTreeMap<String, TableReport>,
    /// Session rows synthesized for orphaned messages.
    pub reconstructed_sessions: usize,
    /// Integrity check result of the recovered database.
    pub integrity_ok: bool,
    /// Whether the FTS index was rebuilt.
    pub fts_rebuilt: bool,
    pub sessions: usize,
    pub messages: usize,
}

/// Errors specific to recovery safety checks.
fn safety(msg: impl Into<String>) -> AgentError {
    AgentError::session(format!("recovery: {}", msg.into()))
}

fn sidecar_paths(source: &Path) -> Vec<PathBuf> {
    let file_name = source.file_name().and_then(|f| f.to_str()).unwrap_or("");
    ["-wal", "-shm", "-journal"]
        .iter()
        .map(|suffix| source.with_file_name(format!("{}{}", file_name, suffix)))
        .filter(|p| p.exists())
        .collect()
}

/// Copy the source bundle (db + sidecars) into `workdir`; returns the path
/// of the copied database (hermes `_copy_source_bundle`).
fn copy_source_bundle(source: &Path, workdir: &Path) -> Result<PathBuf> {
    let dest = workdir.join(
        source
            .file_name()
            .ok_or_else(|| safety("source has no file name"))?,
    );
    std::fs::copy(source, &dest)
        .map_err(|e| safety(format!("copy source into working dir: {}", e)))?;
    for sidecar in sidecar_paths(source) {
        let name = sidecar.file_name().unwrap_or_default();
        std::fs::copy(&sidecar, workdir.join(name)).ok();
    }
    Ok(dest)
}

/// Tables present in the source snapshot.
fn list_tables(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'")
        .map_err(|e| safety(format!("inspect snapshot: {}", e)))?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| safety(format!("inspect snapshot: {}", e)))?;
    let mut tables = Vec::new();
    for row in rows {
        if let Ok(name) = row {
            tables.push(name);
        }
    }
    Ok(tables)
}

/// Column names of `table` in the given connection.
fn table_columns(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({})", table))
        .map_err(|e| safety(format!("table_info({}): {}", table, e)))?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| safety(format!("table_info({}): {}", table, e)))?;
    let mut columns = Vec::new();
    for row in rows {
        if let Ok(name) = row {
            columns.push(name);
        }
    }
    Ok(columns)
}

/// Bulk-copy one table through the shared column intersection.
fn copy_table_bulk(
    source: &Connection,
    dest: &Connection,
    table: &str,
    columns: &[String],
) -> Result<usize> {
    let column_list = columns.join(", ");
    let mut stmt = source
        .prepare(&format!("SELECT {} FROM {}", column_list, table))
        .map_err(|e| safety(format!("select {}: {}", table, e)))?;
    let placeholders: Vec<String> = (0..columns.len()).map(|i| format!("?{}", i + 1)).collect();
    let insert_sql = format!(
        "INSERT INTO {} ({}) VALUES ({})",
        table,
        column_list,
        placeholders.join(", ")
    );
    let mut rows = stmt
        .query([])
        .map_err(|e| safety(format!("read {}: {}", table, e)))?;
    let mut copied = 0usize;
    while let Some(row) = rows
        .next()
        .map_err(|e| safety(format!("read {}: {}", table, e)))?
    {
        let values: Vec<rusqlite::types::Value> = (0..columns.len())
            .map(|i| row.get::<_, rusqlite::types::Value>(i))
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| safety(format!("decode {}: {}", table, e)))?;
        let refs: Vec<&dyn rusqlite::types::ToSql> = values
            .iter()
            .map(|v| v as &dyn rusqlite::types::ToSql)
            .collect();
        dest.execute(&insert_sql, refs.as_slice())
            .map_err(|e| safety(format!("insert into {}: {}", table, e)))?;
        copied += 1;
    }
    Ok(copied)
}

/// Rowid-bounded salvage: read the table row by row, skipping unreadable
/// rows (hermes `_copy_table_salvage`).
fn copy_table_salvage(
    source: &Connection,
    dest: &Connection,
    table: &str,
    columns: &[String],
) -> Result<(usize, usize)> {
    let (min_rowid, max_rowid): (Option<i64>, Option<i64>) = source
        .query_row(
            &format!("SELECT MIN(rowid), MAX(rowid) FROM {}", table),
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|e| safety(format!("salvage bounds {}: {}", table, e)))?
        .unwrap_or((None, None));
    let (Some(min_rowid), Some(max_rowid)) = (min_rowid, max_rowid) else {
        return Ok((0, 0));
    };
    let column_list = columns.join(", ");
    let placeholders: Vec<String> = (0..columns.len()).map(|i| format!("?{}", i + 1)).collect();
    let insert_sql = format!(
        "INSERT INTO {} ({}) VALUES ({})",
        table,
        column_list,
        placeholders.join(", ")
    );
    let mut copied = 0usize;
    let mut skipped = 0usize;
    for rowid in min_rowid..=max_rowid {
        let row = source
            .query_row(
                &format!("SELECT {} FROM {} WHERE rowid = ?1", column_list, table),
                params![rowid],
                |row| {
                    (0..columns.len())
                        .map(|i| row.get::<_, rusqlite::types::Value>(i))
                        .collect::<rusqlite::Result<Vec<_>>>()
                },
            )
            .optional();
        match row {
            Ok(Some(values)) => {
                let refs: Vec<&dyn rusqlite::types::ToSql> = values
                    .iter()
                    .map(|v| v as &dyn rusqlite::types::ToSql)
                    .collect();
                match dest.execute(&insert_sql, refs.as_slice()) {
                    Ok(_) => copied += 1,
                    Err(_) => skipped += 1,
                }
            }
            _ => skipped += 1,
        }
    }
    Ok((copied, skipped))
}

/// Synthesize session rows for orphaned messages (hermes
/// `_reconstruct_missing_sessions`).
fn reconstruct_missing_sessions(dest: &Connection) -> Result<usize> {
    let mut stmt = dest
        .prepare(
            "SELECT session_id, MIN(timestamp), COUNT(*) FROM messages
             WHERE session_id NOT IN (SELECT id FROM sessions)
             GROUP BY session_id",
        )
        .map_err(|e| safety(format!("orphan scan: {}", e)))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, f64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .map_err(|e| safety(format!("orphan scan: {}", e)))?;
    let mut orphans = Vec::new();
    for row in rows {
        if let Ok(tuple) = row {
            orphans.push(tuple);
        }
    }
    for (session_id, started_at, count) in &orphans {
        dest.execute(
            "INSERT OR IGNORE INTO sessions (id, source, started_at, message_count, title)
             VALUES (?1, 'recovered', ?2, ?3, 'Recovered session')",
            params![session_id, started_at, count],
        )
        .map_err(|e| safety(format!("reconstruct session {}: {}", session_id, e)))?;
    }
    Ok(orphans.len())
}

/// Rebuild the FTS index from the messages table.
fn rebuild_fts(dest: &Connection) -> Result<bool> {
    let has_fts = dest
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'messages_fts'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map(|count| count > 0)
        .unwrap_or(false);
    if !has_fts {
        return Ok(false);
    }
    dest.execute_batch(
        "INSERT INTO messages_fts (rowid, content)
         SELECT id, COALESCE(content, '') FROM messages",
    )
    .map_err(|e| safety(format!("rebuild FTS: {}", e)))?;
    Ok(true)
}

/// Recover a damaged session database into a fresh current-schema database.
///
/// `output` must not exist yet, must differ from the source, and must not be
/// the active `<home>/state.db` — the recovered file is never installed over
/// the live database (hermes safety rules).
pub fn recover_session_database(source: &Path, output: &Path) -> Result<RecoveryReport> {
    if !source.is_file() {
        return Err(safety(format!(
            "source database not found: {}",
            source.display()
        )));
    }
    let source = source
        .canonicalize()
        .map_err(|e| safety(format!("resolve source: {}", e)))?;
    if output.exists() {
        return Err(safety(format!(
            "output already exists (refusing to overwrite): {}",
            output.display()
        )));
    }
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent).map_err(|e| safety(format!("create output dir: {}", e)))?;
    }
    let output = output
        .canonicalize()
        .unwrap_or_else(|_| output.to_path_buf());
    if output == source {
        return Err(safety(
            "output must differ from the source database".to_string(),
        ));
    }
    let active = crate::config::ulnclaw_home().join("state.db");
    if let (Ok(active), Ok(output_check)) = (active.canonicalize(), output.canonicalize()) {
        if active == output_check {
            return Err(safety(
                "refusing to write over the active state.db — pass a different --out".to_string(),
            ));
        }
    }

    // 1. Snapshot the bundle into a disposable working directory.
    let workdir = std::env::temp_dir().join(format!(
        "ulnclaw-recovery-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&workdir).map_err(|e| safety(format!("create working dir: {}", e)))?;
    let scope_guard = WorkdirGuard(workdir.clone());
    let snapshot = copy_source_bundle(&source, &workdir)?;

    // 2. Inspect the snapshot (never the original file).
    let source_conn =
        Connection::open_with_flags(&snapshot, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
            .map_err(|e| {
                safety(format!(
                    "snapshot of {} cannot be opened even after copying: {}",
                    source.display(),
                    e
                ))
            })?;
    // Damage is expected — do not let SQLite's integrity checks abort reads
    // early; salvage handles unreadable rows.
    source_conn.execute_batch("PRAGMA foreign_keys=OFF;").ok();
    let source_tables = list_tables(&source_conn)?;

    // 3. Initialize a fresh current-schema destination.
    let dest_conn =
        Connection::open(&output).map_err(|e| safety(format!("create output database: {}", e)))?;
    dest_conn
        .execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=OFF;")
        .map_err(|e| safety(format!("output pragmas: {}", e)))?;
    crate::session::sqlite::initialize_schema(&dest_conn)?;

    // 4. Copy canonical tables (bulk first, salvage on failure).
    let mut tables = BTreeMap::new();
    for table in CANONICAL_TABLES {
        if !source_tables.iter().any(|name| name == table) {
            continue;
        }
        let source_columns = table_columns(&source_conn, table)?;
        let dest_columns = table_columns(&dest_conn, table)?;
        let shared: Vec<String> = dest_columns
            .iter()
            .filter(|column| source_columns.iter().any(|c| c == *column))
            .cloned()
            .collect();
        if shared.is_empty() {
            continue;
        }
        let mut report = TableReport::default();
        let is_meta = *table == "state_meta";
        match copy_table_bulk(&source_conn, &dest_conn, table, &shared) {
            Ok(copied) => {
                report.copied = copied;
                if is_meta {
                    report.skipped = prune_generated_meta(&dest_conn)?;
                }
            }
            Err(_) => {
                // Bulk read hit damage — fall back to row-by-row salvage.
                dest_conn
                    .execute(&format!("DELETE FROM {}", table), [])
                    .ok();
                let (copied, skipped) =
                    copy_table_salvage(&source_conn, &dest_conn, table, &shared)?;
                report.copied = copied;
                report.skipped = skipped;
                report.salvaged = true;
                if is_meta {
                    report.skipped += prune_generated_meta(&dest_conn)?;
                }
            }
        }
        tables.insert(table.to_string(), report);
    }

    // 5. Synthesize missing session rows, rebuild FTS, verify.
    let reconstructed_sessions = reconstruct_missing_sessions(&dest_conn)?;
    let fts_rebuilt = rebuild_fts(&dest_conn)?;
    let integrity_ok = dest_conn
        .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
        .map(|result| result == "ok")
        .unwrap_or(false);
    let sessions = dest_conn
        .query_row("SELECT COUNT(*) FROM sessions", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap_or(0) as usize;
    let messages = dest_conn
        .query_row("SELECT COUNT(*) FROM messages", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap_or(0) as usize;

    drop(source_conn);
    drop(dest_conn);
    drop(scope_guard);

    Ok(RecoveryReport {
        source: source.display().to_string(),
        output: output.display().to_string(),
        tables,
        reconstructed_sessions,
        integrity_ok,
        fts_rebuilt,
        sessions,
        messages,
    })
}

/// Drop derived `fts_*` meta keys from a copied state_meta table.
fn prune_generated_meta(dest: &Connection) -> Result<usize> {
    let mut stmt = dest
        .prepare("SELECT key FROM state_meta")
        .map_err(|e| safety(format!("meta scan: {}", e)))?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| safety(format!("meta scan: {}", e)))?;
    let mut pruned = 0usize;
    let mut keys = Vec::new();
    for row in rows {
        if let Ok(key) = row {
            if is_generated_meta_key(&key) {
                keys.push(key);
            }
        }
    }
    for key in &keys {
        dest.execute("DELETE FROM state_meta WHERE key = ?1", params![key])
            .ok();
        pruned += 1;
    }
    Ok(pruned)
}

/// Delete the working directory on scope exit.
struct WorkdirGuard(PathBuf);

impl Drop for WorkdirGuard {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

/// Write a recovery report next to the output (`<output>.recovery-report.json`).
pub fn write_recovery_report(report: &RecoveryReport) -> Result<PathBuf> {
    let path = PathBuf::from(format!("{}.recovery-report.json", report.output));
    let json = serde_json::to_string_pretty(report)
        .map_err(|e| safety(format!("serialize report: {}", e)))?;
    std::fs::write(&path, json).map_err(|e| safety(format!("write report: {}", e)))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Message;
    use crate::session::sqlite::SqliteSessionStore;
    use crate::session::SessionStore;

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "ulnclaw-recovery-test-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ))
    }

    fn seed_store(path: &Path, sessions: usize, messages_per_session: usize) -> SqliteSessionStore {
        let store = SqliteSessionStore::open(path).expect("open seed store");
        for session_index in 0..sessions {
            let session_id = store
                .create_session("test", Some("test-model"), None)
                .expect("create session");
            for message_index in 0..messages_per_session {
                let content = format!(
                    "session {} message {} unique-search-token-{}",
                    session_index, message_index, session_index
                );
                let message = Message {
                    role: crate::provider::Role::User,
                    content: Some(content),
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                };
                store.append_message(&session_id, &message).expect("append");
            }
        }
        store
    }

    #[test]
    fn recover_healthy_database() {
        let source = temp_path("healthy-src");
        let output = temp_path("healthy-out");
        let store = seed_store(&source, 3, 5);
        drop(store);

        let report = recover_session_database(&source, &output).expect("recover");
        assert!(report.integrity_ok);
        assert_eq!(report.sessions, 3);
        assert_eq!(report.messages, 15);
        assert!(report.tables.contains_key("sessions"));
        assert_eq!(report.reconstructed_sessions, 0);

        // Recovered store is usable, including FTS search.
        let recovered = SqliteSessionStore::open(&output).expect("open recovered");
        let sessions = recovered.list_sessions(10).expect("list");
        assert_eq!(sessions.len(), 3);
        if recovered.has_fts() {
            let hits = recovered
                .search_messages("unique-search-token-1", 10)
                .expect("search");
            assert!(!hits.is_empty());
        }
        let report_path = write_recovery_report(&report).expect("report");
        assert!(report_path.exists());
        std::fs::remove_file(&source).ok();
        std::fs::remove_file(&output).ok();
        std::fs::remove_file(&report_path).ok();
    }

    #[test]
    fn reconstructs_missing_sessions() {
        let source = temp_path("orphan-src");
        let output = temp_path("orphan-out");
        let store = seed_store(&source, 2, 4);
        drop(store);

        // Damage: drop one session row, orphaning its messages.
        let conn = Connection::open(&source).expect("open source");
        conn.execute_batch("PRAGMA foreign_keys=OFF;")
            .expect("fk off");
        let orphaned: String = conn
            .query_row("SELECT id FROM sessions LIMIT 1", [], |row| row.get(0))
            .expect("session id");
        conn.execute("DELETE FROM sessions WHERE id = ?1", params![orphaned])
            .expect("delete session");
        drop(conn);

        let report = recover_session_database(&source, &output).expect("recover");
        assert_eq!(report.reconstructed_sessions, 1);
        assert_eq!(report.sessions, 2);
        assert_eq!(report.messages, 8);
        let conn = Connection::open(&output).expect("open output");
        let recovered_source: String = conn
            .query_row(
                "SELECT source FROM sessions WHERE id = ?1",
                params![orphaned],
                |row| row.get(0),
            )
            .expect("reconstructed row");
        assert_eq!(recovered_source, "recovered");
        std::fs::remove_file(&source).ok();
        std::fs::remove_file(&output).ok();
    }

    #[test]
    fn salvages_corrupted_database() {
        let source = temp_path("corrupt-src");
        let output = temp_path("corrupt-out");
        let store = seed_store(&source, 5, 40);
        drop(store);
        // Checkpoint WAL so the main file holds the data.
        {
            let conn = Connection::open(&source).expect("open");
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").ok();
        }
        // Flip bytes well past the header page.
        let mut bytes = std::fs::read(&source).expect("read db");
        assert!(bytes.len() > 8192);
        let offset = bytes.len() * 2 / 3;
        for index in offset..offset + 32 {
            bytes[index] ^= 0xFF;
        }
        std::fs::write(&source, &bytes).expect("write corrupted db");

        let report = recover_session_database(&source, &output).expect("recover");
        assert!(report.integrity_ok, "recovered database must be clean");
        assert!(report.messages > 0, "some messages must survive");
        assert!(report.sessions > 0);
        std::fs::remove_file(&source).ok();
        std::fs::remove_file(&output).ok();
    }

    #[test]
    fn safety_checks() {
        let source = temp_path("safety-src");
        let store = seed_store(&source, 1, 1);
        drop(store);

        // Output exists → refuse.
        let output = temp_path("safety-out");
        std::fs::write(&output, "existing").expect("write");
        assert!(recover_session_database(&source, &output).is_err());
        std::fs::remove_file(&output).ok();

        // Output == source → refuse.
        assert!(recover_session_database(&source, &source).is_err());

        // Missing source → refuse.
        let missing = temp_path("safety-missing");
        assert!(recover_session_database(&missing, &output).is_err());
        std::fs::remove_file(&source).ok();
    }
}

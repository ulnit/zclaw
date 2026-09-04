//! Repair malformed `state.db` schemas (hermes `repair_state_db_schema`).
//!
//! Handles two corruption classes: the "duplicate object definition" /
//! malformed-schema class where even `PRAGMA` statements fail, and the FTS
//! write-corruption class where base tables read fine and `integrity_check`
//! passes but writes fail through the `messages_fts` triggers. Recovery
//! strategies run least-destructive first and escalate:
//!
//! 1. Rebuild the FTS index in place via the FTS5 `'rebuild'` command.
//! 2. `REINDEX` stale B-tree indexes.
//! 3. De-duplicate `sqlite_master` (keep the lowest rowid per type/name),
//!    preserving the existing FTS index.
//! 4. Drop every `messages_fts%` schema object + `VACUUM`; the next store
//!    open rebuilds the index from the canonical `messages` rows.
//!
//! Canonical `sessions` / `messages` rows are never modified. A timestamped
//! raw backup is taken first unless disabled.

use rusqlite::Connection;
use std::path::{Path, PathBuf};

/// Outcome of a schema repair attempt (hermes repair report shape).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct RepairReport {
    pub repaired: bool,
    pub strategy: Option<String>,
    pub backup_path: Option<PathBuf>,
    pub error: Option<String>,
}

/// Probe a database on a fresh connection (hermes `_db_opens_cleanly`).
/// Returns `None` when healthy, otherwise a human-readable reason. Runs the
/// same first statement (`PRAGMA journal_mode`) that trips a malformed
/// schema parse, then `integrity_check`, a canonical `sessions` read, an
/// FTS MATCH read probe, and a rolled-back FTS write probe so an index that
/// rejects writes cannot slip past as healthy.
pub fn db_opens_cleanly(db_path: &Path) -> Option<String> {
    let conn = match Connection::open(db_path) {
        Ok(conn) => conn,
        Err(e) => return Some(format!("open failed: {e}")),
    };
    if let Err(e) = conn.execute_batch("PRAGMA journal_mode") {
        return Some(e.to_string());
    }
    let problems: Vec<String> = match conn.prepare("PRAGMA integrity_check") {
        Ok(mut stmt) => match stmt.query_map([], |r| r.get::<_, String>(0)) {
            Ok(rows) => rows
                .flatten()
                .filter(|row| row.to_lowercase() != "ok")
                .collect(),
            Err(e) => return Some(e.to_string()),
        },
        Err(e) => return Some(e.to_string()),
    };
    if !problems.is_empty() {
        let mut shown = problems;
        shown.truncate(3);
        return Some(shown.join("; "));
    }
    if let Err(e) = conn.query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get::<_, i64>(0)) {
        let msg = e.to_string();
        if msg.to_lowercase().contains("no such table") {
            // Brand-new file mid-init — not the corruption class we probe.
            return None;
        }
        return Some(msg);
    }
    // FTS read probe: the quoted empty phrase parses, scans zero rows, and
    // exercises the same shadow-table read path the search tools use.
    match conn.query_row(
        "SELECT 1 FROM messages_fts WHERE messages_fts MATCH '\"\"' LIMIT 1",
        [],
        |_| Ok(()),
    ) {
        Ok(()) | Err(rusqlite::Error::QueryReturnedNoRows) => {}
        Err(e) => {
            let msg = e.to_string();
            let lower = msg.to_lowercase();
            let capability_gap = lower.contains("no such table")
                || lower.contains("no such module")
                || lower.contains("no such column");
            if !capability_gap {
                return Some(format!("fts5 read probe failed on messages_fts: {msg}"));
            }
        }
    }
    // FTS write probe: drive a row through the FTS triggers inside a
    // transaction that is always rolled back.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let probe_session_id = format!("_ulnclaw_fts_health_probe_{nanos}");
    let now = nanos as f64 / 1_000_000_000.0;
    let probe = || -> Result<(), rusqlite::Error> {
        conn.execute_batch("BEGIN IMMEDIATE")?;
        conn.execute(
            "INSERT INTO sessions (id, source, started_at) VALUES (?1, '_health_probe', ?2)",
            rusqlite::params![probe_session_id, now],
        )?;
        conn.execute(
            "INSERT INTO messages (session_id, role, content, timestamp)
             VALUES (?1, 'user', '_fts_health_probe', ?2)",
            rusqlite::params![probe_session_id, now],
        )?;
        conn.execute_batch("ROLLBACK")?;
        Ok(())
    };
    if let Err(e) = probe() {
        conn.execute_batch("ROLLBACK").ok();
        let msg = e.to_string();
        let lower = msg.to_lowercase();
        if lower.contains("no such table") || lower.contains("no such column") {
            return None;
        }
        return Some(msg);
    }
    None
}

/// Raw-copy a (possibly malformed) DB file to a timestamped backup beside
/// it (hermes `_backup_db_file`). The DB will not open cleanly, so the
/// bytes are preserved exactly for forensics / manual restore; WAL and SHM
/// sidecars are copied too when present. Returns the backup path, or `None`
/// on failure.
pub fn backup_db_file(db_path: &Path) -> Option<PathBuf> {
    let file_name = db_path.file_name()?.to_str()?;
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let backup = db_path.with_file_name(format!("{file_name}.bak.{ts}"));
    std::fs::copy(db_path, &backup).ok()?;
    for suffix in ["-wal", "-shm"] {
        let sidecar = db_path.with_file_name(format!("{file_name}{suffix}"));
        if sidecar.exists() {
            let dest = db_path.with_file_name(format!("{file_name}{suffix}.bak.{ts}"));
            std::fs::copy(&sidecar, &dest).ok();
        }
    }
    Some(backup)
}

/// Repair a `state.db` whose schema is malformed or whose FTS index rejects
/// writes (hermes `repair_state_db_schema`). Never modifies canonical
/// session/message rows; returns a [`RepairReport`] describing the strategy
/// that made the database open cleanly (if any).
pub fn repair_state_db_schema(db_path: &Path, backup: bool) -> RepairReport {
    let mut report = RepairReport {
        repaired: false,
        strategy: None,
        backup_path: None,
        error: None,
    };
    if !db_path.exists() {
        report.error = Some(format!("{} does not exist", db_path.display()));
        return report;
    }
    if db_opens_cleanly(db_path).is_none() {
        report.repaired = true;
        report.strategy = Some("already_healthy".to_string());
        return report;
    }
    if backup {
        report.backup_path = backup_db_file(db_path);
    }

    // ── Strategy 0: rebuild the FTS index in place (write-corruption) ──
    if with_conn(db_path, |conn| {
        conn.execute(
            "INSERT INTO messages_fts(messages_fts) VALUES('rebuild')",
            [],
        )
        .map(|_| ())
        .or_else(|e| {
            // Table absent (FTS disabled / dropped) — skip.
            if e.to_string().to_lowercase().contains("no such table") {
                Ok(())
            } else {
                Err(e)
            }
        })
    }) && db_opens_cleanly(db_path).is_none()
    {
        report.repaired = true;
        report.strategy = Some("rebuild_fts".to_string());
        return report;
    }

    // ── Strategy 0.5: rebuild stale B-tree indexes via REINDEX ──
    if with_conn(db_path, |conn| conn.execute_batch("REINDEX"))
        && db_opens_cleanly(db_path).is_none()
    {
        report.repaired = true;
        report.strategy = Some("reindex_btree".to_string());
        return report;
    }

    // ── Strategy 1: de-duplicate sqlite_master (keeps the FTS index) ──
    if with_conn(db_path, |conn| {
        conn.execute_batch("PRAGMA writable_schema=ON")?;
        let dupes: Vec<(String, String, i64)> = {
            let mut stmt = conn.prepare(
                "SELECT type, name, MIN(rowid) FROM sqlite_master
                 GROUP BY type, name HAVING COUNT(*) > 1",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        for (type_, name, keep) in dupes {
            conn.execute(
                "DELETE FROM sqlite_master WHERE type IS ?1 AND name IS ?2 AND rowid <> ?3",
                rusqlite::params![type_, name, keep],
            )?;
        }
        conn.execute_batch("PRAGMA writable_schema=OFF")
    }) && db_opens_cleanly(db_path).is_none()
    {
        report.repaired = true;
        report.strategy = Some("dedup_schema".to_string());
        return report;
    }

    // ── Strategy 2: drop all FTS schema, VACUUM, rebuild on next open ──
    match with_conn(db_path, |conn| {
        conn.execute_batch("PRAGMA writable_schema=ON")?;
        conn.execute(
            "DELETE FROM sqlite_master WHERE name LIKE 'messages_fts%'",
            [],
        )?;
        conn.execute_batch("PRAGMA writable_schema=OFF")?;
        conn.execute_batch("VACUUM")
    }) {
        true if db_opens_cleanly(db_path).is_none() => {
            report.repaired = true;
            report.strategy = Some("drop_fts_rebuild".to_string());
        }
        true => {
            report.error = db_opens_cleanly(db_path);
        }
        false => {
            report.error = Some("all repair strategies failed to execute".to_string());
        }
    }
    report
}

/// Run a closure against a fresh autocommit connection, swallowing errors
/// into `false` so the caller can escalate to the next strategy.
fn with_conn<F>(db_path: &Path, f: F) -> bool
where
    F: FnOnce(&Connection) -> Result<(), rusqlite::Error>,
{
    match Connection::open(db_path) {
        Ok(conn) => f(&conn).is_ok(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn healthy_db(dir: &Path) -> PathBuf {
        let path = dir.join("state.db");
        let store = crate::session::sqlite::SqliteSessionStore::open(&path).unwrap();
        let sid = store.create_session("cli", None, None).unwrap();
        store
            .append_message(
                &sid,
                &crate::provider::Message {
                    role: crate::provider::Role::User,
                    content: Some("hello repair".into()),
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                },
            )
            .unwrap();
        drop(store);
        path
    }

    #[test]
    fn healthy_db_needs_no_repair() {
        let dir = tempfile::tempdir().unwrap();
        let path = healthy_db(dir.path());
        assert_eq!(db_opens_cleanly(&path), None);
        let report = repair_state_db_schema(&path, true);
        assert!(report.repaired);
        assert_eq!(report.strategy.as_deref(), Some("already_healthy"));
        assert!(report.backup_path.is_none(), "healthy DB takes no backup");
    }

    #[test]
    fn duplicate_fts_schema_is_deduplicated() {
        let dir = tempfile::tempdir().unwrap();
        let path = healthy_db(dir.path());
        // Inject a duplicate sqlite_master row for messages_fts by copying
        // the schema row — the classic "table messages_fts already exists"
        // corruption class.
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA writable_schema=ON").unwrap();
        conn.execute(
            "INSERT INTO sqlite_master (type, name, tbl_name, rootpage, sql)
             SELECT type, name, tbl_name, rootpage, sql FROM sqlite_master
             WHERE name = 'messages'",
            [],
        )
        .unwrap();
        conn.execute_batch("PRAGMA writable_schema=OFF").unwrap();
        drop(conn);

        let reason = db_opens_cleanly(&path);
        assert!(
            reason.is_some(),
            "duplicate schema row should trip the probe: {reason:?}"
        );
        let report = repair_state_db_schema(&path, true);
        assert!(report.repaired, "report: {report:?}");
        assert!(report.backup_path.is_some());
        assert!(matches!(
            report.strategy.as_deref(),
            Some("dedup_schema") | Some("rebuild_fts") | Some("reindex_btree")
        ));
        // Data survives: session + message still readable.
        let store = crate::session::sqlite::SqliteSessionStore::open(&path).unwrap();
        assert_eq!(store.count_sessions().unwrap(), 1);
        assert_eq!(store.count_messages().unwrap(), 1);
    }

    #[test]
    fn repair_rejects_missing_file_and_reports_backup() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.db");
        let report = repair_state_db_schema(&missing, true);
        assert!(!report.repaired);
        assert!(report.error.unwrap().contains("does not exist"));

        let path = healthy_db(dir.path());
        let backup = backup_db_file(&path).expect("backup copy");
        assert!(backup.exists());
        assert!(backup
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .contains(".bak."));
    }
}

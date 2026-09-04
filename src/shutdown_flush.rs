//! Shutdown flush — port of hermes `gateway/shutdown_flush.py`
//! (hermes issue #72680).
//!
//! Flushes data that would otherwise be lost on shutdown:
//!
//! 1. [`flush_parked_to_file`] — serialises parked follow-up messages
//!    (the busy-policy queues) that never got their turn before the
//!    process exits. hermes accumulates such messages in
//!    `_pending_messages` and loses them when shutdown clears the
//!    in-memory slots; ulnclaw's equivalent is the per-chat FIFO of
//!    queued [`crate::messaging::MessageEvent`]s inside the Dispatcher.
//!
//! 2. [`recover_parked_events`] — called on startup; reads the flush
//!    files back so the events can be re-dispatched, then deletes each
//!    file on success. Files that fail to parse are preserved for the
//!    next boot (hermes retry semantics).
//!
//! 3. [`flush_agent_history_to_file`] — best-effort dump of an
//!    in-memory transcript when persisting it to the session DB raises
//!    (hermes #72680 FTS-corruption mode). These snapshots use a
//!    distinct `reason` and are skipped by automatic recovery — they
//!    are meant for manual operator salvage after repairing the DB.
//!
//! Payloads land under `<home>/pending_messages/` as atomic,
//! uniquely-named, 0600-permission JSON files.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Marker `reason` for agent-history snapshots (manual operator
/// recovery only — never re-dispatched automatically).
pub const AGENT_HISTORY_REASON: &str = "shutdown-with-unpersisted-agent-history";

/// Return the pending-messages flush directory under `home`, creating
/// it with owner-only permissions (hermes `_get_flush_dir`).
pub fn flush_dir(home: &Path) -> std::io::Result<PathBuf> {
    let dir = home.join("pending_messages");
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).ok();
    }
    Ok(dir)
}

/// Persist a directory entry on platforms that support directory fsync
/// (hermes `_fsync_directory`).
#[cfg(unix)]
fn fsync_directory(path: &Path) {
    if let Ok(file) = std::fs::File::open(path) {
        let _ = file.sync_all();
    }
}

/// Atomically write one private recovery payload under `name` (hermes
/// `_write_payload`).
fn write_payload_named(dir: &Path, name: &str, payload: &Value) -> std::io::Result<PathBuf> {
    let final_path = dir.join(format!("{name}.json"));
    let tmp_path = dir.join(format!(".{name}.tmp"));
    let bytes = serde_json::to_vec(payload)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&tmp_path, &bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o600)).ok();
    }
    std::fs::rename(&tmp_path, &final_path)?;
    #[cfg(unix)]
    fsync_directory(dir);
    Ok(final_path)
}

/// Serialise parked follow-up events to disk (hermes
/// `flush_pending_to_file`). Returns the number of events flushed.
pub fn flush_parked_to_file(
    home: &Path,
    parked: &[(String, crate::messaging::MessageEvent)],
    reason: &str,
) -> usize {
    if parked.is_empty() {
        return 0;
    }
    let dir = match flush_dir(home) {
        Ok(dir) => dir,
        Err(e) => {
            tracing::warn!("[shutdown_flush] cannot create flush dir: {e}");
            return 0;
        }
    };
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut flushed = 0usize;
    for (idx, (session_key, event)) in parked.iter().enumerate() {
        let payload = json!({
            "session_key": session_key,
            "reason": reason,
            "ts": ts,
            "data": {
                "platform": event.platform,
                "chat_id": event.chat_id,
                "sender_id": event.sender_id,
                "sender_name": event.sender_name,
                "text": event.text,
                "message_id": event.message_id,
            },
        });
        // Timestamp + batch index keep recovery FIFO within a flush
        // batch (the sorted-glob replay order matters for re-dispatch;
        // hermes could append to the DB out of order, ulnclaw re-runs).
        let name = format!(
            "pending-{ts}-{idx:04}-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        );
        match write_payload_named(&dir, &name, &payload) {
            Ok(_) => flushed += 1,
            Err(e) => {
                tracing::debug!(
                    "[shutdown_flush] failed to flush parked message for {session_key}: {e}"
                );
            }
        }
    }
    if flushed > 0 {
        tracing::info!(
            "[shutdown_flush] flushed {flushed} parked message(s) to {} (reason={reason})",
            dir.display()
        );
    }
    flushed
}

/// Read recovery payloads back on startup (hermes
/// `recover_pending_to_db`): returns `(session_key, event)` pairs for
/// every structurally valid flush file, deleting each file as it is
/// consumed. Agent-history snapshots are skipped silently (manual
/// recovery only); structurally invalid files are preserved for the
/// next boot.
pub fn recover_parked_events(home: &Path) -> Vec<(String, crate::messaging::MessageEvent)> {
    let dir = match flush_dir(home) {
        Ok(dir) => dir,
        Err(_) => return Vec::new(),
    };
    let mut files: Vec<PathBuf> = match std::fs::read_dir(&dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
            .collect(),
        Err(_) => return Vec::new(),
    };
    files.sort();
    let mut recovered = Vec::new();
    for path in files {
        let payload: Value = match std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
        {
            Some(v) => v,
            None => {
                tracing::warn!(
                    "[shutdown_flush] preserving unreadable flush file {}",
                    path.display()
                );
                continue;
            }
        };
        if payload.get("reason").and_then(Value::as_str) == Some(AGENT_HISTORY_REASON) {
            continue;
        }
        let session_key = payload
            .get("session_key")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let data = payload.get("data").cloned().unwrap_or(Value::Null);
        let text = data.get("text").and_then(Value::as_str).unwrap_or("");
        if session_key.is_empty() || text.is_empty() {
            tracing::warn!(
                "[shutdown_flush] cannot recover structurally invalid pending message from {}; the flush file has been preserved",
                path.display()
            );
            continue;
        }
        let event = crate::messaging::MessageEvent {
            platform: data
                .get("platform")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            chat_id: data
                .get("chat_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            sender_id: data
                .get("sender_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            sender_name: data
                .get("sender_name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            text: text.to_string(),
            message_id: data
                .get("message_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            attachments: Vec::new(),
        };
        recovered.push((session_key, event));
        std::fs::remove_file(&path).ok();
    }
    if !recovered.is_empty() {
        tracing::info!(
            "[shutdown_flush] recovered {} parked message(s) from shutdown flush",
            recovered.len()
        );
    }
    recovered
}

/// Best-effort dump of an in-memory transcript before teardown (hermes
/// `flush_agent_history_to_file`). Used when persisting the transcript
/// to the session DB raises (FTS/SQLite corruption, hermes #72680):
/// serialise to an atomic JSON file outside the broken DB so an
/// operator can salvage the conversation after repairing it. Failures
/// are swallowed — shutdown must never block on a best-effort backup.
pub fn flush_agent_history_to_file(home: &Path, session_id: Option<&str>, history: &[Value]) {
    if history.is_empty() {
        return;
    }
    let result = flush_dir(home).and_then(|dir| {
        let name = format!("pending-{}", uuid::Uuid::new_v4().simple());
        write_payload_named(
            &dir,
            &name,
            &json!({
                "reason": AGENT_HISTORY_REASON,
                "issue": "#72680",
                "session_id": session_id,
                "count": history.len(),
                "messages": history,
            }),
        )
    });
    match result {
        Ok(path) => tracing::warn!(
            "[shutdown_flush] preserved {} in-memory message(s) for session {} at {} (possible DB corruption — recover after repairing)",
            history.len(),
            session_id.unwrap_or("?"),
            path.display()
        ),
        Err(e) => tracing::warn!(
            "[shutdown_flush] agent-history shutdown preservation failed for session {}: {e}",
            session_id.unwrap_or("?")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messaging::MessageEvent;

    fn event(text: &str) -> MessageEvent {
        MessageEvent {
            platform: "telegram".into(),
            chat_id: "42".into(),
            sender_id: "u1".into(),
            sender_name: "User One".into(),
            text: text.into(),
            message_id: "m1".into(),
            attachments: Vec::new(),
        }
    }

    #[test]
    fn test_flush_and_recover_roundtrip() {
        let temp = tempfile::tempdir().unwrap();
        let parked = vec![
            ("platform-telegram-42".to_string(), event("first parked")),
            ("platform-telegram-42".to_string(), event("second parked")),
        ];
        assert_eq!(flush_parked_to_file(temp.path(), &parked, "shutdown"), 2);
        let recovered = recover_parked_events(temp.path());
        assert_eq!(recovered.len(), 2);
        assert_eq!(recovered[0].0, "platform-telegram-42");
        assert_eq!(recovered[0].1.text, "first parked");
        assert_eq!(recovered[1].1.text, "second parked");
        assert_eq!(recovered[0].1.platform, "telegram");
        // Files are deleted once consumed.
        assert!(recover_parked_events(temp.path()).is_empty());
    }

    #[test]
    fn test_empty_parked_flushes_nothing() {
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(flush_parked_to_file(temp.path(), &[], "shutdown"), 0);
        assert!(!temp.path().join("pending_messages").exists());
    }

    #[test]
    fn test_agent_history_skipped_by_recovery() {
        let temp = tempfile::tempdir().unwrap();
        flush_agent_history_to_file(
            temp.path(),
            Some("20260809_120000_abc"),
            &[json!({"role": "user", "content": "hi"})],
        );
        // Recovery skips history snapshots silently.
        assert!(recover_parked_events(temp.path()).is_empty());
        // The snapshot file stays for manual operator recovery.
        let dir = flush_dir(temp.path()).unwrap();
        let count = std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_invalid_payload_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let dir = flush_dir(temp.path()).unwrap();
        std::fs::write(dir.join("pending-bad.json"), "{ not json").unwrap();
        // Structurally valid one recovers; the bad one is preserved.
        let parked = vec![("platform-telegram-42".to_string(), event("ok"))];
        assert_eq!(flush_parked_to_file(temp.path(), &parked, "shutdown"), 1);
        let recovered = recover_parked_events(temp.path());
        assert_eq!(recovered.len(), 1);
        let remaining: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(remaining.len(), 1);
        assert!(remaining[0].file_name().to_string_lossy().contains("bad"));
    }

    #[test]
    fn test_missing_text_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let dir = flush_dir(temp.path()).unwrap();
        std::fs::write(
            dir.join("pending-empty.json"),
            r#"{"session_key": "platform-telegram-42", "reason": "shutdown", "ts": 0, "data": {"text": ""}}"#,
        )
        .unwrap();
        assert!(recover_parked_events(temp.path()).is_empty());
        assert!(dir.join("pending-empty.json").exists());
    }

    #[test]
    fn test_flush_dir_permissions() {
        let temp = tempfile::tempdir().unwrap();
        let dir = flush_dir(temp.path()).unwrap();
        assert!(dir.is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700);
        }
    }

    #[test]
    fn test_empty_history_not_written() {
        let temp = tempfile::tempdir().unwrap();
        flush_agent_history_to_file(temp.path(), Some("s"), &[]);
        assert!(!temp.path().join("pending_messages").exists());
    }
}

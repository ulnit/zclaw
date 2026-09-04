//! Session mirroring for out-of-band delivery (hermes
//! `gateway/mirror.py` parity).
//!
//! When a cron job delivers its final response to a platform chat, the
//! delivered text is otherwise invisible to the receiving-side agent:
//! it exists only in the cron job's own run session, so the user's
//! next reply in that chat ("what is Task #2?") has no context. This
//! module appends a delivery-mirror record to the target chat's
//! session transcript so the conversation continues coherently.
//!
//! Scope follows hermes exactly:
//!
//! * Mirrors only ever APPEND to a session that already exists — a
//!   missing session means the target was never the origin
//!   conversation, so mirroring is a silent no-op (never a synthetic
//!   session).
//! * The mirror lands as a USER turn with a labelled prefix, not an
//!   assistant turn: the brief is not the agent speaking, and an
//!   assistant-role mirror would produce `assistant → assistant`
//!   pairs that break strict-alternation providers (hermes issue
//!   #2221). A user-role mirror collapses safely via the consecutive
//!   user-merge on replay.
//! * Everything is best-effort: a delivery that succeeded must never
//!   be reported failed because the transcript mirror hit a problem.

use crate::provider::{Message, Role};
use crate::session::SqliteSessionStore;

/// Append a delivery-mirror message to the target chat's session
/// transcript (hermes `mirror_to_session`).
///
/// The session id resolves through the same deterministic key +
/// resume-remap the dispatcher uses (`platform-{platform}-{chat_id}`
/// → [`crate::messaging::effective_session_id_for`]), so a chat that
/// was handed off to another session mirrors into that session.
///
/// Returns true when mirrored, false when no matching session exists
/// or any error occurs — never fatal.
pub fn mirror_to_session(
    store: &SqliteSessionStore,
    platform: &str,
    chat_id: &str,
    message_text: &str,
    source_label: &str,
    role: Role,
) -> bool {
    let text = message_text.trim();
    if text.is_empty() || chat_id.trim().is_empty() {
        return false;
    }
    let session_key = format!("platform-{platform}-{chat_id}");
    let session_id = crate::messaging::effective_session_id_for(&session_key);
    // Only append to sessions that already exist — hermes mirror never
    // creates a synthetic session (cold start = silent no-op).
    let exists = store
        .resolve_session_id(&session_id)
        .ok()
        .flatten()
        .is_some();
    if !exists {
        tracing::debug!(
            "[mirror] no session found for {platform}:{chat_id} (cold start) — skipping"
        );
        return false;
    }
    let message = Message {
        role,
        content: Some(text.to_string()),
        tool_calls: None,
        tool_call_id: None,
        name: None,
    };
    match store.append_message(&session_id, &message) {
        Ok(()) => {
            tracing::debug!("[mirror] wrote to session {session_id} (from {source_label})");
            true
        }
        Err(e) => {
            tracing::debug!(
                "[mirror] append failed for {platform}:{chat_id} session {session_id}: {e}"
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn temp_store() -> (tempfile::TempDir, Arc<SqliteSessionStore>) {
        let dir = tempfile::tempdir().unwrap();
        let store =
            Arc::new(SqliteSessionStore::open(dir.path().join("state.db")).expect("store opens"));
        (dir, store)
    }

    #[test]
    fn mirror_appends_to_existing_session() {
        let (_dir, store) = temp_store();
        store
            .create_named_session("platform-testplat-chat-1", "platform:testplat", None, None)
            .unwrap();
        let ok = mirror_to_session(
            &store,
            "testplat",
            "chat-1",
            "[Cron delivery: morning brief]\nThe brief.",
            "cron",
            Role::User,
        );
        assert!(ok);
        let messages = store.load_messages("platform-testplat-chat-1").unwrap();
        assert_eq!(messages.len(), 1);
        assert!(matches!(messages[0].role, Role::User));
        assert!(messages[0]
            .content
            .as_deref()
            .unwrap()
            .starts_with("[Cron delivery: morning brief]"));
    }

    #[test]
    fn mirror_without_session_is_a_noop() {
        let (_dir, store) = temp_store();
        let ok = mirror_to_session(
            &store,
            "testplat",
            "chat-never-seen",
            "text",
            "cron",
            Role::User,
        );
        assert!(!ok);
        // No synthetic session was created.
        assert_eq!(store.count_sessions().unwrap(), 0);
    }

    #[test]
    fn mirror_rejects_empty_inputs() {
        let (_dir, store) = temp_store();
        store
            .create_named_session("platform-testplat-chat-2", "platform:testplat", None, None)
            .unwrap();
        assert!(!mirror_to_session(
            &store,
            "testplat",
            "chat-2",
            "   ",
            "cron",
            Role::User
        ));
        assert!(!mirror_to_session(
            &store,
            "testplat",
            "",
            "text",
            "cron",
            Role::User
        ));
    }

    #[test]
    fn mirror_assistant_role_is_supported() {
        let (_dir, store) = temp_store();
        store
            .create_named_session("platform-testplat-chat-3", "platform:testplat", None, None)
            .unwrap();
        let ok = mirror_to_session(
            &store,
            "testplat",
            "chat-3",
            "mirrored assistant text",
            "send_message",
            Role::Assistant,
        );
        assert!(ok);
        let messages = store.load_messages("platform-testplat-chat-3").unwrap();
        assert!(matches!(messages[0].role, Role::Assistant));
    }
}

//! Persistent registry of confirmed-dead delivery targets (hermes
//! `gateway/dead_targets.py` parity).
//!
//! When a messaging platform reports that a target chat is permanently
//! gone — a deleted group (`Forbidden: the group chat was deleted`), a
//! bot kicked/blocked, or a deactivated user — re-sending to it on
//! every cron tick or fan-out delivery wastes a send attempt against
//! the platform's flood-control envelope and spams the logs. This
//! registry lets the delivery layer short-circuit a target it has
//! already proven dead, while staying self-healing: any successful
//! send to that target clears the flag, so a user who re-adds the bot
//! (or restores the chat) recovers automatically with no manual
//! cleanup.
//!
//! Scope is deliberately narrow. Only *whole-chat* deaths are recorded
//! — the `forbidden` and chat-level `not_found` ("chat not found")
//! error kinds. Thread/topic-level not_found is NOT recorded here: a
//! deleted topic does not mean the parent chat is dead. The error text
//! is classified with the same platform-neutral substring rules hermes
//! uses (`classify_send_error` / `is_chat_level_not_found` from
//! `gateway/platforms/base.py`).
//!
//! The store is a small JSON file under the active home
//! (`<home>/gateway/dead_targets.json`). Reads/writes are best-effort:
//! a corrupt or unwritable file degrades to an in-memory-only registry
//! rather than failing on the delivery path.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Reason text is capped before persistence (hermes `[:200]`).
pub const MAX_REASON_LEN: usize = 200;
/// Error-text tail kept inside the mark reason (hermes `[:120]`).
const REASON_ERROR_TAIL_LEN: usize = 120;

/// Error kinds that mean the *whole chat* is unreachable, not a
/// transient or thread-level problem (hermes `_DEAD_ERROR_KINDS`).
const DEAD_ERROR_KINDS: &[&str] = &["forbidden", "not_found"];

/// not_found substrings split by blast radius (hermes
/// `_CHAT_LEVEL_NOT_FOUND_SUBSTRINGS` / `_SUBCHAT_NOT_FOUND_SUBSTRINGS`).
/// A chat-level not_found means the chat/user/group itself is gone, so
/// the whole target is dead. A sub-chat not_found (deleted forum
/// topic, edited-away message) leaves the parent chat reachable and
/// must NOT mark the whole chat dead.
const CHAT_LEVEL_NOT_FOUND_SUBSTRINGS: &[&str] = &["chat not found"];
const SUBCHAT_NOT_FOUND_SUBSTRINGS: &[&str] = &[
    "message to edit not found",
    "message to reply not found",
    "thread not found",
    "topic_deleted",
    "message_id_invalid",
];

/// Network-failure markers that classify as transient/retryable
/// (hermes `_RETRYABLE_ERROR_PATTERNS`).
const RETRYABLE_ERROR_PATTERNS: &[&str] = &[
    "connecterror",
    "connectionerror",
    "connectionreset",
    "connectionrefused",
    "connecttimeout",
    "network",
    "broken pipe",
    "remotedisconnected",
    "eoferror",
];

/// Platform-neutral send-error classification (hermes
/// `classify_send_error`): maps the lowercased error text onto the
/// hermes `SEND_ERROR_KINDS` values. Conservative — anything
/// unrecognized returns `"unknown"` so callers never mistake an
/// unclassified failure for a benign one.
pub fn classify_send_error(error_text: &str) -> &'static str {
    let blob = error_text.to_lowercase();
    if blob.trim().is_empty() {
        return "unknown";
    }
    if blob.contains("message_too_long") || blob.contains("too long") {
        return "too_long";
    }
    if blob.contains("can't parse entities")
        || blob.contains("cant parse entities")
        || blob.contains("can't find end")
        || blob.contains("unsupported start tag")
        || (blob.contains("entity") && blob.contains("parse"))
        || (blob.contains("bad request") && blob.contains("entit"))
    {
        return "bad_format";
    }
    if blob.contains("forbidden")
        || blob.contains("bot was blocked")
        || blob.contains("blocked by the user")
        || blob.contains("user is deactivated")
        || blob.contains("not enough rights")
        || blob.contains("have no rights")
        || blob.contains("not a member")
    {
        return "forbidden";
    }
    if CHAT_LEVEL_NOT_FOUND_SUBSTRINGS
        .iter()
        .any(|s| blob.contains(s))
        || SUBCHAT_NOT_FOUND_SUBSTRINGS
            .iter()
            .any(|s| blob.contains(s))
    {
        return "not_found";
    }
    if blob.contains("flood")
        || blob.contains("too many requests")
        || blob.contains("retry after")
        || blob.contains("rate limit")
    {
        return "rate_limited";
    }
    if RETRYABLE_ERROR_PATTERNS.iter().any(|p| blob.contains(p)) {
        return "transient";
    }
    "unknown"
}

/// Whether a `not_found` failure means the *whole chat* is gone
/// (hermes `is_chat_level_not_found`). When both a chat-level and a
/// sub-chat marker are present, the sub-chat reading wins
/// (conservative: never kill a chat that may still be reachable).
pub fn is_chat_level_not_found(error_text: &str) -> bool {
    let blob = error_text.to_lowercase();
    if SUBCHAT_NOT_FOUND_SUBSTRINGS
        .iter()
        .any(|s| blob.contains(s))
    {
        return false;
    }
    CHAT_LEVEL_NOT_FOUND_SUBSTRINGS
        .iter()
        .any(|s| blob.contains(s))
}

/// True when `kind` denotes a permanent whole-chat death (hermes
/// `DeadTargetRegistry.is_dead_error_kind`).
pub fn is_dead_error_kind(kind: &str) -> bool {
    !kind.is_empty() && DEAD_ERROR_KINDS.contains(&kind)
}

/// Best-effort dead-target classification from a send error's text
/// (hermes `_classify_dead_from_error_text`): recovers the error kind,
/// keeps only dead kinds, and for `not_found` requires the chat-level
/// reading. Returns the kind or None when the error must not mark the
/// target dead.
pub fn classify_dead_from_error_text(error_text: &str) -> Option<&'static str> {
    if error_text.is_empty() {
        return None;
    }
    let kind = classify_send_error(error_text);
    if !is_dead_error_kind(kind) {
        return None;
    }
    if kind == "not_found" && !is_chat_level_not_found(error_text) {
        return None;
    }
    Some(kind)
}

/// Canonical key for a (platform, chat_id) pair (hermes `_normalize`).
fn normalize(platform: &str, chat_id: &str) -> String {
    format!("{}:{}", platform.trim().to_lowercase(), chat_id.trim())
}

/// Thread-safe, persistent set of confirmed-dead delivery targets
/// (hermes `DeadTargetRegistry`). Keyed on `platform:chat_id`; stores
/// the reason and a timestamp for observability. Self-healing:
/// [`DeadTargetRegistry::revive`] (hermes `clear`, called on a
/// successful send) removes the flag.
pub struct DeadTargetRegistry {
    path: Option<PathBuf>,
    dead: Mutex<HashMap<String, Value>>,
}

impl DeadTargetRegistry {
    /// Build a registry persisted at `path` (or in-memory-only when
    /// None). Loading is best-effort: a corrupt file starts empty.
    pub fn new(path: Option<PathBuf>) -> Self {
        let dead = match &path {
            Some(path) if path.exists() => {
                match std::fs::read_to_string(path)
                    .ok()
                    .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                {
                    Some(Value::Object(map)) => {
                        map.into_iter().filter(|(_, v)| v.is_object()).collect()
                    }
                    _ => HashMap::new(),
                }
            }
            _ => HashMap::new(),
        };
        Self {
            path,
            dead: Mutex::new(dead),
        }
    }

    /// Persistence target (None = in-memory-only registry).
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Whether a target is currently marked dead. Empty chat ids are
    /// never dead-tracked (hermes: LOCAL / origin-without-chat).
    pub fn is_dead(&self, platform: &str, chat_id: &str) -> bool {
        if chat_id.trim().is_empty() {
            return false;
        }
        self.dead
            .lock()
            .map(|dead| dead.contains_key(&normalize(platform, chat_id)))
            .unwrap_or(false)
    }

    /// Record a target as confirmed-dead; returns true when newly
    /// added (hermes `mark_dead`). Reason is capped at
    /// [`MAX_REASON_LEN`] chars.
    pub fn mark_dead(&self, platform: &str, chat_id: &str, reason: &str) -> bool {
        if chat_id.trim().is_empty() {
            return false;
        }
        let key = normalize(platform, chat_id);
        let newly_added = match self.dead.lock() {
            Ok(mut dead) => {
                let existed = dead.contains_key(&key);
                dead.insert(
                    key.clone(),
                    json!({
                        "platform": platform.trim().to_lowercase(),
                        "chat_id": chat_id.trim(),
                        "reason": reason.chars().take(MAX_REASON_LEN).collect::<String>(),
                        "marked_at": now_secs(),
                    }),
                );
                self.flush(&dead);
                !existed
            }
            Err(_) => return false,
        };
        if newly_added {
            tracing::info!(
                "[dead_targets] marked {key} as unreachable ({}) — future deliveries \
                 to this target will be skipped until a send succeeds",
                if reason.is_empty() {
                    "no reason given"
                } else {
                    reason
                }
            );
        }
        newly_added
    }

    /// Remove a target's dead flag on a successful send
    /// (self-healing; hermes `clear`). Returns true when a flag was
    /// actually cleared.
    pub fn revive(&self, platform: &str, chat_id: &str) -> bool {
        if chat_id.trim().is_empty() {
            return false;
        }
        let key = normalize(platform, chat_id);
        let cleared = match self.dead.lock() {
            Ok(mut dead) => {
                if dead.remove(&key).is_some() {
                    self.flush(&dead);
                    true
                } else {
                    false
                }
            }
            Err(_) => return false,
        };
        if cleared {
            tracing::info!("[dead_targets] cleared {key} (delivery succeeded again)");
        }
        cleared
    }

    /// Snapshot of the current dead set (for diagnostics / ops
    /// surfaces; hermes `all_dead`), sorted by key for stable output.
    pub fn list(&self) -> Vec<(String, Value)> {
        let mut rows: Vec<(String, Value)> = self
            .dead
            .lock()
            .map(|dead| dead.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default();
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        rows
    }

    /// Best-effort atomic persist (tmp + rename). Failures keep the
    /// in-memory state and never propagate to the delivery path.
    fn flush(&self, dead: &HashMap<String, Value>) {
        let Some(path) = &self.path else { return };
        let mut map = serde_json::Map::new();
        for (key, value) in dead {
            map.insert(key.clone(), value.clone());
        }
        let result = (|| -> std::io::Result<()> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_string_pretty(&Value::Object(map))?)?;
            std::fs::rename(&tmp, path)?;
            Ok(())
        })();
        if let Err(e) = result {
            tracing::debug!("[dead_targets] could not persist {}: {e}", path.display());
        }
    }
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Process-wide registry (hermes constructs one per `DeliveryService`;
/// the gateway has exactly one). Path resolves to
/// `<home>/gateway/dead_targets.json` under the active ulnclaw home;
/// when the home cannot be resolved the registry degrades to
/// in-memory-only.
pub fn registry() -> &'static DeadTargetRegistry {
    static REGISTRY: OnceLock<DeadTargetRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let path = crate::config::ensure_home()
            .ok()
            .map(|home| home.join("gateway").join("dead_targets.json"));
        DeadTargetRegistry::new(path)
    })
}

/// Whether a target is currently marked dead (process-wide registry).
pub fn is_dead(platform: &str, chat_id: &str) -> bool {
    registry().is_dead(platform, chat_id)
}

/// Record a target as confirmed-dead (process-wide registry). Returns
/// true when newly added.
pub fn mark_dead(platform: &str, chat_id: &str, reason: &str) -> bool {
    registry().mark_dead(platform, chat_id, reason)
}

/// Clear a target's dead flag after a successful send (process-wide
/// registry). Returns true when a flag was actually cleared.
pub fn revive(platform: &str, chat_id: &str) -> bool {
    registry().revive(platform, chat_id)
}

/// Classify a send error's text and, when it proves a whole-chat
/// death, mark the target dead with reason `"{kind}: {text[:120]}"`
/// (hermes `deliver()` exception path). Returns true when newly added.
pub fn mark_dead_from_error(platform: &str, chat_id: &str, error_text: &str) -> bool {
    let Some(kind) = classify_dead_from_error_text(error_text) else {
        return false;
    };
    let tail: String = error_text.chars().take(REASON_ERROR_TAIL_LEN).collect();
    registry().mark_dead(platform, chat_id, &format!("{kind}: {tail}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_registry() -> (tempfile::TempDir, DeadTargetRegistry) {
        let dir = tempfile::tempdir().unwrap();
        let reg =
            DeadTargetRegistry::new(Some(dir.path().join("gateway").join("dead_targets.json")));
        (dir, reg)
    }

    #[test]
    fn mark_is_dead_and_revive_cycle() {
        let (_dir, reg) = temp_registry();
        assert!(!reg.is_dead("telegram", "42"));
        assert!(reg.mark_dead("Telegram", " 42 ", "group deleted"));
        // Normalization: case/space-insensitive platform + chat id.
        assert!(reg.is_dead("telegram", "42"));
        assert!(reg.is_dead("TELEGRAM", "42"));
        // Re-marking is idempotent (not newly added).
        assert!(!reg.mark_dead("telegram", "42", "again"));
        assert!(reg.revive("telegram", "42"));
        assert!(!reg.is_dead("telegram", "42"));
        assert!(!reg.revive("telegram", "42"));
    }

    #[test]
    fn empty_chat_id_is_never_tracked() {
        let (_dir, reg) = temp_registry();
        assert!(!reg.mark_dead("telegram", "", "x"));
        assert!(!reg.mark_dead("telegram", "   ", "x"));
        assert!(!reg.is_dead("telegram", ""));
        assert!(!reg.revive("telegram", ""));
    }

    #[test]
    fn entries_persist_and_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gateway").join("dead_targets.json");
        let reg = DeadTargetRegistry::new(Some(path.clone()));
        reg.mark_dead("discord", "c1", "Forbidden: bot was kicked");
        assert!(path.exists());

        // A fresh registry at the same path sees the persisted set.
        let reloaded = DeadTargetRegistry::new(Some(path));
        assert!(reloaded.is_dead("discord", "c1"));
        let rows = reloaded.list();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "discord:c1");
        assert_eq!(rows[0].1["platform"], "discord");
        assert_eq!(rows[0].1["chat_id"], "c1");
        assert_eq!(rows[0].1["reason"], "Forbidden: bot was kicked");
        assert!(rows[0].1["marked_at"].as_f64().unwrap() > 0.0);
    }

    #[test]
    fn corrupt_file_degrades_to_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dead_targets.json");
        std::fs::write(&path, "{not json").unwrap();
        let reg = DeadTargetRegistry::new(Some(path.clone()));
        assert!(reg.list().is_empty());
        // Still writable afterwards.
        reg.mark_dead("slack", "C1", "chat not found");
        let reloaded = DeadTargetRegistry::new(Some(path));
        assert!(reloaded.is_dead("slack", "C1"));
    }

    #[test]
    fn malformed_entries_are_dropped_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dead_targets.json");
        std::fs::write(
            &path,
            r#"{"telegram:1": {"platform": "telegram", "chat_id": "1"}, "bad": 42}"#,
        )
        .unwrap();
        let reg = DeadTargetRegistry::new(Some(path));
        assert_eq!(reg.list().len(), 1);
        assert!(reg.is_dead("telegram", "1"));
    }

    #[test]
    fn in_memory_registry_works_without_path() {
        let reg = DeadTargetRegistry::new(None);
        assert!(reg.path().is_none());
        reg.mark_dead("matrix", "!room", "forbidden");
        assert!(reg.is_dead("matrix", "!room"));
        reg.revive("matrix", "!room");
        assert!(!reg.is_dead("matrix", "!room"));
    }

    #[test]
    fn reason_is_capped() {
        let (_dir, reg) = temp_registry();
        let long = "x".repeat(500);
        reg.mark_dead("telegram", "1", &long);
        let rows = reg.list();
        assert_eq!(rows[0].1["reason"].as_str().unwrap().len(), MAX_REASON_LEN);
    }

    #[test]
    fn classifier_kinds_match_hermes() {
        assert_eq!(classify_send_error(""), "unknown");
        assert_eq!(classify_send_error("Message is too long"), "too_long");
        assert_eq!(classify_send_error("message_too_long"), "too_long");
        assert_eq!(
            classify_send_error("Bad Request: can't parse entities"),
            "bad_format"
        );
        assert_eq!(
            classify_send_error("Forbidden: the group chat was deleted"),
            "forbidden"
        );
        assert_eq!(
            classify_send_error("bot was blocked by the user"),
            "forbidden"
        );
        assert_eq!(classify_send_error("user is deactivated"), "forbidden");
        assert_eq!(classify_send_error("Chat not found"), "not_found");
        assert_eq!(
            classify_send_error("message to reply not found"),
            "not_found"
        );
        assert_eq!(
            classify_send_error("Too Many Requests: retry after 3"),
            "rate_limited"
        );
        assert_eq!(
            classify_send_error("ConnectionResetError happened"),
            "transient"
        );
        assert_eq!(classify_send_error("broken pipe"), "transient");
        assert_eq!(classify_send_error("something exploded"), "unknown");
    }

    #[test]
    fn chat_level_not_found_is_conservative() {
        assert!(is_chat_level_not_found("Chat not found"));
        assert!(!is_chat_level_not_found("message to reply not found"));
        assert!(!is_chat_level_not_found("topic_deleted"));
        // Both markers present: sub-chat reading wins.
        assert!(!is_chat_level_not_found("chat not found; topic_deleted"));
        assert!(!is_chat_level_not_found("unrelated"));
    }

    #[test]
    fn dead_kind_filter() {
        assert!(is_dead_error_kind("forbidden"));
        assert!(is_dead_error_kind("not_found"));
        assert!(!is_dead_error_kind("rate_limited"));
        assert!(!is_dead_error_kind("transient"));
        assert!(!is_dead_error_kind(""));
    }

    #[test]
    fn classify_dead_from_error_text_rules() {
        assert_eq!(
            classify_dead_from_error_text("Forbidden: the group chat was deleted"),
            Some("forbidden")
        );
        assert_eq!(
            classify_dead_from_error_text("Chat not found"),
            Some("not_found")
        );
        // Thread/topic-level not_found must NOT mark the chat dead.
        assert_eq!(
            classify_dead_from_error_text("message to reply not found"),
            None
        );
        assert_eq!(classify_dead_from_error_text("thread not found"), None);
        // Transient / rate-limit / unknown never mark dead.
        assert_eq!(classify_dead_from_error_text("retry after 5"), None);
        assert_eq!(classify_dead_from_error_text("ConnectionReset"), None);
        assert_eq!(classify_dead_from_error_text("boom"), None);
        assert_eq!(classify_dead_from_error_text(""), None);
    }

    #[test]
    fn mark_dead_from_error_builds_kind_prefixed_reason() {
        let (_dir, reg) = temp_registry();
        let kind =
            classify_dead_from_error_text("Forbidden: bot was kicked from the group chat").unwrap();
        let tail = "Forbidden: bot was kicked from the group chat";
        reg.mark_dead("telegram", "9", &format!("{kind}: {tail}"));
        let rows = reg.list();
        assert!(rows[0].1["reason"]
            .as_str()
            .unwrap()
            .starts_with("forbidden: Forbidden: bot was kicked"));
    }
}

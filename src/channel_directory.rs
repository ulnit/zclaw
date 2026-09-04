//! Channel directory (P259) — port of hermes `gateway/channel_directory.py`:
//! a persistent registry of the messaging channels the adapters have seen,
//! keyed by platform. Powers `send_message` target resolution (human-friendly
//! names → chat ids), the `list` action, and reaction `message_id` recall.
//! Stored as `<ulnclaw_home>/channel_directory.json`:
//!
//! ```json
//! { "platforms": { "telegram": [ { "id": "…", "name": "…",
//!   "type": "group|dm|channel|", "updated_at": 1712345678 } ] } }
//! ```
//!
//! A user-maintained friendly-name overlay (`channel_aliases.json`, hermes
//! `CHANNEL_ALIASES_PATH`) is re-applied on every load and save so hand
//! edits survive directory rebuilds and can pre-name chats before their
//! first message.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelEntry {
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// `group` / `dm` / `channel` — empty when the adapter didn't know.
    #[serde(default, rename = "type")]
    pub chat_type: String,
    #[serde(default)]
    pub updated_at: i64,
    /// Last inbound message id seen in this chat — lets
    /// `send_message(action='react')` omit `message_id` and target the
    /// most recent message (hermes react handler semantics).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub last_message_id: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Directory {
    #[serde(default)]
    platforms: BTreeMap<String, Vec<ChannelEntry>>,
}

fn state() -> &'static Mutex<Directory> {
    static STATE: OnceLock<Mutex<Directory>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(load_from_disk()))
}

pub fn directory_path() -> std::path::PathBuf {
    crate::config::ulnclaw_home().join("channel_directory.json")
}

/// hermes `CHANNEL_ALIASES_PATH` — `{"platform": {"chat_id": "friendly"}}`.
pub fn aliases_path() -> std::path::PathBuf {
    crate::config::ulnclaw_home().join("channel_aliases.json")
}

fn load_aliases() -> BTreeMap<String, BTreeMap<String, String>> {
    std::fs::read_to_string(aliases_path())
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// hermes `_apply_channel_aliases`: rename matching entries, inject
/// placeholder entries for aliased ids not discovered yet.
fn apply_aliases(dir: &mut Directory) {
    for (platform, id_map) in load_aliases() {
        let entries = dir.platforms.entry(platform.to_lowercase()).or_default();
        for (chat_id, friendly) in id_map {
            let friendly = friendly.trim().to_string();
            if friendly.is_empty() {
                continue;
            }
            if let Some(entry) = entries.iter_mut().find(|entry| entry.id == chat_id) {
                entry.name = friendly;
            } else {
                let chat_type = if chat_id.ends_with("@g.us") {
                    "group"
                } else {
                    "dm"
                };
                entries.push(ChannelEntry {
                    id: chat_id,
                    name: friendly,
                    chat_type: chat_type.to_string(),
                    updated_at: 0,
                    last_message_id: String::new(),
                });
            }
        }
    }
}

/// Test hook: reload the in-memory directory from the current
/// `ULNCLAW_HOME` (the static outlives any single test's temp home, and
/// tests share the process).
#[cfg(test)]
pub fn reset_for_tests() {
    *state().lock().unwrap() = load_from_disk();
}

fn load_from_disk() -> Directory {
    let mut dir = std::fs::read_to_string(directory_path())
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    apply_aliases(&mut dir);
    dir
}

fn save(dir: &mut Directory) {
    apply_aliases(dir);
    let path = directory_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(raw) = serde_json::to_string_pretty(dir) {
        let _ = std::fs::write(path, raw);
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Upsert a channel observation (adapters call this on every inbound
/// event through the dispatcher). Non-empty names refresh stale ones;
/// `updated_at` always advances; the inbound message id is remembered
/// for reaction targeting.
/// Recorded chat type for a platform/chat, if any (P718 scope
/// derivation for slash-access gating; empty/unset → `None`).
pub fn chat_type_for(platform: &str, id: &str) -> Option<String> {
    let dir = state().lock().unwrap();
    let entries = dir.platforms.get(&platform.to_lowercase())?;
    entries
        .iter()
        .find(|entry| entry.id == id)
        .map(|entry| entry.chat_type.clone())
        .filter(|chat_type| !chat_type.is_empty())
}

pub fn record_channel(platform: &str, id: &str, name: &str, chat_type: &str, message_id: &str) {
    if platform.is_empty() || id.is_empty() {
        return;
    }
    let mut dir = state().lock().unwrap();
    let entries = dir.platforms.entry(platform.to_lowercase()).or_default();
    if let Some(entry) = entries.iter_mut().find(|entry| entry.id == id) {
        if !name.is_empty() {
            entry.name = name.to_string();
        }
        if !chat_type.is_empty() {
            entry.chat_type = chat_type.to_string();
        }
        if !message_id.is_empty() {
            entry.last_message_id = message_id.to_string();
        }
        entry.updated_at = now_secs();
    } else {
        entries.push(ChannelEntry {
            id: id.to_string(),
            name: name.to_string(),
            chat_type: chat_type.to_string(),
            updated_at: now_secs(),
            last_message_id: message_id.to_string(),
        });
    }
    save(&mut dir);
}

/// All known channels, newest first, optionally filtered by platform.
pub fn list_channels(platform: Option<&str>) -> Vec<(String, ChannelEntry)> {
    let dir = state().lock().unwrap();
    let mut out: Vec<(String, ChannelEntry)> = Vec::new();
    for (plat, entries) in &dir.platforms {
        if let Some(filter) = platform {
            if !filter.is_empty() && plat != &filter.to_lowercase() {
                continue;
            }
        }
        for entry in entries {
            out.push((plat.clone(), entry.clone()));
        }
    }
    out.sort_by(|a, b| b.1.updated_at.cmp(&a.1.updated_at));
    out
}

/// Last inbound message id recorded for a chat (reaction fallback when
/// the caller omits `message_id`).
pub fn last_message_id_for(platform: &str, chat_id: &str) -> Option<String> {
    let dir = state().lock().unwrap();
    dir.platforms
        .get(&platform.to_lowercase())
        .and_then(|entries| entries.iter().find(|entry| entry.id == chat_id))
        .map(|entry| entry.last_message_id.clone())
        .filter(|id| !id.is_empty())
}

/// Directory entry for one specific chat (MCP bridge enrichment —
/// hermes `lookup_channel_type` plus the friendly name).
pub fn channel_info(platform: &str, chat_id: &str) -> Option<ChannelEntry> {
    let dir = state().lock().unwrap();
    dir.platforms
        .get(&platform.to_lowercase())
        .and_then(|entries| entries.iter().find(|entry| entry.id == chat_id).cloned())
}

/// hermes `_normalize_channel_query`.
fn normalize_query(value: &str) -> String {
    value.trim().trim_start_matches('#').trim().to_lowercase()
}

/// hermes `_channel_target_name` — the human-facing label in `list`.
fn target_name(platform: &str, entry: &ChannelEntry) -> String {
    if platform == "discord" {
        format!("#{}", entry.name)
    } else if !entry.chat_type.is_empty() {
        format!("{} ({})", entry.name, entry.chat_type)
    } else {
        entry.name.clone()
    }
}

/// Resolve a human-friendly channel name to a chat id (hermes
/// `resolve_channel_name`): exact id → exact name/display-label →
/// unambiguous prefix → unambiguous substring.
pub fn resolve_by_name(platform: &str, name: &str) -> Option<String> {
    let dir = state().lock().unwrap();
    let entries = dir.platforms.get(&platform.to_lowercase())?;
    if entries.is_empty() {
        return None;
    }
    let raw = name.trim();
    if let Some(exact) = entries.iter().find(|entry| entry.id == raw) {
        return Some(exact.id.clone());
    }
    let query = normalize_query(name);
    if query.is_empty() {
        return None;
    }
    for entry in entries {
        if normalize_query(&entry.name) == query
            || normalize_query(&target_name(platform, entry)) == query
        {
            return Some(entry.id.clone());
        }
    }
    let prefix_matches: Vec<&ChannelEntry> = entries
        .iter()
        .filter(|entry| !entry.name.is_empty() && normalize_query(&entry.name).starts_with(&query))
        .collect();
    if prefix_matches.len() == 1 {
        return Some(prefix_matches[0].id.clone());
    }
    let substring_matches: Vec<&ChannelEntry> = entries
        .iter()
        .filter(|entry| !entry.name.is_empty() && normalize_query(&entry.name).contains(&query))
        .collect();
    if substring_matches.len() == 1 {
        return Some(substring_matches[0].id.clone());
    }
    None
}

/// hermes `format_directory_for_display` — the `action='list'` rendering.
pub fn format_directory_for_display() -> String {
    let dir = state().lock().unwrap();
    if dir.platforms.is_empty() || dir.platforms.values().all(|entries| entries.is_empty()) {
        return "No messaging platforms connected or no channels discovered yet.".to_string();
    }
    let mut lines: Vec<String> = vec!["Available messaging targets:\n".to_string()];
    for (platform, entries) in &dir.platforms {
        let title = {
            let mut chars = platform.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        };
        if entries.is_empty() {
            lines.push(format!("{title}:"));
            lines.push(format!(
                "  (no channels discovered yet — send directly with {platform}:<chat_id>, \
                 or bare '{platform}' for the home channel)"
            ));
            lines.push(String::new());
            continue;
        }
        lines.push(format!("{title}:"));
        let mut sorted: Vec<&ChannelEntry> = entries.iter().collect();
        sorted.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        for entry in sorted {
            lines.push(format!("  {platform}:{}", target_name(platform, entry)));
        }
        lines.push(String::new());
    }
    lines.push("Use these as the \"target\" parameter when sending.".to_string());
    lines.push("Bare platform name (e.g. \"telegram\") sends to home channel.".to_string());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ulnclaw-chdir-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    struct HomeGuard {
        prev: Option<String>,
    }

    impl HomeGuard {
        fn set(home: &std::path::Path) -> Self {
            let prev = std::env::var("ULNCLAW_HOME").ok();
            std::env::set_var("ULNCLAW_HOME", home);
            Self { prev }
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match self.prev.take() {
                Some(value) => std::env::set_var("ULNCLAW_HOME", value),
                None => std::env::remove_var("ULNCLAW_HOME"),
            }
        }
    }

    #[test]
    fn record_and_list_roundtrip() {
        let _env = crate::models_dev::test_env_lock();
        let home = temp_home();
        let _guard = HomeGuard::set(&home);
        reset_for_tests();

        record_channel("Telegram", "42", "General", "group", "m1");
        record_channel("telegram", "42", "General renamed", "", "m2");
        record_channel("discord", "99", "dev", "channel", "");

        let all = list_channels(None);
        assert_eq!(all.len(), 2);
        let tele = list_channels(Some("telegram"));
        assert_eq!(tele.len(), 1);
        assert_eq!(tele[0].1.id, "42");
        assert_eq!(tele[0].1.name, "General renamed");
        assert_eq!(tele[0].1.chat_type, "group");
        assert_eq!(last_message_id_for("telegram", "42"), Some("m2".into()));

        // Persisted to disk.
        let raw = std::fs::read_to_string(home.join("channel_directory.json")).unwrap();
        assert!(raw.contains("General renamed"));
    }

    #[test]
    fn resolve_matches_hermes_priority() {
        let _env = crate::models_dev::test_env_lock();
        let home = temp_home();
        let _guard = HomeGuard::set(&home);
        reset_for_tests();

        record_channel("slack", "C111", "engineering team", "channel", "");
        record_channel("slack", "C222", "engineering", "channel", "");

        // Exact id wins even when names would match too.
        assert_eq!(resolve_by_name("slack", "C111"), Some("C111".into()));
        // Exact name beats prefix.
        assert_eq!(resolve_by_name("slack", "Engineering"), Some("C222".into()));
        // '#' normalization (display label form).
        assert_eq!(
            resolve_by_name("slack", "#engineering"),
            Some("C222".into())
        );
        // Ambiguous prefix falls back to unique substring.
        assert_eq!(resolve_by_name("slack", "team"), Some("C111".into()));
        assert_eq!(resolve_by_name("slack", "missing"), None);
    }

    #[test]
    fn aliases_overlay_renames_and_injects() {
        let _env = crate::models_dev::test_env_lock();
        let home = temp_home();
        let _guard = HomeGuard::set(&home);
        reset_for_tests();

        record_channel("whatsapp", "123@g.us", "Raw Group", "group", "");
        std::fs::write(
            home.join("channel_aliases.json"),
            r#"{"whatsapp": {"123@g.us": "Family", "456@g.us": "Pre-named"}}"#,
        )
        .unwrap();

        // A record after the alias file exists re-applies the overlay.
        record_channel("whatsapp", "123@g.us", "", "", "");
        let entries = list_channels(Some("whatsapp"));
        let names: Vec<&str> = entries
            .iter()
            .map(|(_, entry)| entry.name.as_str())
            .collect();
        assert!(names.contains(&"Family"));
        assert!(names.contains(&"Pre-named"));
        assert_eq!(
            resolve_by_name("whatsapp", "family"),
            Some("123@g.us".into())
        );
    }

    #[test]
    fn display_format_lists_targets() {
        let _env = crate::models_dev::test_env_lock();
        let home = temp_home();
        let _guard = HomeGuard::set(&home);
        reset_for_tests();

        record_channel("telegram", "-100", "News", "channel", "");
        record_channel("discord", "55", "bot-home", "channel", "");

        let display = format_directory_for_display();
        assert!(display.contains("telegram:News (channel)"));
        assert!(display.contains("discord:#bot-home"));
        assert!(display.contains("Bare platform name"));
    }
}

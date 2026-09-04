//! Skill usage telemetry — port of hermes `tools/skill_usage.py`
//! (v2026.8.3), scoped to what the learning graph / journey surface needs.
//!
//! Every profile skill carries a sidecar record in `<home>/skills/.usage.json`
//! tracking view/use/patch counters, lifecycle state, pinning, and
//! provenance (`created_by: "agent"`). Counters are pure observability —
//! they never block anything. Lifecycle mutators (state / pinned /
//! created_by) only touch records for skills that live in the profile
//! skills dir.
//!
//! Archive/restore moves skill directories into `<home>/skills/.archive/`
//! (journey's `delete` is an archive, recoverable via `restore`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{json, Value};

pub const STATE_ACTIVE: &str = "active";
pub const STATE_STALE: &str = "stale";
pub const STATE_ARCHIVED: &str = "archived";
pub const VALID_STATES: &[&str] = &[STATE_ACTIVE, STATE_STALE, STATE_ARCHIVED];

fn usage_lock() -> &'static Mutex<()> {
    static LOCK: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn parse_iso(value: Option<&str>) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    let raw = value?.trim();
    if raw.is_empty() {
        return None;
    }
    chrono::DateTime::parse_from_rfc3339(raw).ok()
}

/// `<home>/skills/.usage.json`.
pub fn usage_file(home: &Path) -> PathBuf {
    home.join("skills").join(".usage.json")
}

fn archive_dir(home: &Path) -> PathBuf {
    home.join("skills").join(".archive")
}

fn empty_record() -> Value {
    json!({
        "created_by": null,
        "use_count": 0,
        "view_count": 0,
        "last_used_at": null,
        "last_viewed_at": null,
        "patch_count": 0,
        "last_patched_at": null,
        "created_at": now_iso(),
        "state": STATE_ACTIVE,
        "pinned": false,
        "archived_at": null,
    })
}

/// Read the entire `.usage.json` map; missing/corrupt → empty (hermes
/// `load_usage`).
pub fn load_usage(home: &Path) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    let Ok(text) = std::fs::read_to_string(usage_file(home)) else {
        return out;
    };
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&text) else {
        return out;
    };
    for (key, value) in map {
        if value.is_object() {
            out.insert(key, value);
        }
    }
    out
}

/// Write the usage map atomically (temp file + rename) — hermes
/// `save_usage`. Best-effort: errors are swallowed.
pub fn save_usage(home: &Path, data: &BTreeMap<String, Value>) {
    let path = usage_file(home);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let payload = serde_json::to_string_pretty(&data).unwrap_or_default();
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, payload).is_err() {
        return;
    }
    std::fs::rename(&tmp, &path).ok();
}

/// The record for `skill_name`, or a fresh one when missing (missing keys
/// backfilled) — hermes `get_record`.
pub fn get_record(home: &Path, skill_name: &str) -> Value {
    let data = load_usage(home);
    let base = empty_record();
    let Some(Value::Object(existing)) = data.get(skill_name) else {
        return base;
    };
    let mut record = Value::Object(existing.clone());
    let base_map = base.as_object().expect("empty_record is an object");
    if let Some(map) = record.as_object_mut() {
        for (key, value) in base_map {
            map.entry(key.clone()).or_insert_with(|| value.clone());
        }
    }
    record
}

fn mutate<F: FnOnce(&mut Value)>(home: &Path, skill_name: &str, mutator: F) {
    if skill_name.is_empty() {
        return;
    }
    let _guard = usage_lock().lock().ok();
    let mut data = load_usage(home);
    let mut record = match data.get(skill_name) {
        Some(Value::Object(_)) => data.remove(skill_name).unwrap(),
        _ => empty_record(),
    };
    mutator(&mut record);
    data.insert(skill_name.to_string(), record);
    save_usage(home, &data);
}

/// Bump `view_count` + `last_viewed_at` (hermes `bump_view`) — called from
/// `skill_view`.
pub fn bump_view(home: &Path, skill_name: &str) {
    mutate(home, skill_name, |rec| {
        let count = rec.get("view_count").and_then(|v| v.as_u64()).unwrap_or(0);
        rec["view_count"] = json!(count + 1);
        rec["last_viewed_at"] = json!(now_iso());
    });
}

/// Bump `use_count` + `last_used_at` (hermes `bump_use`) — called when a
/// skill is actively used.
pub fn bump_use(home: &Path, skill_name: &str) {
    mutate(home, skill_name, |rec| {
        let count = rec.get("use_count").and_then(|v| v.as_u64()).unwrap_or(0);
        rec["use_count"] = json!(count + 1);
        rec["last_used_at"] = json!(now_iso());
    });
}

/// Bump `patch_count` + `last_patched_at` (hermes `bump_patch`) — called
/// from `skill_manage` updates.
pub fn bump_patch(home: &Path, skill_name: &str) {
    mutate(home, skill_name, |rec| {
        let count = rec.get("patch_count").and_then(|v| v.as_u64()).unwrap_or(0);
        rec["patch_count"] = json!(count + 1);
        rec["last_patched_at"] = json!(now_iso());
    });
}

/// Mark a skill agent-created (hermes `mark_agent_created`) — called from
/// `skill_manage` create.
pub fn mark_agent_created(home: &Path, skill_name: &str) {
    mutate(home, skill_name, |rec| {
        rec["created_by"] = json!("agent");
    });
}

/// Set lifecycle state; invalid states are ignored (hermes `set_state`).
pub fn set_state(home: &Path, skill_name: &str, state: &str) {
    if !VALID_STATES.contains(&state) {
        return;
    }
    let state = state.to_string();
    mutate(home, skill_name, move |rec| {
        rec["state"] = json!(state);
        if state == STATE_ARCHIVED {
            rec["archived_at"] = json!(now_iso());
        } else if state == STATE_ACTIVE {
            rec["archived_at"] = Value::Null;
        }
    });
}

/// Pin/unpin (hermes `set_pinned`).
pub fn set_pinned(home: &Path, skill_name: &str, pinned: bool) {
    mutate(home, skill_name, move |rec| {
        rec["pinned"] = json!(pinned);
    });
}

/// Drop a skill's usage entry entirely — called when a skill is deleted
/// (hermes `forget`).
pub fn forget(home: &Path, skill_name: &str) {
    if skill_name.is_empty() {
        return;
    }
    let _guard = usage_lock().lock().ok();
    let mut data = load_usage(home);
    if data.remove(skill_name).is_some() {
        save_usage(home, &data);
    }
}

/// Newest actual activity timestamp (use/view/patch; creation excluded) —
/// hermes `latest_activity_at`.
pub fn latest_activity_at(record: &Value) -> Option<String> {
    let mut latest: Option<(chrono::DateTime<chrono::FixedOffset>, String)> = None;
    for key in ["last_used_at", "last_viewed_at", "last_patched_at"] {
        let Some(raw) = record.get(key).and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(dt) = parse_iso(Some(raw)) else {
            continue;
        };
        match &latest {
            Some((prev, _)) if dt <= *prev => {}
            _ => latest = Some((dt, raw.to_string())),
        }
    }
    latest.map(|(_, raw)| raw)
}

/// Total observed activity across use/view/patch (hermes
/// `activity_count`).
pub fn activity_count(record: &Value) -> u64 {
    ["use_count", "view_count", "patch_count"]
        .iter()
        .map(|key| record.get(*key).and_then(|v| v.as_u64()).unwrap_or(0))
        .sum()
}

// ---------------------------------------------------------------------------
// Reports (curator surfaces)
// ---------------------------------------------------------------------------

/// One row of [`usage_report`].
pub struct UsageReportRow {
    pub name: String,
    pub provenance: String,
    pub use_count: u64,
    pub view_count: u64,
    pub patch_count: u64,
    pub activity_count: u64,
    pub last_activity_at: Option<String>,
    pub created_by: Option<String>,
    pub state: String,
    pub pinned: bool,
    pub record: Value,
}

/// Usage telemetry for every skill on disk (hermes `usage_report`).
pub fn usage_report(home: &Path) -> Vec<UsageReportRow> {
    let skills_dir = home.join("skills");
    let data = load_usage(home);
    let mut rows: Vec<UsageReportRow> = Vec::new();
    for skill in crate::skills::list_skills(&skills_dir) {
        let record = data.get(&skill.name).cloned().unwrap_or_else(empty_record);
        let created_by = record
            .get("created_by")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        rows.push(UsageReportRow {
            provenance: if created_by.as_deref() == Some("agent") {
                "agent".to_string()
            } else {
                "user".to_string()
            },
            use_count: record
                .get("use_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            view_count: record
                .get("view_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            patch_count: record
                .get("patch_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            activity_count: activity_count(&record),
            last_activity_at: latest_activity_at(&record),
            state: record
                .get("state")
                .and_then(|v| v.as_str())
                .unwrap_or(STATE_ACTIVE)
                .to_string(),
            pinned: record
                .get("pinned")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            name: skill.name,
            created_by,
            record,
        });
    }
    rows
}

/// Names of archived (recoverable) skills — directory names under
/// `<home>/skills/.archive` (hermes `list_archived_skill_names`).
pub fn list_archived_skill_names(home: &Path) -> Vec<String> {
    let root = archive_dir(home);
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
        .collect();
    names.sort();
    names
}

/// Skills on disk that carry no provenance marker (`created_by`) — hermes
/// `unmanaged_report`. `has_provenance_key` is true when a record exists
/// but the marker is null.
pub fn unmanaged_report(home: &Path) -> Vec<Value> {
    let data = load_usage(home);
    let mut rows = Vec::new();
    for skill in crate::skills::list_skills(&home.join("skills")) {
        match data.get(&skill.name) {
            Some(record) => {
                let has_key = record
                    .get("created_by")
                    .map(|v| !v.is_null())
                    .unwrap_or(false);
                if has_key {
                    continue; // managed
                }
                rows.push(json!({
                    "name": skill.name,
                    "has_provenance_key": true,
                    "activity_count": activity_count(record),
                    "last_activity_at": latest_activity_at(record),
                }));
            }
            None => {
                rows.push(json!({
                    "name": skill.name,
                    "has_provenance_key": false,
                    "activity_count": 0,
                    "last_activity_at": null,
                }));
            }
        }
    }
    rows
}

/// Names of unmanaged skills (hermes `list_unmanaged_skill_names`).
pub fn list_unmanaged_skill_names(home: &Path) -> Vec<String> {
    unmanaged_report(home)
        .iter()
        .filter_map(|r| {
            r.get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .collect()
}

/// Hand an unmanaged skill to the curator by explicit user declaration
/// (hermes `adopt_skill`): seeds the record and stamps provenance.
pub fn adopt_skill(home: &Path, skill_name: &str) -> (bool, String) {
    if skill_name.is_empty() {
        return (false, "skill name required".to_string());
    }
    if crate::skills::find_skill(&home.join("skills"), skill_name).is_none() {
        return (false, format!("skill '{}' not found", skill_name));
    }
    mark_agent_created(home, skill_name);
    (
        true,
        format!("adopted '{}' into curator management", skill_name),
    )
}

// ---------------------------------------------------------------------------
// Archive / restore
// ---------------------------------------------------------------------------

/// Move a skill directory into `<home>/skills/.archive/` (hermes
/// `archive_skill`, minus hub/bundled gating — ulnclaw profiles carry only
/// user skills). Collisions get a UTC timestamp suffix.
pub fn archive_skill(home: &Path, skill_name: &str) -> (bool, String) {
    let skills_dir = home.join("skills");
    let skill_dir = match crate::skills::find_skill(&skills_dir, skill_name) {
        Some(skill) => skill.path.clone(),
        None => return (false, format!("skill '{}' not found", skill_name)),
    };
    let record = get_record(home, skill_name);
    if record
        .get("pinned")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return (
            false,
            format!("'{}' is pinned — unpin it first", skill_name),
        );
    }
    let root = archive_dir(home);
    if std::fs::create_dir_all(&root).is_err() {
        return (false, "failed to create archive dir".to_string());
    }
    let dir_name = skill_dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| skill_name.to_string());
    let mut dest = root.join(&dir_name);
    if dest.exists() {
        let stamp = chrono::Utc::now().format("%Y%m%d%H%M%S");
        dest = root.join(format!("{}-{}", dir_name, stamp));
    }
    if std::fs::rename(&skill_dir, &dest).is_err() {
        return (false, "failed to archive".to_string());
    }
    set_state(home, skill_name, STATE_ARCHIVED);
    (true, format!("archived to {}", dest.display()))
}

/// Restore an archived skill back to the skills dir (hermes
/// `restore_skill`). Matches the exact name first, then the
/// timestamp-suffixed duplicate shape `<name>-<14 digits>` (newest first).
pub fn restore_skill(home: &Path, skill_name: &str) -> (bool, String) {
    let root = archive_dir(home);
    if !root.exists() {
        return (false, "no archive directory".to_string());
    }
    let mut exact: Vec<PathBuf> = Vec::new();
    let mut timestamped: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(name) = path.file_name().map(|n| n.to_string_lossy().to_string()) else {
                continue;
            };
            if name == skill_name {
                exact.push(path);
            } else if let Some(suffix) = name.strip_prefix(&format!("{}-", skill_name)) {
                if suffix.len() == 14 && suffix.chars().all(|c| c.is_ascii_digit()) {
                    timestamped.push(path);
                }
            }
        }
    }
    // Exact name wins; timestamped duplicates restore newest-first.
    timestamped.sort_by(|a, b| b.cmp(a));
    let mut candidates = exact;
    candidates.extend(timestamped);
    let Some(src) = candidates.into_iter().next() else {
        return (
            false,
            format!("skill '{}' not found in archive", skill_name),
        );
    };
    let dest = home.join("skills").join(skill_name);
    if dest.exists() {
        return (
            false,
            format!("destination already exists: {}", dest.display()),
        );
    }
    if std::fs::rename(&src, &dest).is_err() {
        return (false, "failed to restore".to_string());
    }
    set_state(home, skill_name, STATE_ACTIVE);
    (true, format!("restored to {}", dest.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    static HOME_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn temp_home() -> (PathBuf, PathBuf) {
        let n = HOME_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("ulnclaw-skill-usage-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(dir.join("skills")).unwrap();
        (dir.clone(), dir)
    }

    fn make_skill(home: &Path, name: &str) {
        let dir = home.join("skills").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {}\ndescription: test\n---\n\nbody\n", name),
        )
        .unwrap();
    }

    #[test]
    fn counters_and_backfill() {
        let (home, _keep) = temp_home();
        bump_view(&home, "demo");
        bump_view(&home, "demo");
        bump_use(&home, "demo");
        bump_patch(&home, "demo");
        let rec = get_record(&home, "demo");
        assert_eq!(rec["view_count"], 2);
        assert_eq!(rec["use_count"], 1);
        assert_eq!(rec["patch_count"], 1);
        assert!(rec["last_viewed_at"].is_string());
        // Missing record gets a fresh one with defaults.
        let fresh = get_record(&home, "ghost");
        assert_eq!(fresh["state"], "active");
        assert_eq!(fresh["use_count"], 0);
        assert_eq!(activity_count(&rec), 4);
        assert!(latest_activity_at(&rec).is_some());
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn lifecycle_mutators() {
        let (home, _keep) = temp_home();
        mark_agent_created(&home, "demo");
        assert_eq!(get_record(&home, "demo")["created_by"], "agent");
        set_state(&home, "demo", STATE_ARCHIVED);
        let rec = get_record(&home, "demo");
        assert_eq!(rec["state"], STATE_ARCHIVED);
        assert!(rec["archived_at"].is_string());
        set_state(&home, "demo", STATE_ACTIVE);
        assert!(get_record(&home, "demo")["archived_at"].is_null());
        // Invalid state is ignored.
        set_state(&home, "demo", "bogus");
        assert_eq!(get_record(&home, "demo")["state"], STATE_ACTIVE);
        set_pinned(&home, "demo", true);
        assert_eq!(get_record(&home, "demo")["pinned"], true);
        forget(&home, "demo");
        assert!(!load_usage(&home).contains_key("demo"));
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn corrupt_usage_loads_empty() {
        let (home, _keep) = temp_home();
        std::fs::write(usage_file(&home), "{ not json").unwrap();
        assert!(load_usage(&home).is_empty());
        // Non-object values are dropped.
        std::fs::write(usage_file(&home), r#"{"a": {"use_count": 1}, "b": 5}"#).unwrap();
        let data = load_usage(&home);
        assert_eq!(data.len(), 1);
        assert!(data.contains_key("a"));
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn archive_and_restore_roundtrip() {
        let (home, _keep) = temp_home();
        make_skill(&home, "demo");
        let (ok, message) = archive_skill(&home, "demo");
        assert!(ok, "archive failed: {}", message);
        assert!(!home.join("skills/demo").exists());
        assert!(home.join("skills/.archive/demo").exists());
        assert_eq!(get_record(&home, "demo")["state"], STATE_ARCHIVED);

        let (ok, _) = restore_skill(&home, "demo");
        assert!(ok);
        assert!(home.join("skills/demo/SKILL.md").exists());
        assert_eq!(get_record(&home, "demo")["state"], STATE_ACTIVE);

        // Missing skill / missing archive entries report cleanly.
        assert!(!archive_skill(&home, "ghost").0);
        assert!(!restore_skill(&home, "ghost").0);
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn pinned_skill_refuses_archive() {
        let (home, _keep) = temp_home();
        make_skill(&home, "demo");
        set_pinned(&home, "demo", true);
        let (ok, message) = archive_skill(&home, "demo");
        assert!(!ok);
        assert!(message.contains("pinned"));
        assert!(home.join("skills/demo").exists());
        std::fs::remove_dir_all(&home).unwrap();
    }
}

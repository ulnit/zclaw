//! Skill curator — port of the local (non-LLM) half of hermes
//! `hermes_cli/curator.py` (v2026.8.3).
//!
//! The curator keeps the skill library tidy: pin/unpin, manual
//! archive/restore, usage telemetry reporting, unmanaged-skill adoption,
//! and idle pruning (bulk-archive of unpinned agent-created skills unused
//! for N days). The hermes automatic background run with LLM-driven
//! consolidation stays desktop-side; ulnclaw exposes the same lifecycle
//! verbs as explicit CLI actions.

use std::path::{Path, PathBuf};

use crate::skill_usage::{self, STATE_ACTIVE, STATE_ARCHIVED, STATE_STALE};

/// Days since the skill's last activity (view / use / patch) — hermes
/// `_idle_days`.
///
/// Falls back to `created_at` so a skill that was authored but never used
/// can still be pruned — otherwise never-touched skills would be immortal.
/// Returns `None` only when both fields are missing or unparseable.
pub fn idle_days(record: &serde_json::Value) -> Option<u64> {
    let raw = record
        .get("last_activity_at")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| record.get("created_at").and_then(|v| v.as_str()))?;
    let dt = chrono::DateTime::parse_from_rfc3339(raw).ok()?;
    let now = chrono::Utc::now();
    let days = (now - dt.with_timezone(&chrono::Utc)).num_days();
    Some(days.max(0) as u64)
}

/// Bulk-archive candidates: unpinned, non-archived, agent-created skills
/// idle for at least `days` (hermes `_cmd_prune` candidate selection),
/// idlest first.
pub fn prune_candidates(home: &Path, days: u64) -> Vec<(String, u64)> {
    let mut candidates: Vec<(String, u64)> = Vec::new();
    for row in skill_usage::usage_report(home) {
        if row.provenance != "agent" {
            continue;
        }
        if row.pinned || row.state == STATE_ARCHIVED {
            continue;
        }
        // Report rows carry the derived last_activity_at (hermes
        // curated_report feeds _idle_days the same shape).
        let mut record = row.record.clone();
        if let Some(ts) = &row.last_activity_at {
            record["last_activity_at"] = serde_json::json!(ts);
        }
        let Some(idle) = idle_days(&record) else {
            continue;
        };
        if idle < days {
            continue;
        }
        candidates.push((row.name, idle));
    }
    candidates.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    candidates
}

/// Human "5s/3m/4h/12d ago" rendering of an ISO timestamp (hermes
/// `_fmt_ts`).
pub fn fmt_ts(ts: Option<&str>) -> String {
    let Some(raw) = ts.filter(|s| !s.is_empty()) else {
        return "never".to_string();
    };
    let Ok(dt) = chrono::DateTime::parse_from_rfc3339(raw) else {
        return raw.to_string();
    };
    let secs = (chrono::Utc::now() - dt.with_timezone(&chrono::Utc)).num_seconds();
    if secs < 0 {
        return "just now".to_string();
    }
    if secs < 60 {
        format!("{}s ago", secs)
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

/// Status summary rows (hermes `_cmd_status`, scoped to local telemetry —
/// the background-run state lives desktop-side).
pub fn status_summary(home: &Path) -> Vec<(String, usize)> {
    let rows = skill_usage::usage_report(home);
    let archived = skill_usage::list_archived_skill_names(home).len();
    let unmanaged = skill_usage::list_unmanaged_skill_names(home).len();
    let mut states: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    let mut pinned = 0usize;
    let mut agent_created = 0usize;
    let mut used = 0usize;
    for row in &rows {
        *states.entry(row.state.clone()).or_insert(0) += 1;
        if row.pinned {
            pinned += 1;
        }
        if row.provenance == "agent" {
            agent_created += 1;
        }
        if row.activity_count > 0 {
            used += 1;
        }
    }
    let mut out = vec![
        ("skills on disk".to_string(), rows.len()),
        ("agent-created".to_string(), agent_created),
        ("with activity".to_string(), used),
        ("pinned".to_string(), pinned),
        ("unmanaged (no provenance)".to_string(), unmanaged),
        ("archived (recoverable)".to_string(), archived),
    ];
    for (state, count) in states {
        out.push((format!("state: {}", state), count));
    }
    out
}

/// Window before an unused skill is marked stale (hermes
/// `DEFAULT_STALE_AFTER_DAYS`).
pub const DEFAULT_STALE_AFTER_DAYS: i64 = 30;
/// Window before a stale skill is archived (hermes
/// `DEFAULT_ARCHIVE_AFTER_DAYS`).
pub const DEFAULT_ARCHIVE_AFTER_DAYS: i64 = 90;

/// Counters for one auto-transition pass (hermes
/// `apply_automatic_transitions` return shape, lean subset).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AutoTransitionCounts {
    pub checked: u64,
    pub marked_stale: u64,
    pub archived: u64,
    pub reactivated: u64,
}

/// Deterministic stale/archive/reactivate pass over curated skills —
/// lean port of hermes `apply_automatic_transitions` (no cron-reference
/// exemption, no builtin seeding). Pinned skills are never touched;
/// skills without a parseable activity timestamp are left alone.
/// `dry_run` counts the would-be transitions without mutating anything.
pub fn apply_automatic_transitions(home: &Path, dry_run: bool) -> AutoTransitionCounts {
    let now = chrono::Utc::now();
    let stale_cutoff = now - chrono::Duration::days(DEFAULT_STALE_AFTER_DAYS);
    let archive_cutoff = now - chrono::Duration::days(DEFAULT_ARCHIVE_AFTER_DAYS);
    let mut counts = AutoTransitionCounts::default();
    for row in skill_usage::usage_report(home) {
        counts.checked += 1;
        if row.pinned {
            continue;
        }
        let Some(raw) = row.last_activity_at.as_deref() else {
            continue;
        };
        let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(raw) else {
            continue;
        };
        let anchor = parsed.with_timezone(&chrono::Utc);
        if anchor <= archive_cutoff && row.state != STATE_ARCHIVED {
            counts.archived += 1;
            if !dry_run {
                skill_usage::archive_skill(home, &row.name);
            }
        } else if anchor <= stale_cutoff && row.state == STATE_ACTIVE {
            counts.marked_stale += 1;
            if !dry_run {
                skill_usage::set_state(home, &row.name, STATE_STALE);
            }
        } else if anchor > stale_cutoff && row.state == STATE_STALE {
            counts.reactivated += 1;
            if !dry_run {
                skill_usage::set_state(home, &row.name, STATE_ACTIVE);
            }
        }
    }
    counts
}

/// Curator runtime state file (pause flag; hermes curator state parity).
pub fn state_file(home: &Path) -> PathBuf {
    home.join("curator-state.json")
}

/// Whether the curator is paused (`ulnclaw curator pause`).
pub fn is_paused(home: &Path) -> bool {
    std::fs::read_to_string(state_file(home))
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|value| value.get("paused").and_then(|v| v.as_bool()))
        .unwrap_or(false)
}

/// Persist the curator pause flag.
pub fn set_paused(home: &Path, paused: bool) {
    let mut value = std::fs::read_to_string(state_file(home))
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .filter(|v| v.is_object())
        .unwrap_or_else(|| serde_json::json!({}));
    value["paused"] = serde_json::json!(paused);
    if let Ok(text) = serde_json::to_string_pretty(&value) {
        let _ = std::fs::write(state_file(home), text);
    }
}

const CURATOR_SLASH_USAGE: &str = "(o_o) usage: /curator [status|run [--dry-run]|pause|resume|pin <skill>|unpin <skill>|archive <skill>|restore <skill>|list-archived]\n";

/// Shared `/curator` dispatch for REPL and gateway (P670 — hermes
/// `/curator` parity): background skill-maintenance controls from the
/// chat prompt. One formatted string back, no process spawn.
pub fn run_slash(home: &Path, rest: &str) -> String {
    let mut parts = rest.split_whitespace();
    let sub = parts.next().unwrap_or("status");
    match sub {
        "status" => {
            let mut out = String::new();
            for (label, count) in status_summary(home) {
                out.push_str(&format!("  {:<28} {}\n", label, count));
            }
            out.push_str(&format!(
                "  {:<28} {}\n",
                "paused",
                if is_paused(home) { "yes" } else { "no" }
            ));
            out
        }
        "run" => {
            let dry_run = parts.any(|token| token == "--dry-run" || token == "dry-run");
            if is_paused(home) && !dry_run {
                return "(._.) curator is paused — resume it first (/curator resume)\n".to_string();
            }
            let counts = apply_automatic_transitions(home, dry_run);
            format!(
                "curator: checked={} stale={} archived={} reactivated={}{}\n",
                counts.checked,
                counts.marked_stale,
                counts.archived,
                counts.reactivated,
                if dry_run { " (dry-run)" } else { "" }
            )
        }
        "pause" => {
            set_paused(home, true);
            "⏸ curator paused (auto-transition passes will not apply)\n".to_string()
        }
        "resume" => {
            set_paused(home, false);
            "▶ curator resumed\n".to_string()
        }
        "pin" | "unpin" => {
            let Some(skill) = parts.next() else {
                return format!("(o_o) usage: /curator {sub} <skill>\n");
            };
            crate::skill_usage::set_pinned(home, skill, sub == "pin");
            if sub == "pin" {
                format!("📌 pinned '{skill}' (bypasses auto-transitions)\n")
            } else {
                format!("unpinned '{skill}'\n")
            }
        }
        "archive" | "restore" => {
            let Some(skill) = parts.next() else {
                return format!("(o_o) usage: /curator {sub} <skill>\n");
            };
            let (ok, message) = if sub == "archive" {
                crate::skill_usage::archive_skill(home, skill)
            } else {
                crate::skill_usage::restore_skill(home, skill)
            };
            if ok {
                format!("✓ {message}\n")
            } else {
                format!("(._.) {message}\n")
            }
        }
        "list-archived" | "archived" => {
            let names = crate::skill_usage::list_archived_skill_names(home);
            if names.is_empty() {
                return "(o_o) no archived skills.\n".to_string();
            }
            let mut out = format!(
                "{} archived skill(s) — /curator restore <name> to recover:\n",
                names.len()
            );
            for name in names {
                out.push_str(&format!("  {name}\n"));
            }
            out
        }
        _ => CURATOR_SLASH_USAGE.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    static HOME_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn temp_home() -> std::path::PathBuf {
        let n = HOME_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("ulnclaw-curator-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(dir.join("skills")).unwrap();
        dir
    }

    fn make_skill(home: &Path, name: &str) {
        let dir = home.join("skills").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {}\ndescription: t\n---\nbody\n", name),
        )
        .unwrap();
    }

    fn iso_days_ago(days: i64) -> String {
        (chrono::Utc::now() - chrono::Duration::days(days))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    }

    #[test]
    fn idle_days_computation() {
        let rec = json!({"last_activity_at": iso_days_ago(10), "created_at": iso_days_ago(50)});
        assert_eq!(idle_days(&rec), Some(10));
        // Falls back to created_at when there is no activity.
        let rec = json!({"created_at": iso_days_ago(30)});
        assert_eq!(idle_days(&rec), Some(30));
        // Unparseable → None.
        let rec = json!({"last_activity_at": "garbage"});
        assert_eq!(idle_days(&rec), None);
    }

    #[test]
    fn prune_selection_rules() {
        let home = temp_home();
        make_skill(&home, "idle-agent");
        make_skill(&home, "fresh-agent");
        make_skill(&home, "idle-user");
        make_skill(&home, "pinned-agent");
        let mut data = std::collections::BTreeMap::new();
        data.insert(
            "idle-agent".to_string(),
            json!({"created_by": "agent", "state": "active", "pinned": false, "created_at": iso_days_ago(120)}),
        );
        data.insert(
            "fresh-agent".to_string(),
            json!({"created_by": "agent", "state": "active", "pinned": false, "last_used_at": iso_days_ago(2), "created_at": iso_days_ago(120)}),
        );
        data.insert(
            "idle-user".to_string(),
            json!({"created_by": null, "state": "active", "pinned": false, "created_at": iso_days_ago(200)}),
        );
        data.insert(
            "pinned-agent".to_string(),
            json!({"created_by": "agent", "state": "active", "pinned": true, "created_at": iso_days_ago(300)}),
        );
        skill_usage::save_usage(&home, &data);

        let candidates = prune_candidates(&home, 90);
        let names: Vec<&str> = candidates.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["idle-agent"]);
        let (_, idle) = &candidates[0];
        assert!(*idle >= 90);
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn fmt_ts_shapes() {
        assert_eq!(fmt_ts(None), "never");
        assert_eq!(fmt_ts(Some("")), "never");
        assert!(fmt_ts(Some(&iso_days_ago(3))).contains("d ago"));
        assert_eq!(fmt_ts(Some("not-a-date")), "not-a-date");
    }

    #[test]
    fn status_and_unmanaged_reports() {
        let home = temp_home();
        make_skill(&home, "managed");
        make_skill(&home, "stray");
        skill_usage::mark_agent_created(&home, "managed");
        skill_usage::bump_use(&home, "managed");

        let status = status_summary(&home);
        let get = |label: &str| {
            status
                .iter()
                .find(|(l, _)| l == label)
                .map(|(_, n)| *n)
                .unwrap_or(999)
        };
        assert_eq!(get("skills on disk"), 2);
        assert_eq!(get("agent-created"), 1);
        assert_eq!(get("with activity"), 1);
        assert_eq!(get("unmanaged (no provenance)"), 1);

        let unmanaged = skill_usage::list_unmanaged_skill_names(&home);
        assert_eq!(unmanaged, vec!["stray".to_string()]);

        // Adopt stamps provenance and clears the unmanaged list.
        let (ok, _) = skill_usage::adopt_skill(&home, "stray");
        assert!(ok);
        assert!(skill_usage::list_unmanaged_skill_names(&home).is_empty());
        // Adopting a missing skill fails cleanly.
        assert!(!skill_usage::adopt_skill(&home, "ghost").0);
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn usage_report_rows() {
        let home = temp_home();
        make_skill(&home, "demo");
        skill_usage::bump_view(&home, "demo");
        skill_usage::bump_view(&home, "demo");
        skill_usage::bump_patch(&home, "demo");
        let rows = skill_usage::usage_report(&home);
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.name, "demo");
        assert_eq!(row.view_count, 2);
        assert_eq!(row.patch_count, 1);
        assert_eq!(row.activity_count, 3);
        assert_eq!(row.provenance, "user");
        assert!(row.last_activity_at.is_some());
        std::fs::remove_dir_all(&home).unwrap();
    }

    fn set_last_used(home: &Path, name: &str, days_ago: i64) {
        let ts = (chrono::Utc::now() - chrono::Duration::days(days_ago)).to_rfc3339();
        let path = skill_usage::usage_file(home);
        let mut data: serde_json::Value = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_else(|| json!({}));
        data[name]["last_used_at"] = json!(ts);
        data[name]["use_count"] = json!(1);
        std::fs::write(&path, serde_json::to_string_pretty(&data).unwrap()).unwrap();
    }

    #[test]
    fn auto_transitions_stale_archive_reactivate() {
        let home = temp_home();
        make_skill(&home, "old-skill");
        make_skill(&home, "stale-skill");
        make_skill(&home, "revived-skill");
        make_skill(&home, "fresh-skill");
        make_skill(&home, "pinned-old");
        set_last_used(&home, "old-skill", DEFAULT_ARCHIVE_AFTER_DAYS + 5);
        set_last_used(&home, "stale-skill", DEFAULT_STALE_AFTER_DAYS + 5);
        set_last_used(&home, "revived-skill", 2);
        set_last_used(&home, "fresh-skill", 2);
        set_last_used(&home, "pinned-old", DEFAULT_ARCHIVE_AFTER_DAYS + 5);
        skill_usage::set_pinned(&home, "pinned-old", true);
        skill_usage::set_state(&home, "revived-skill", STATE_STALE);

        // Dry run counts but does not mutate.
        let counts = apply_automatic_transitions(&home, true);
        assert_eq!(counts.checked, 5);
        assert_eq!(counts.archived, 1);
        assert_eq!(counts.marked_stale, 1);
        assert_eq!(counts.reactivated, 1);
        assert_eq!(
            skill_usage::get_record(&home, "old-skill")["state"],
            STATE_ACTIVE
        );

        // Real pass applies.
        let counts = apply_automatic_transitions(&home, false);
        assert_eq!(counts.archived, 1);
        assert_eq!(counts.marked_stale, 1);
        assert_eq!(counts.reactivated, 1);
        assert_eq!(
            skill_usage::get_record(&home, "old-skill")["state"],
            STATE_ARCHIVED
        );
        assert_eq!(
            skill_usage::get_record(&home, "stale-skill")["state"],
            STATE_STALE
        );
        assert_eq!(
            skill_usage::get_record(&home, "revived-skill")["state"],
            STATE_ACTIVE
        );
        assert_eq!(
            skill_usage::get_record(&home, "pinned-old")["state"],
            STATE_ACTIVE
        );
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn paused_flag_roundtrip() {
        let home = temp_home();
        assert!(!is_paused(&home));
        set_paused(&home, true);
        assert!(is_paused(&home));
        set_paused(&home, false);
        assert!(!is_paused(&home));
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn archived_listing() {
        let home = temp_home();
        make_skill(&home, "demo");
        assert!(skill_usage::list_archived_skill_names(&home).is_empty());
        let (ok, _) = skill_usage::archive_skill(&home, "demo");
        assert!(ok);
        assert_eq!(
            skill_usage::list_archived_skill_names(&home),
            vec!["demo".to_string()]
        );
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn curator_slash_status_pause_pin_archive_flow() {
        // P670: /curator slash end-to-end over a temp home.
        let home = temp_home();
        make_skill(&home, "demo-skill");

        // Default = status: summary rows + paused flag.
        let out = run_slash(&home, "");
        assert!(out.contains("paused"), "{out}");

        // Pause blocks a real run but not a dry-run.
        let out = run_slash(&home, "pause");
        assert!(out.contains("paused"), "{out}");
        assert!(is_paused(&home));
        let out = run_slash(&home, "run");
        assert!(out.contains("resume it first"), "{out}");
        let out = run_slash(&home, "run --dry-run");
        assert!(out.contains("dry-run"), "{out}");
        let out = run_slash(&home, "resume");
        assert!(out.contains("resumed"), "{out}");

        // Pin → archive refuses; unpin → archive + list + restore.
        let out = run_slash(&home, "pin demo-skill");
        assert!(out.contains("pinned"), "{out}");
        let out = run_slash(&home, "archive demo-skill");
        assert!(out.contains("pinned"), "{out}");
        let out = run_slash(&home, "unpin demo-skill");
        assert!(out.contains("unpinned"), "{out}");
        let out = run_slash(&home, "archive demo-skill");
        assert!(out.contains("archived to"), "{out}");
        let out = run_slash(&home, "list-archived");
        assert!(out.contains("demo-skill"), "{out}");
        let out = run_slash(&home, "restore demo-skill");
        assert!(out.contains("restored"), "{out}");
        let out = run_slash(&home, "list-archived");
        assert!(out.contains("no archived skills"), "{out}");

        // Usage fallback.
        let out = run_slash(&home, "bogus");
        assert!(out.contains("usage:"), "{out}");
    }
}

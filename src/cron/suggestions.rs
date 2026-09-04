//! Suggested automations — pending cron-job proposals with accept/dismiss.
//!
//! Port of hermes `cron/suggestions.py` + `cron/suggestion_catalog.py` +
//! `hermes_cli/suggestions_cmd.py` (v2026.8.3): a JSON-backed suggestion
//! store under `<home>/cron/suggestions.json` (pending/accepted/dismissed,
//! dedup latching, MAX_PENDING cap), a curated starter catalog, and the
//! shared `/suggestions` command dispatch used by REPL and CLI.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Maximum pending suggestions (hermes `MAX_PENDING`).
pub const MAX_PENDING: usize = 5;

/// Valid suggestion sources (hermes `VALID_SOURCES`).
pub const VALID_SOURCES: &[&str] = &["catalog", "blueprint", "usage", "integration"];

const STATUS_PENDING: &str = "pending";
const STATUS_ACCEPTED: &str = "accepted";
const STATUS_DISMISSED: &str = "dismissed";

/// `create_job`-shaped spec stored verbatim on the suggestion
/// (hermes `job_spec`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JobSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub prompt: String,
    pub schedule: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deliver: Option<String>,
}

/// One suggestion record (hermes suggestion dict).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Suggestion {
    pub id: String,
    pub title: String,
    pub description: String,
    pub source: String,
    pub job_spec: JobSpec,
    pub dedup_key: String,
    pub status: String,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct FileShape {
    #[serde(default)]
    suggestions: Vec<Suggestion>,
}

fn now_iso() -> String {
    chrono::Local::now()
        .format("%Y-%m-%dT%H:%M:%S%:z")
        .to_string()
}

/// JSON-backed suggestion store (hermes `cron/suggestions.py`).
pub struct SuggestionStore {
    path: PathBuf,
}

impl SuggestionStore {
    pub fn open(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Default location: `<home>/cron/suggestions.json`.
    pub fn open_default() -> Self {
        Self::open(
            crate::config::ulnclaw_home()
                .join("cron")
                .join("suggestions.json"),
        )
    }

    fn load_raw(&self) -> Vec<Suggestion> {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|text| serde_json::from_str::<FileShape>(&text).ok())
            .map(|shape| shape.suggestions)
            .unwrap_or_default()
    }

    fn save_raw(&self, suggestions: &[Suggestion]) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(&FileShape {
            suggestions: suggestions.to_vec(),
        })
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        // hermes `_secure_file`: owner-only permissions for the write.
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// All suggestion records, any status (hermes `load_suggestions`).
    pub fn load(&self) -> Vec<Suggestion> {
        self.load_raw()
    }

    /// Pending suggestions in creation order (hermes `list_pending`).
    pub fn list_pending(&self) -> Vec<Suggestion> {
        self.load_raw()
            .into_iter()
            .filter(|s| s.status == STATUS_PENDING)
            .collect()
    }

    /// Register a pending suggestion (hermes `add_suggestion`).
    ///
    /// Returns `None` when skipped: dedup key already decided on or still
    /// pending, or the pending backlog is full. Unknown source / empty
    /// title / empty dedup key are errors.
    pub fn add(
        &self,
        title: &str,
        description: &str,
        source: &str,
        job_spec: JobSpec,
        dedup_key: &str,
    ) -> Result<Option<Suggestion>, String> {
        if !VALID_SOURCES.contains(&source) {
            return Err(format!("unknown suggestion source: {source:?}"));
        }
        if title.trim().is_empty() || dedup_key.trim().is_empty() {
            return Err("title and dedup_key are required".to_string());
        }

        let mut suggestions = self.load_raw();

        // Never re-offer something already decided on; never duplicate pending.
        for existing in &suggestions {
            if existing.dedup_key == dedup_key.trim() {
                return Ok(None);
            }
        }
        let pending_count = suggestions
            .iter()
            .filter(|s| s.status == STATUS_PENDING)
            .count();
        if pending_count >= MAX_PENDING {
            tracing::info!("Suggestion backlog full ({MAX_PENDING}); dropping {title:?}");
            return Ok(None);
        }

        let record = Suggestion {
            id: uuid::Uuid::new_v4().simple().to_string()[..12].to_string(),
            title: title.trim().to_string(),
            description: description.trim().to_string(),
            source: source.to_string(),
            job_spec,
            dedup_key: dedup_key.trim().to_string(),
            status: STATUS_PENDING.to_string(),
            created_at: now_iso(),
            resolved_at: None,
        };
        suggestions.push(record.clone());
        self.save_raw(&suggestions)
            .map_err(|e| format!("save suggestions: {e}"))?;
        Ok(Some(record))
    }

    /// Resolve by id, 1-based pending index, or exact title
    /// (hermes `get_suggestion`).
    pub fn get(&self, reference: &str) -> Option<Suggestion> {
        let suggestions = self.load_raw();
        if let Some(hit) = suggestions.iter().find(|s| s.id == reference) {
            return Some(hit.clone());
        }
        if reference.chars().all(|c| c.is_ascii_digit()) && !reference.is_empty() {
            let pending: Vec<&Suggestion> = suggestions
                .iter()
                .filter(|s| s.status == STATUS_PENDING)
                .collect();
            let idx = reference.parse::<usize>().ok()?.checked_sub(1)?;
            if idx < pending.len() {
                return Some(pending[idx].clone());
            }
        }
        suggestions
            .iter()
            .find(|s| s.title.eq_ignore_ascii_case(reference))
            .cloned()
    }

    fn set_status(&self, suggestion_id: &str, status: &str) -> bool {
        let mut suggestions = self.load_raw();
        let mut changed = false;
        for suggestion in &mut suggestions {
            if suggestion.id == suggestion_id {
                suggestion.status = status.to_string();
                suggestion.resolved_at = Some(now_iso());
                changed = true;
                break;
            }
        }
        if changed {
            self.save_raw(&suggestions).ok();
        }
        changed
    }

    /// Dismiss — latched, never re-offered for its dedup key
    /// (hermes `dismiss_suggestion`).
    pub fn dismiss(&self, reference: &str) -> bool {
        let Some(suggestion) = self.get(reference) else {
            return false;
        };
        self.set_status(&suggestion.id, STATUS_DISMISSED)
    }

    /// Accept: create the real cron job from the stored spec and mark the
    /// suggestion accepted (hermes `accept_suggestion`). Returns the created
    /// job, or `None` when the suggestion is missing/not pending.
    pub fn accept(&self, reference: &str) -> Result<Option<crate::cron::CronJob>, String> {
        let Some(suggestion) = self.get(reference) else {
            return Ok(None);
        };
        if suggestion.status != STATUS_PENDING {
            return Ok(None);
        }
        let spec = &suggestion.job_spec;
        let schedule = crate::cron::parse_schedule(&spec.schedule)
            .map_err(|e| format!("suggestion schedule '{}': {}", spec.schedule, e))?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let job = crate::cron::CronJob {
            id: uuid::Uuid::new_v4().to_string(),
            name: spec
                .name
                .clone()
                .unwrap_or_else(|| suggestion.title.clone()),
            schedule: spec.schedule.clone(),
            prompt: spec.prompt.clone(),
            skills: spec.skills.clone(),
            enabled: true,
            repeat: None,
            next_run: crate::cron::next_run(&schedule),
            created_at: now,
            last_run: None,
            last_status: None,
            deliver: None,
            origin: None,
            last_delivery_error: None,
            attach_to_session: None,
        };
        let store = crate::cron::CronStore::open_default().map_err(|e| e.to_string())?;
        store.add(&job).map_err(|e| e.to_string())?;
        self.set_status(&suggestion.id, STATUS_ACCEPTED);
        Ok(Some(job))
    }

    /// Drop accepted records; dismissed stay for their dedup keys
    /// (hermes `clear_resolved`). Returns the count removed.
    pub fn clear_resolved(&self) -> usize {
        let suggestions = self.load_raw();
        let kept: Vec<Suggestion> = suggestions
            .into_iter()
            .filter(|s| s.status != STATUS_ACCEPTED)
            .collect();
        let removed = suggestions_len_diff(&self.load_raw(), &kept);
        if removed > 0 {
            self.save_raw(&kept).ok();
        }
        removed
    }
}

fn suggestions_len_diff(all: &[Suggestion], kept: &[Suggestion]) -> usize {
    all.len().saturating_sub(kept.len())
}

// =========================================================================
// Curated catalog (hermes cron/suggestion_catalog.py)
// =========================================================================

/// A curated starter automation offered as a suggestion
/// (hermes `CatalogEntry`).
pub struct CatalogEntry {
    /// Stable dedup key (never re-offered once dismissed).
    pub key: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub job_spec: JobSpec,
}

/// The curated starter set (hermes `CATALOG`), prompts adapted to be
/// self-contained for ulnclaw cron runs.
pub fn catalog() -> Vec<CatalogEntry> {
    vec![
        CatalogEntry {
            key: "catalog:daily-briefing",
            title: "Daily briefing",
            description: "Every morning at 8am, a short briefing: today's agenda, \
                          and anything urgent waiting on you.",
            job_spec: JobSpec {
                name: Some("Daily briefing".to_string()),
                prompt: "Produce a concise morning briefing for the user: today's \
                         agenda and any urgent items (due tasks, unread important \
                         messages). Keep it short and scannable. If you have no \
                         connected data sources, give a brief general good-morning \
                         with the date and offer to connect sources."
                    .to_string(),
                schedule: "0 8 * * *".to_string(),
                deliver: Some("origin".to_string()),
                ..Default::default()
            },
        },
        CatalogEntry {
            key: "catalog:important-mail-monitor",
            title: "Important-mail monitor",
            description: "Check your inbox periodically and ping you ONLY about mail \
                          that actually needs attention — never the newsletters.",
            job_spec: JobSpec {
                name: Some("Important-mail monitor".to_string()),
                prompt: "Check the user's inbox for new messages since the last run. \
                         Surface only mail that needs a reply today, is from a \
                         manager/family member, or mentions a deadline. If nothing \
                         clears the bar, respond with [SILENT] so the user is not \
                         pinged. Requires a connected mail source; if none is \
                         configured, explain how to connect one and then stop."
                    .to_string(),
                schedule: "every 30m".to_string(),
                deliver: Some("origin".to_string()),
                ..Default::default()
            },
        },
        CatalogEntry {
            key: "catalog:weekly-review",
            title: "Weekly review",
            description: "Every Sunday evening, a recap of the week: what got done, \
                          what's still open, and what's coming up next week.",
            job_spec: JobSpec {
                name: Some("Weekly review".to_string()),
                prompt: "Produce a weekly review for the user: summarize what was \
                         accomplished this week (recent sessions and diffs), list \
                         still-open items, and preview next week. Keep it tight."
                    .to_string(),
                schedule: "0 18 * * 0".to_string(),
                deliver: Some("origin".to_string()),
                ..Default::default()
            },
        },
        CatalogEntry {
            key: "catalog:standup-reminder",
            title: "Workday start reminder",
            description: "A weekday nudge at 9am with your day's agenda and top \
                          priorities, so you start focused.",
            job_spec: JobSpec {
                name: Some("Workday start reminder".to_string()),
                prompt: "Give the user a brief weekday start-of-day nudge: today's \
                         agenda and the 1-3 highest-priority things to focus on, \
                         inferred from recent context. Encouraging, short, one \
                         message."
                    .to_string(),
                schedule: "0 9 * * 1-5".to_string(),
                deliver: Some("origin".to_string()),
                ..Default::default()
            },
        },
    ]
}

/// Register catalog entries as pending suggestions (hermes
/// `seed_catalog_suggestions`). Safe and idempotent — the store skips
/// decided dedup keys and a full backlog.
pub fn seed_catalog_suggestions(store: &SuggestionStore) -> Vec<Suggestion> {
    let mut created = Vec::new();
    for entry in catalog() {
        if let Ok(Some(record)) = store.add(
            entry.title,
            entry.description,
            "catalog",
            entry.job_spec,
            entry.key,
        ) {
            created.push(record);
        }
    }
    created
}

// =========================================================================
// /suggestions command dispatch (hermes suggestions_cmd.py)
// =========================================================================

fn fmt_pending(pending: &[Suggestion]) -> String {
    if pending.is_empty() {
        return "No suggested automations right now.\nTry `/suggestions catalog` to see the \
                curated starter set, or install a blueprint skill to get one."
            .to_string();
    }
    let mut lines =
        vec!["Suggested automations — `/suggestions accept N` or `dismiss N`:\n".to_string()];
    for (i, suggestion) in pending.iter().enumerate() {
        lines.push(format!(
            "  {}. {}  [{}]  ({})",
            i + 1,
            suggestion.title,
            suggestion.job_spec.schedule,
            suggestion.source
        ));
        let description = suggestion.description.trim();
        if !description.is_empty() {
            lines.push(format!("     {description}"));
        }
    }
    lines.join("\n")
}

/// Dispatch a `/suggestions` invocation; returns text to show the user
/// (hermes `handle_suggestions_command`, CLI surface).
pub fn handle_suggestions_command(args: &str) -> String {
    let store = SuggestionStore::open_default();
    handle_suggestions_command_with(&store, args)
}

/// Store-injectable dispatch core (test-friendly).
pub fn handle_suggestions_command_with(store: &SuggestionStore, args: &str) -> String {
    let mut parts = args.trim().split_whitespace();
    let sub = parts.next().unwrap_or("").to_lowercase();
    let rest: Vec<&str> = parts.collect();
    let rest = rest.join(" ");

    if sub.is_empty() {
        return fmt_pending(&store.list_pending());
    }

    match sub.as_str() {
        "accept" | "add" | "schedule" => {
            if rest.is_empty() {
                return "Usage: /suggestions accept <number|id>".to_string();
            }
            match store.accept(&rest) {
                Ok(Some(job)) => format!(
                    "Scheduled '{}' ({}). Manage it with /cron or `ulnclaw cron`.",
                    job.name, job.schedule
                ),
                Ok(None) => format!(
                    "No pending suggestion matches '{rest}'. Run /suggestions to list them."
                ),
                Err(e) => format!("Could not schedule '{rest}': {e}"),
            }
        }
        "dismiss" | "no" | "reject" => {
            if rest.is_empty() {
                return "Usage: /suggestions dismiss <number|id>".to_string();
            }
            if store.dismiss(&rest) {
                "Dismissed. Won't suggest that again.".to_string()
            } else {
                format!("No pending suggestion matches '{rest}'.")
            }
        }
        "catalog" => {
            let created = seed_catalog_suggestions(store);
            if created.is_empty() {
                return "No new catalog automations to add (already offered, dismissed, \
                        or your suggestion list is full). Run /suggestions to see pending."
                    .to_string();
            }
            let titles: Vec<&str> = created.iter().map(|c| c.title.as_str()).collect();
            format!(
                "Added {} suggestion(s): {}.\nRun /suggestions to review.",
                created.len(),
                titles.join(", ")
            )
        }
        "clear" => {
            let removed = store.clear_resolved();
            format!("Cleared {removed} resolved suggestion record(s).")
        }
        _ => "Usage:\n  /suggestions              list pending\n  /suggestions accept N     \
              schedule suggestion N\n  /suggestions dismiss N    dismiss suggestion N\n  \
              /suggestions catalog      add curated starter automations\n  /suggestions \
              clear        housekeeping"
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn temp_store(dir: &Path) -> SuggestionStore {
        SuggestionStore::open(dir.join("suggestions.json"))
    }

    fn spec(schedule: &str) -> JobSpec {
        JobSpec {
            name: Some("Test job".to_string()),
            prompt: "do the thing".to_string(),
            schedule: schedule.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn add_dedup_and_cap() {
        let dir = tempfile::tempdir().unwrap();
        let store = temp_store(dir.path());
        let first = store
            .add("T1", "d", "catalog", spec("every 1h"), "k1")
            .unwrap();
        assert!(first.is_some());
        // Identical dedup key → skipped.
        assert!(store
            .add("T1 dup", "d", "catalog", spec("every 1h"), "k1")
            .unwrap()
            .is_none());
        // Fill to MAX_PENDING.
        for i in 2..=MAX_PENDING {
            assert!(store
                .add(
                    &format!("T{i}"),
                    "d",
                    "usage",
                    spec("every 1h"),
                    &format!("k{i}")
                )
                .unwrap()
                .is_some());
        }
        // Backlog full → dropped.
        assert!(store
            .add("T extra", "d", "usage", spec("every 1h"), "k-extra")
            .unwrap()
            .is_none());
        assert_eq!(store.list_pending().len(), MAX_PENDING);
        // Unknown source → error.
        assert!(store.add("X", "d", "nope", spec("every 1h"), "kx").is_err());
        // Empty title → error.
        assert!(store
            .add(" ", "d", "catalog", spec("every 1h"), "ky")
            .is_err());
    }

    #[test]
    fn resolve_by_index_id_and_title() {
        let dir = tempfile::tempdir().unwrap();
        let store = temp_store(dir.path());
        store
            .add("Alpha", "", "catalog", spec("every 1h"), "a")
            .unwrap();
        store
            .add("Beta Job", "", "blueprint", spec("every 2h"), "b")
            .unwrap();
        assert_eq!(store.get("1").unwrap().title, "Alpha");
        assert_eq!(store.get("2").unwrap().title, "Beta Job");
        let id = store.get("1").unwrap().id;
        assert_eq!(store.get(&id).unwrap().title, "Alpha");
        assert_eq!(store.get("beta job").unwrap().title, "Beta Job");
        assert!(store.get("99").is_none());
        assert!(store.get("missing").is_none());
    }

    #[test]
    fn dismiss_latches_dedup_key() {
        let dir = tempfile::tempdir().unwrap();
        let store = temp_store(dir.path());
        store
            .add("Alpha", "", "catalog", spec("every 1h"), "a")
            .unwrap();
        assert!(store.dismiss("1"));
        assert!(store.list_pending().is_empty());
        // Same dedup key never re-offered.
        assert!(store
            .add("Alpha again", "", "catalog", spec("every 1h"), "a")
            .unwrap()
            .is_none());
        // Dismissed records survive clear_resolved (dedup memory).
        assert_eq!(store.clear_resolved(), 0);
        assert_eq!(store.load().len(), 1);
    }

    #[test]
    fn accept_creates_cron_job_and_clears() {
        let _guard = crate::models_dev::test_env_lock();
        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var("ULNCLAW_HOME").ok();
        std::env::set_var("ULNCLAW_HOME", dir.path());
        let store = SuggestionStore::open_default();
        store
            .add("Alpha", "", "catalog", spec("every 1h"), "a")
            .unwrap();
        let job = store.accept("1").unwrap().expect("job created");
        assert_eq!(job.name, "Test job");
        assert!(job.enabled);
        // Not pending anymore → second accept returns None.
        assert!(store.accept("Alpha").unwrap().is_none());
        // clear_resolved prunes the accepted record.
        assert_eq!(store.clear_resolved(), 1);
        assert!(store.load().is_empty());
        // The job landed in the default cron store.
        let cron = crate::cron::CronStore::open_default().unwrap();
        assert!(cron.list().unwrap().iter().any(|j| j.id == job.id));
        match prev {
            Some(v) => std::env::set_var("ULNCLAW_HOME", v),
            None => std::env::remove_var("ULNCLAW_HOME"),
        }
    }

    #[test]
    fn command_dispatch_flows() {
        let dir = tempfile::tempdir().unwrap();
        let store = temp_store(dir.path());
        // Empty list text.
        let out = handle_suggestions_command_with(&store, "");
        assert!(out.contains("No suggested automations"));
        // Seed catalog.
        let out = handle_suggestions_command_with(&store, "catalog");
        assert!(out.starts_with("Added 4 suggestion(s)"));
        // Re-seed is idempotent.
        let out = handle_suggestions_command_with(&store, "catalog");
        assert!(out.contains("No new catalog automations"));
        // List shows numbered entries.
        let out = handle_suggestions_command_with(&store, "");
        assert!(out.contains("1. Daily briefing"));
        // Dismiss by index.
        let out = handle_suggestions_command_with(&store, "dismiss 1");
        assert!(out.contains("Dismissed"));
        let out = handle_suggestions_command_with(&store, "dismiss 99");
        assert!(out.contains("No pending suggestion"));
        // Unknown subcommand → usage.
        let out = handle_suggestions_command_with(&store, "bogus");
        assert!(out.contains("Usage:"));
    }

    #[test]
    fn catalog_entries_have_valid_schedules() {
        for entry in catalog() {
            crate::cron::parse_schedule(&entry.job_spec.schedule)
                .unwrap_or_else(|e| panic!("{}: {}", entry.key, e));
            assert!(!entry.key.starts_with(" "));
            assert!(!entry.title.is_empty());
        }
    }
}

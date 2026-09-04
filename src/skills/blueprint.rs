//! Blueprints — schedulable skills. Port of hermes' `tools/blueprints.py`.
//!
//! A blueprint is NOT a new object type: it is an ordinary skill whose
//! SKILL.md frontmatter declares an automation schedule under
//! `metadata.hermes.blueprint`:
//!
//! ```yaml
//! metadata:
//!   hermes:
//!     blueprint:
//!       schedule: "0 9 * * *"   # presence of `blueprint:` marks it runnable
//!       deliver: origin         # optional (default "origin")
//!       prompt: "..."           # optional task instruction for the run
//! ```
//!
//! The bridge to cron is `blueprint_to_job`; ulnclaw exposes it through
//! `ulnclaw skills schedule <name>` (hermes registers a suggestion instead
//! of auto-scheduling — ulnclaw's explicit command is the equivalent).

use crate::cron::CronJob;
use std::collections::HashMap;
use std::path::Path;

/// Parsed `metadata.hermes.blueprint` automation spec for a skill.
#[derive(Debug, Clone, PartialEq)]
pub struct BlueprintSpec {
    pub skill_name: String,
    pub schedule: String,
    pub deliver: String,
    pub prompt: Option<String>,
}

/// Flatten YAML-ish frontmatter into dotted-path → value entries.
///
/// Returns `None` when the frontmatter fences are missing. Container keys
/// (lines ending in a bare `:`) are recorded in the second map so empty
/// blocks are still detectable.
fn flatten_frontmatter(content: &str) -> Option<(HashMap<String, String>, HashMap<String, ()>)> {
    let stripped = content.trim_start_matches('\u{feff}').trim_start();
    let rest = stripped.strip_prefix("---")?;
    let end = rest.find("\n---")?;
    let mut values: HashMap<String, String> = HashMap::new();
    let mut containers: HashMap<String, ()> = HashMap::new();
    let mut stack: Vec<(usize, String)> = Vec::new();
    for raw_line in rest[..end].lines() {
        if raw_line.trim().is_empty() || raw_line.trim_start().starts_with('#') {
            continue;
        }
        let indent = raw_line.len() - raw_line.trim_start().len();
        let line = raw_line.trim();
        while let Some(&(top_indent, _)) = stack.last() {
            if top_indent >= indent {
                stack.pop();
            } else {
                break;
            }
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim().trim_matches('"').trim_matches('\'');
        let value = value.trim();
        let mut path: Vec<&str> = stack.iter().map(|(_, key)| key.as_str()).collect();
        path.push(key);
        let dotted = path.join(".");
        if value.is_empty() {
            containers.insert(dotted, ());
            stack.push((indent, key.to_string()));
        } else {
            let clean = value.trim_matches('"').trim_matches('\'').to_string();
            values.insert(dotted, clean);
        }
    }
    Some((values, containers))
}

const BLUEPRINT_PREFIX: &str = "metadata.hermes.blueprint";

/// Extract a `BlueprintSpec` from a SKILL.md string, or `None` when the
/// skill is not a blueprint. A present-but-malformed blueprint block is an
/// error (hermes `BlueprintError`), so typos surface instead of silently
/// no-op'ing.
pub fn parse_blueprint(content: &str) -> Result<Option<BlueprintSpec>, String> {
    let Some((values, containers)) = flatten_frontmatter(content) else {
        return Ok(None);
    };
    let has_block = containers.contains_key(BLUEPRINT_PREFIX)
        || values
            .keys()
            .any(|key| key.starts_with(&format!("{}.", BLUEPRINT_PREFIX)));
    if !has_block {
        return Ok(None);
    }
    let skill_name = values.get("name").cloned().unwrap_or_default();
    let schedule = values
        .get(&format!("{}.schedule", BLUEPRINT_PREFIX))
        .map(|value: &String| value.trim())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "blueprint.schedule is required and must be non-empty".to_string())?
        .to_string();
    let deliver = values
        .get(&format!("{}.deliver", BLUEPRINT_PREFIX))
        .map(|value: &String| value.trim())
        .filter(|value| !value.is_empty())
        .unwrap_or("origin")
        .to_string();
    let prompt = values
        .get(&format!("{}.prompt", BLUEPRINT_PREFIX))
        .map(|value: &String| value.trim())
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    Ok(Some(BlueprintSpec {
        skill_name,
        schedule,
        deliver,
        prompt,
    }))
}

/// Locate an installed skill's SKILL.md and parse its blueprint block
/// (hermes `blueprint_spec_for_installed`).
pub fn blueprint_spec_for_installed(skills_dir: &Path, skill_name: &str) -> Option<BlueprintSpec> {
    let direct = skills_dir.join(skill_name).join("SKILL.md");
    let mut candidates = vec![direct];
    if let Ok(entries) = std::fs::read_dir(skills_dir) {
        for entry in entries.flatten() {
            let nested = entry.path().join(skill_name).join("SKILL.md");
            candidates.push(nested);
        }
    }
    for path in candidates {
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Ok(Some(mut spec)) = parse_blueprint(&content) {
            if spec.skill_name.is_empty() {
                spec.skill_name = skill_name.to_string();
            }
            return Some(spec);
        }
    }
    None
}

/// Translate a blueprint into a cron job (hermes `blueprint_to_job_spec` +
/// `create_blueprint_job`, adapted to ulnclaw's cron schema).
pub fn blueprint_to_job(
    spec: &BlueprintSpec,
    name_override: Option<&str>,
) -> Result<CronJob, String> {
    if spec.skill_name.is_empty() {
        return Err("blueprint has no skill name".to_string());
    }
    let schedule = crate::cron::parse_schedule(&spec.schedule)
        .map_err(|e| format!("blueprint schedule '{}': {}", spec.schedule, e))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    Ok(CronJob {
        id: uuid::Uuid::new_v4().to_string(),
        name: name_override
            .map(str::to_string)
            .unwrap_or_else(|| format!("blueprint:{}", spec.skill_name)),
        schedule: spec.schedule.clone(),
        prompt: spec.prompt.clone().unwrap_or_else(|| {
            format!(
                "Run the '{}' skill now and report the results.",
                spec.skill_name
            )
        }),
        skills: vec![spec.skill_name.clone()],
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
    })
}

/// Register a pending "schedule this blueprint" suggestion (hermes
/// `register_blueprint_suggestion`). Blueprints are the unified
/// suggestion surface's `blueprint` source: an installed blueprint is
/// offered as a suggestion the user accepts (or dismisses) rather than
/// auto-scheduled. hermes wires this into the skill-install flow;
/// ulnclaw has no install surface, so `skills blueprints` registers
/// lazily instead — dedup latching in the store makes that idempotent.
///
/// Returns the created record, or `None` when skipped (already
/// offered/decided, backlog full, empty skill name).
pub fn register_blueprint_suggestion(
    store: &crate::cron::suggestions::SuggestionStore,
    spec: &BlueprintSpec,
) -> Option<crate::cron::suggestions::Suggestion> {
    if spec.skill_name.is_empty() {
        return None;
    }
    let mut description = format!(
        "The '{}' blueprint runs on schedule {}",
        spec.skill_name, spec.schedule
    );
    if !spec.deliver.is_empty() && spec.deliver != "origin" {
        description.push_str(&format!(", delivering to {}", spec.deliver));
    }
    description.push('.');
    let job_spec = crate::cron::suggestions::JobSpec {
        name: Some(format!("blueprint:{}", spec.skill_name)),
        prompt: spec.prompt.clone().unwrap_or_else(|| {
            format!(
                "Run the '{}' skill now and report the results.",
                spec.skill_name
            )
        }),
        schedule: spec.schedule.clone(),
        skills: vec![spec.skill_name.clone()],
        deliver: if spec.deliver.is_empty() || spec.deliver == "origin" {
            None
        } else {
            Some(spec.deliver.clone())
        },
    };
    store
        .add(
            &format!("Schedule '{}'", spec.skill_name),
            &description,
            "blueprint",
            job_spec,
            &format!("blueprint:{}:{}", spec.skill_name, spec.schedule),
        )
        .ok()
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLUEPRINT_SKILL: &str = r#"---
name: daily-digest
description: Summarize the day's sessions
metadata:
  hermes:
    tags: automation
    blueprint:
      schedule: "0 9 * * *"
      deliver: local
      prompt: "Compile today's session digest."
---

# Daily digest
Body text here.
"#;

    #[test]
    fn parses_blueprint_block() {
        let spec = parse_blueprint(BLUEPRINT_SKILL)
            .unwrap()
            .expect("blueprint");
        assert_eq!(spec.skill_name, "daily-digest");
        assert_eq!(spec.schedule, "0 9 * * *");
        assert_eq!(spec.deliver, "local");
        assert_eq!(
            spec.prompt.as_deref(),
            Some("Compile today's session digest.")
        );
    }

    #[test]
    fn plain_skill_is_not_a_blueprint() {
        let content = "---\nname: plain\ndescription: no schedule\n---\nbody\n";
        assert!(parse_blueprint(content).unwrap().is_none());
        assert!(parse_blueprint("no frontmatter at all").unwrap().is_none());
    }

    #[test]
    fn blueprint_defaults() {
        let content = "---\nname: min\nmetadata:\n  hermes:\n    blueprint:\n      schedule: every 30m\n---\n";
        let spec = parse_blueprint(content).unwrap().expect("blueprint");
        assert_eq!(spec.schedule, "every 30m");
        assert_eq!(spec.deliver, "origin");
        assert!(spec.prompt.is_none());
    }

    #[test]
    fn missing_schedule_is_an_error() {
        let content =
            "---\nname: bad\nmetadata:\n  hermes:\n    blueprint:\n      deliver: local\n---\n";
        let error = parse_blueprint(content).err().expect("must error");
        assert!(error.contains("schedule"));
    }

    #[test]
    fn spec_for_installed_skill() {
        let dir = std::env::temp_dir().join(format!(
            "ulnclaw-blueprint-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let skill_dir = dir.join("daily-digest");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), BLUEPRINT_SKILL).unwrap();
        let spec = blueprint_spec_for_installed(&dir, "daily-digest").expect("found");
        assert_eq!(spec.schedule, "0 9 * * *");
        assert!(blueprint_spec_for_installed(&dir, "missing").is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn job_translation() {
        let spec = BlueprintSpec {
            skill_name: "daily-digest".into(),
            schedule: "every 2h".into(),
            deliver: "origin".into(),
            prompt: Some("do the thing".into()),
        };
        let job = blueprint_to_job(&spec, None).unwrap();
        assert_eq!(job.name, "blueprint:daily-digest");
        assert_eq!(job.skills, vec!["daily-digest".to_string()]);
        assert_eq!(job.prompt, "do the thing");
        assert!(job.enabled);
        assert!(job.next_run.is_some());
        let custom = blueprint_to_job(&spec, Some("my-digest")).unwrap();
        assert_eq!(custom.name, "my-digest");
    }

    #[test]
    fn invalid_schedule_rejected() {
        let spec = BlueprintSpec {
            skill_name: "x".into(),
            schedule: "not a schedule".into(),
            deliver: "origin".into(),
            prompt: None,
        };
        assert!(blueprint_to_job(&spec, None).is_err());
    }

    fn digest_spec() -> BlueprintSpec {
        BlueprintSpec {
            skill_name: "daily-digest".into(),
            schedule: "0 9 * * *".into(),
            deliver: "origin".into(),
            prompt: Some("Summarize the inbox.".into()),
        }
    }

    #[test]
    fn register_blueprint_suggestion_creates_pending_record() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::cron::suggestions::SuggestionStore::open(dir.path().join("s.json"));
        let record = register_blueprint_suggestion(&store, &digest_spec()).expect("registered");
        assert_eq!(record.title, "Schedule 'daily-digest'");
        assert_eq!(record.source, "blueprint");
        assert_eq!(record.dedup_key, "blueprint:daily-digest:0 9 * * *");
        assert_eq!(record.status, "pending");
        assert!(
            record.description.contains("runs on schedule 0 9 * * *"),
            "{}",
            record.description
        );
        // origin delivery stays implicit (hermes only surfaces non-origin).
        assert!(!record.description.contains("delivering"));
        // Accepted suggestions must produce the same job as `skills schedule`.
        assert_eq!(
            record.job_spec.name.as_deref(),
            Some("blueprint:daily-digest")
        );
        assert_eq!(record.job_spec.skills, vec!["daily-digest".to_string()]);
        assert!(record.job_spec.deliver.is_none());
    }

    #[test]
    fn register_blueprint_suggestion_dedup_latches() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::cron::suggestions::SuggestionStore::open(dir.path().join("s.json"));
        assert!(register_blueprint_suggestion(&store, &digest_spec()).is_some());
        assert!(
            register_blueprint_suggestion(&store, &digest_spec()).is_none(),
            "second registration must latch"
        );
        assert_eq!(store.list_pending().len(), 1);
        // Dismissal latches too — the blueprint is never re-offered.
        assert!(store.dismiss("1"));
        assert!(register_blueprint_suggestion(&store, &digest_spec()).is_none());
    }

    #[test]
    fn register_blueprint_suggestion_nonorigin_delivery_surfaces() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::cron::suggestions::SuggestionStore::open(dir.path().join("s.json"));
        let mut spec = digest_spec();
        spec.deliver = "telegram".into();
        let record = register_blueprint_suggestion(&store, &spec).expect("registered");
        assert!(record.description.contains("delivering to telegram"));
        assert_eq!(record.job_spec.deliver.as_deref(), Some("telegram"));
    }

    #[test]
    fn register_blueprint_suggestion_requires_skill_name() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::cron::suggestions::SuggestionStore::open(dir.path().join("s.json"));
        let mut spec = digest_spec();
        spec.skill_name = String::new();
        assert!(register_blueprint_suggestion(&store, &spec).is_none());
    }
}

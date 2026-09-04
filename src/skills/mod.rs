//! Skills — port of hermes' skills system (tools/skills_tool.py)
//!
//! A skill is a directory `<home>/skills/<name>/SKILL.md` with YAML-ish
//! frontmatter (name, description) plus optional linked files under
//! references/, templates/, scripts/.

pub mod blueprint;
pub mod guard;

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub category: String,
    pub path: PathBuf,
}

/// Parse YAML-ish frontmatter between `---` fences.
fn parse_frontmatter(content: &str) -> (String, String) {
    let mut name = String::new();
    let mut description = String::new();
    let trimmed = content.trim_start();
    if let Some(rest) = trimmed.strip_prefix("---") {
        for line in rest.lines() {
            let line = line.trim();
            if line == "---" {
                break;
            }
            if let Some((key, value)) = line.split_once(':') {
                let value = value
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'')
                    .to_string();
                match key.trim() {
                    "name" => name = value,
                    "description" => description = value,
                    _ => {}
                }
            }
        }
    }
    (name, description)
}

/// List all skills found in a skills directory.
pub fn list_skills(skills_dir: &Path) -> Vec<Skill> {
    let mut skills = Vec::new();
    let Ok(entries) = std::fs::read_dir(skills_dir) else {
        return skills;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let skill_md = path.join("SKILL.md");
        if !skill_md.exists() {
            continue;
        }
        let content = std::fs::read_to_string(&skill_md).unwrap_or_default();
        let (mut name, description) = parse_frontmatter(&content);
        if name.is_empty() {
            name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
        }
        skills.push(Skill {
            name,
            description,
            category: "general".to_string(),
            path,
        });
    }
    skills
}

/// Parse `required_environment_variables` from SKILL.md frontmatter.
///
/// Accepts a comma-separated string (`VAR1, VAR2`) or an inline YAML
/// list (`[VAR1, VAR2]`). Skills declare the env vars their scripts
/// need; `skill_view` registers them as sandbox passthrough (provider
/// credentials are refused — hermes GHSA-rhgp-j443-p4rf).
pub fn required_env_vars(content: &str) -> Vec<String> {
    let trimmed = content.trim_start();
    let Some(rest) = trimmed.strip_prefix("---") else {
        return Vec::new();
    };
    for line in rest.lines() {
        let line = line.trim();
        if line == "---" {
            break;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if key.trim() != "required_environment_variables" {
            continue;
        }
        let value = value.trim();
        let inner = value
            .strip_prefix('[')
            .and_then(|v| v.strip_suffix(']'))
            .unwrap_or(value);
        return inner
            .split(',')
            .map(|item| {
                item.trim()
                    .trim_matches('"')
                    .trim_matches('\'')
                    .trim()
                    .to_string()
            })
            .filter(|item| !item.is_empty())
            .collect();
    }
    Vec::new()
}

/// Find a skill by name (case-insensitive).
pub fn find_skill(skills_dir: &Path, name: &str) -> Option<Skill> {
    list_skills(skills_dir)
        .into_iter()
        .find(|skill| skill.name.eq_ignore_ascii_case(name))
}

/// Collect linked files relative to the skill directory
/// (references/, templates/, scripts/, assets/).
pub fn linked_files(skill_path: &Path) -> Vec<String> {
    let mut files = Vec::new();
    for subdir in ["references", "templates", "scripts", "assets"] {
        let dir = skill_path.join(subdir);
        if !dir.is_dir() {
            continue;
        }
        for entry in walkdir::WalkDir::new(&dir).into_iter().flatten() {
            if entry.file_type().is_file() {
                if let Ok(rel) = entry.path().strip_prefix(skill_path) {
                    files.push(rel.display().to_string());
                }
            }
        }
    }
    files.sort();
    files
}

/// Build the scaffolded user message for a `/skill-name` slash invocation
/// (hermes `build_skill_invocation_message` + `_build_skill_message`).
/// Embeds the whole skill body behind the canonical activation note so the
/// agent follows the skill, injects the skill directory + supporting-file
/// hints, and appends the user's instruction marker. Returns `None` when
/// the skill does not exist.
pub fn build_skill_invocation_message(
    skills_dir: &Path,
    name: &str,
    user_instruction: &str,
) -> Option<String> {
    let skill = find_skill(skills_dir, name)?;
    let content = std::fs::read_to_string(skill.path.join("SKILL.md")).unwrap_or_default();
    // Track active usage for curator lifecycle (hermes bump_use #17782).
    if let Some(home) = skills_dir.parent() {
        crate::skill_usage::bump_use(home, &skill.name);
    }
    let activation_note = format!(
        "[IMPORTANT: The user has invoked the \"{}\" skill, indicating they want \
         you to follow its instructions. The full skill content is loaded below.]",
        skill.name
    );
    let mut parts: Vec<String> = vec![activation_note, String::new(), content.trim().to_string()];
    parts.push(String::new());
    parts.push(format!("[Skill directory: {}]", skill.path.display()));
    parts.push(
        "Resolve any relative paths in this skill (e.g. `scripts/foo.js`, \
         `templates/config.yaml`) against that directory, then run them \
         with the terminal tool using the absolute path."
            .to_string(),
    );
    let missing_env: Vec<String> = required_env_vars(&content)
        .into_iter()
        .filter(|var| std::env::var(var).map(|v| v.is_empty()).unwrap_or(true))
        .collect();
    if !missing_env.is_empty() {
        parts.push(String::new());
        parts.push(format!(
            "[Skill setup note: required environment variables are not set: {}. \
             Continue and note any reduced functionality.]",
            missing_env.join(", ")
        ));
    }
    let supporting = linked_files(&skill.path);
    if !supporting.is_empty() {
        parts.push(String::new());
        parts.push("[This skill has supporting files:]".to_string());
        for rel in &supporting {
            parts.push(format!("- {}  ->  {}", rel, skill.path.join(rel).display()));
        }
        parts.push(format!(
            "\nLoad any of these with skill_view(name=\"{}\", file_path=\"<path>\"), \
             or run scripts directly by absolute path (e.g. `node {}/scripts/foo.js`).",
            skill.name,
            skill.path.display()
        ));
    }
    let instruction = user_instruction.trim();
    if !instruction.is_empty() {
        parts.push(String::new());
        parts.push(format!(
            "The user has provided the following instruction alongside the skill invocation: {}",
            instruction
        ));
    }
    Some(parts.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_skill_discovery() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("deploy-helper");
        std::fs::create_dir_all(skill_dir.join("references")).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: deploy-helper\ndescription: Helps with deployments\n---\n\n# Steps\n1. build\n",
        )
        .unwrap();
        std::fs::write(skill_dir.join("references/runbook.md"), "# runbook").unwrap();

        let skills = list_skills(dir.path());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "deploy-helper");
        assert_eq!(skills[0].description, "Helps with deployments");

        let files = linked_files(&skills[0].path);
        assert_eq!(files, vec!["references/runbook.md"]);

        assert!(find_skill(dir.path(), "DEPLOY-helper").is_some());
        assert!(find_skill(dir.path(), "nope").is_none());
    }

    #[test]
    fn test_required_env_vars_parsing() {
        // Comma-separated string form.
        let content = "---\nname: x\ndescription: d\nrequired_environment_variables: TENOR_API_KEY, NOTION_TOKEN\n---\nbody";
        assert_eq!(
            required_env_vars(content),
            vec!["TENOR_API_KEY".to_string(), "NOTION_TOKEN".to_string()]
        );
        // Inline YAML list form.
        let content = "---\nrequired_environment_variables: [A_KEY, B_KEY]\n---\n";
        assert_eq!(
            required_env_vars(content),
            vec!["A_KEY".to_string(), "B_KEY".to_string()]
        );
        // Absent.
        assert_eq!(
            required_env_vars("---\nname: x\n---\n"),
            Vec::<String>::new()
        );
        // No frontmatter.
        assert_eq!(required_env_vars("plain body"), Vec::<String>::new());
    }

    #[test]
    fn skill_invocation_message_scaffolds_hermes_markers() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("work");
        std::fs::create_dir_all(skill_dir.join("scripts")).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: work\ndescription: Do the work\n---\n\nDo the thing.\n",
        )
        .unwrap();
        std::fs::write(skill_dir.join("scripts/run.sh"), "#!/bin/sh\necho ok").unwrap();

        let message =
            build_skill_invocation_message(dir.path(), "work", "fix the title leak").unwrap();
        assert!(message.starts_with(
            "[IMPORTANT: The user has invoked the \"work\" skill, indicating they want"
        ));
        assert!(message.contains("The full skill content is loaded below.]"));
        assert!(message.contains("Do the thing."));
        assert!(message.contains(&format!("[Skill directory: {}]", skill_dir.display())));
        assert!(message.contains("scripts/run.sh"));
        assert!(message.ends_with(
            "The user has provided the following instruction alongside the skill invocation: fix the title leak"
        ));
        // Round-trips through the retitle describe surface.
        assert_eq!(
            crate::session::retitle::describe_skill_invocation(&message).as_deref(),
            Some("/work — fix the title leak")
        );

        // Bare invocation: no instruction marker; case-insensitive lookup.
        let bare = build_skill_invocation_message(dir.path(), "WORK", "  ").unwrap();
        assert!(!bare.contains("alongside the skill invocation"));
        assert!(build_skill_invocation_message(dir.path(), "nope", "").is_none());
    }
}

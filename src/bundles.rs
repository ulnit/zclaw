//! Skill bundles — aliases that load multiple skills under one slash
//! command (hermes `agent/skill_bundles.py` + `hermes_cli/bundles.py` parity).
//!
//! A bundle is a small YAML file in `<home>/skill-bundles/` naming a set of
//! skills to load together. Invoking `/<bundle>` loads every referenced
//! skill's full SKILL.md into a single user message — hermes semantics:
//! missing skills are skipped with a note, bundles win over same-named
//! skills in slash dispatch, and hyphens/underscores are interchangeable.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// On-disk bundle file shape (hermes bundle YAML).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BundleFile {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub instruction: Option<String>,
}

/// Parsed, validated bundle (hermes bundle info dict).
#[derive(Debug, Clone, PartialEq)]
pub struct BundleInfo {
    pub name: String,
    pub slug: String,
    pub description: String,
    pub skills: Vec<String>,
    pub instruction: String,
    pub path: PathBuf,
}

/// Diff reported by a rescan (hermes `reload_bundles`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReloadDiff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub unchanged: Vec<String>,
    pub total: usize,
}

/// Canonical bundles directory (hermes `_bundles_dir`; honors
/// `ULNCLAW_BUNDLES_DIR` for tests, mirroring `HERMES_BUNDLES_DIR`).
pub fn bundles_dir() -> PathBuf {
    if let Some(override_dir) = std::env::var_os("ULNCLAW_BUNDLES_DIR") {
        return PathBuf::from(override_dir);
    }
    crate::config::ulnclaw_home().join("skill-bundles")
}

/// Slug normalization (hermes `_slugify`, matching skill-command behavior).
pub fn slugify(name: &str) -> String {
    let lower = name.to_lowercase().replace(' ', "-").replace('_', "-");
    let cleaned: String = lower
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-')
        .collect();
    let mut out = String::new();
    let mut prev_hyphen = false;
    for ch in cleaned.chars() {
        if ch == '-' {
            if !prev_hyphen {
                out.push(ch);
            }
            prev_hyphen = true;
        } else {
            out.push(ch);
            prev_hyphen = false;
        }
    }
    out.trim_matches('-').to_string()
}

fn iter_bundle_files() -> Vec<PathBuf> {
    let base = bundles_dir();
    let mut files = Vec::new();
    let Ok(entries) = std::fs::read_dir(&base) else {
        return files;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            if ext == "yaml" || ext == "yml" {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

/// Parse one bundle file; returns `None` on any error (a broken bundle must
/// not take down slash-command discovery — hermes `_load_bundle_file`).
pub fn load_bundle_file(path: &Path) -> Option<BundleInfo> {
    let raw = std::fs::read_to_string(path).ok()?;
    let data: BundleFile = serde_yaml::from_str(&raw).ok()?;

    let stem = path.file_stem()?.to_string_lossy().to_string();
    let name = data.name.unwrap_or_default().trim().to_string();
    let name = if name.is_empty() { stem } else { name };

    let skills: Vec<String> = data
        .skills
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if skills.is_empty() {
        return None;
    }

    let slug = slugify(&name);
    if slug.is_empty() {
        return None;
    }

    Some(BundleInfo {
        name,
        slug: slug.clone(),
        description: data
            .description
            .map(|d| d.trim().to_string())
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| format!("Load {} skills as a bundle", skills.len())),
        skills,
        instruction: data.instruction.unwrap_or_default().trim().to_string(),
        path: path.to_path_buf(),
    })
}

/// Scan the bundles directory: `"/slug"` → info. Duplicate slugs keep the
/// first file alphabetically (hermes `scan_bundles`).
pub fn scan_bundles() -> BTreeMap<String, BundleInfo> {
    let mut out = BTreeMap::new();
    for path in iter_bundle_files() {
        let Some(info) = load_bundle_file(&path) else {
            continue;
        };
        let key = format!("/{}", info.slug);
        out.entry(key).or_insert(info);
    }
    out
}

/// Resolve a user-typed command to its canonical bundle slash key; hyphens
/// and underscores are interchangeable (hermes
/// `resolve_bundle_command_key`).
pub fn resolve_bundle_command_key(command: &str) -> Option<String> {
    if command.is_empty() {
        return None;
    }
    let key = format!("/{}", command.replace('_', "-"));
    if scan_bundles().contains_key(&key) {
        Some(key)
    } else {
        None
    }
}

/// Sorted bundle list for display (hermes `list_bundles`).
pub fn list_bundles() -> Vec<BundleInfo> {
    scan_bundles().into_values().collect()
}

/// Look up a bundle by name, slug-normalized (hermes `get_bundle`).
pub fn get_bundle(name: &str) -> Option<BundleInfo> {
    scan_bundles().get(&format!("/{}", slugify(name))).cloned()
}

/// Diff a fresh scan against a previous snapshot (hermes reload diff).
pub fn reload_diff(before: &BTreeMap<String, BundleInfo>) -> ReloadDiff {
    let after = scan_bundles();
    let added: Vec<String> = after
        .keys()
        .filter(|k| !before.contains_key(*k))
        .map(|k| k.trim_start_matches('/').to_string())
        .collect();
    let removed: Vec<String> = before
        .keys()
        .filter(|k| !after.contains_key(*k))
        .map(|k| k.trim_start_matches('/').to_string())
        .collect();
    let unchanged: Vec<String> = after
        .keys()
        .filter(|k| before.contains_key(*k))
        .map(|k| k.trim_start_matches('/').to_string())
        .collect();
    ReloadDiff {
        added,
        removed,
        unchanged,
        total: after.len(),
    }
}

/// Canonical filesystem path for a bundle name (hermes `bundle_path_for`).
pub fn bundle_path_for(name: &str) -> Result<PathBuf, String> {
    let slug = slugify(name);
    if slug.is_empty() {
        return Err(format!("Bundle name {name:?} normalizes to an empty slug"));
    }
    Ok(bundles_dir().join(format!("{slug}.yaml")))
}

/// Write a bundle to disk (hermes `save_bundle`).
pub fn save_bundle(
    name: &str,
    skills: &[String],
    description: &str,
    instruction: &str,
    overwrite: bool,
) -> Result<PathBuf, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("Bundle name is required".to_string());
    }
    let cleaned: Vec<String> = skills
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if cleaned.is_empty() {
        return Err("Bundle must reference at least one skill".to_string());
    }
    let path = bundle_path_for(name)?;
    if path.exists() && !overwrite {
        return Err(format!("Bundle already exists at {}", path.display()));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let mut payload = BundleFile {
        name: Some(name.to_string()),
        skills: cleaned,
        ..Default::default()
    };
    if !description.trim().is_empty() {
        payload.description = Some(description.trim().to_string());
    }
    if !instruction.trim().is_empty() {
        payload.instruction = Some(instruction.trim().to_string());
    }
    let text = serde_yaml::to_string(&payload).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(path)
}

/// Delete a bundle by name (hermes `delete_bundle`).
pub fn delete_bundle(name: &str) -> Result<PathBuf, String> {
    let path = bundle_path_for(name)?;
    if !path.exists() {
        return Err(format!("No bundle at {}", path.display()));
    }
    std::fs::remove_file(&path).map_err(|e| format!("cannot delete {}: {e}", path.display()))?;
    Ok(path)
}

/// Build the user message for a bundle invocation (hermes
/// `build_bundle_invocation_message`). Returns `(message, loaded, missing)`;
/// missing skills are skipped with a note — the forgiving stance hermes
/// uses for `-s` preloading.
pub fn build_bundle_invocation_message(
    cmd_key: &str,
    user_instruction: &str,
    skills_dir: &Path,
) -> Option<(String, Vec<String>, Vec<String>)> {
    let info = scan_bundles().get(cmd_key)?.clone();

    let mut loaded: Vec<String> = Vec::new();
    let mut missing: Vec<String> = Vec::new();
    let mut blocks: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for identifier in &info.skills {
        if !seen.insert(identifier.clone()) {
            continue;
        }
        let Some(skill) = crate::skills::find_skill(skills_dir, identifier) else {
            missing.push(identifier.clone());
            continue;
        };
        let content = std::fs::read_to_string(skill.path.join("SKILL.md")).unwrap_or_default();
        blocks.push(format!(
            "[Loaded as part of the \"{}\" skill bundle.]\n\n## Skill: {}\n{}",
            info.name, skill.name, content
        ));
        loaded.push(skill.name);
    }

    if blocks.is_empty() {
        return None;
    }

    let mut header = vec![
        format!(
            "[IMPORTANT: The user has invoked the \"{}\" skill bundle, loading {} skills \
             together. Treat every skill below as active guidance for this turn.]",
            info.name,
            loaded.len()
        ),
        String::new(),
        format!("Bundle: {}", info.name),
        format!("Skills loaded: {}", loaded.join(", ")),
    ];
    if !missing.is_empty() {
        header.push(format!("Skills missing (skipped): {}", missing.join(", ")));
    }
    if !info.instruction.is_empty() {
        header.push(String::new());
        header.push(format!("Bundle instruction: {}", info.instruction));
    }
    if !user_instruction.trim().is_empty() {
        header.push(String::new());
        header.push(format!("User instruction: {}", user_instruction.trim()));
    }

    let mut parts = vec![header.join("\n")];
    parts.extend(blocks);
    Some((parts.join("\n\n"), loaded, missing))
}

/// Shared `/bundles` dispatch for REPL and gateway (P671 — hermes
/// `/bundles` parity): list installed skill bundles, or show one in
/// full. Invoking a bundle stays `/<slug> [instruction]`.
pub fn run_slash(rest: &str) -> String {
    let mut parts = rest.split_whitespace();
    match parts.next() {
        None | Some("list") => {
            let list = list_bundles();
            if list.is_empty() {
                return format!(
                    "(o_o) no bundles installed. Create one with `ulnclaw bundles create <name> --skill a --skill b`.\nBundles directory: {}\n",
                    bundles_dir().display()
                );
            }
            let mut out = format!("skill bundles ({}):\n", list.len());
            for info in &list {
                out.push_str(&format!(
                    "  /{:<20} {:<24} {:>2} skills  {}\n",
                    info.slug,
                    info.name,
                    info.skills.len(),
                    info.description
                ));
            }
            out
        }
        Some("show") => {
            let Some(name) = parts.next() else {
                return "(o_o) usage: /bundles show <name>\n".to_string();
            };
            let Some(info) = get_bundle(name) else {
                return format!("(._.) bundle '{name}' not found\n");
            };
            let mut out = format!("/{}  {}\n", info.slug, info.name);
            if !info.description.is_empty() {
                out.push_str(&format!("  {}\n", info.description));
            }
            out.push_str(&format!("  skills ({}):\n", info.skills.len()));
            for skill in &info.skills {
                out.push_str(&format!("    - {skill}\n"));
            }
            if !info.instruction.is_empty() {
                out.push_str(&format!("  instruction: {}\n", info.instruction));
            }
            out
        }
        Some(_) => "(o_o) usage: /bundles [list|show <name>]\n".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_bundles_dir<F: FnOnce()>(dir: &Path, f: F) {
        let _guard = crate::models_dev::test_env_lock();
        let prev = std::env::var("ULNCLAW_BUNDLES_DIR").ok();
        std::env::set_var("ULNCLAW_BUNDLES_DIR", dir);
        f();
        match prev {
            Some(v) => std::env::set_var("ULNCLAW_BUNDLES_DIR", v),
            None => std::env::remove_var("ULNCLAW_BUNDLES_DIR"),
        }
    }

    #[test]
    fn slugify_normalizes_names() {
        assert_eq!(slugify("Foo Bar"), "foo-bar");
        assert_eq!(slugify("Hello__World!!"), "hello-world");
        assert_eq!(slugify("  --Weird--  "), "weird");
        assert_eq!(slugify("a  b"), "a-b");
        assert_eq!(slugify("!!!"), "");
    }

    #[test]
    fn load_bundle_file_validates() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("backend.yaml");
        std::fs::write(
            &good,
            "name: Backend Dev\ndescription: feature work\nskills:\n  - review\n  - tdd\ninstruction: go\n",
        )
        .unwrap();
        let info = load_bundle_file(&good).unwrap();
        assert_eq!(info.name, "Backend Dev");
        assert_eq!(info.slug, "backend-dev");
        assert_eq!(info.skills, vec!["review".to_string(), "tdd".to_string()]);
        assert_eq!(info.instruction, "go");

        let no_skills = dir.path().join("empty.yaml");
        std::fs::write(&no_skills, "name: x\nskills: []\n").unwrap();
        assert!(load_bundle_file(&no_skills).is_none());

        let bad_yaml = dir.path().join("bad.yaml");
        std::fs::write(&bad_yaml, "name: [unclosed\n").unwrap();
        assert!(load_bundle_file(&bad_yaml).is_none());

        // Stem fallback when name: is absent.
        let stem = dir.path().join("deploy-flow.yml");
        std::fs::write(&stem, "skills:\n  - ship\n").unwrap();
        let info = load_bundle_file(&stem).unwrap();
        assert_eq!(info.name, "deploy-flow");
        assert_eq!(info.slug, "deploy-flow");
        assert!(info.description.contains("1 skills"));
    }

    #[test]
    fn scan_and_resolve_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        with_bundles_dir(dir.path(), || {
            save_bundle(
                "My Bundle",
                &["a".to_string(), "b".to_string()],
                "desc",
                "",
                false,
            )
            .unwrap();
            let bundles = scan_bundles();
            assert!(bundles.contains_key("/my-bundle"));
            assert_eq!(
                resolve_bundle_command_key("my_bundle"),
                Some("/my-bundle".to_string())
            );
            assert_eq!(resolve_bundle_command_key("nope"), None);
            assert_eq!(get_bundle("MY BUNDLE").unwrap().skills.len(), 2);
            assert_eq!(list_bundles().len(), 1);
        });
    }

    #[test]
    fn save_requires_overwrite_and_delete_works() {
        let dir = tempfile::tempdir().unwrap();
        with_bundles_dir(dir.path(), || {
            let skills = vec!["a".to_string()];
            save_bundle("dup", &skills, "", "", false).unwrap();
            let err = save_bundle("dup", &skills, "", "", false).unwrap_err();
            assert!(err.contains("already exists"), "{err}");
            save_bundle("dup", &skills, "", "", true).unwrap();

            let err = save_bundle("no skills", &[], "", "", false).unwrap_err();
            assert!(err.contains("at least one skill"), "{err}");
            let err = save_bundle("!!!", &skills, "", "", false).unwrap_err();
            assert!(err.contains("empty slug"), "{err}");

            let deleted = delete_bundle("dup").unwrap();
            assert!(!deleted.exists());
            let err = delete_bundle("dup").unwrap_err();
            assert!(err.contains("No bundle"), "{err}");
        });
    }

    #[test]
    fn reload_diff_reports_changes() {
        let dir = tempfile::tempdir().unwrap();
        with_bundles_dir(dir.path(), || {
            let before = scan_bundles();
            save_bundle("one", &["a".to_string()], "", "", false).unwrap();
            let diff = reload_diff(&before);
            assert_eq!(diff.added, vec!["one".to_string()]);
            assert!(diff.removed.is_empty());
            assert_eq!(diff.total, 1);

            let with_one = scan_bundles();
            delete_bundle("one").unwrap();
            let diff = reload_diff(&with_one);
            assert_eq!(diff.removed, vec!["one".to_string()]);
            assert!(diff.added.is_empty());
            assert_eq!(diff.total, 0);
        });
    }

    #[test]
    fn invocation_message_loads_and_reports_missing() {
        let dir = tempfile::tempdir().unwrap();
        with_bundles_dir(dir.path(), || {
            // Two installed skills + one missing.
            let skills_dir = dir.path().join("skills");
            for name in ["review", "tdd"] {
                let skill = skills_dir.join(name);
                std::fs::create_dir_all(&skill).unwrap();
                std::fs::write(
                    skill.join("SKILL.md"),
                    format!("---\nname: {name}\n---\n{name} body"),
                )
                .unwrap();
            }
            save_bundle(
                "devflow",
                &["review".to_string(), "tdd".to_string(), "ghost".to_string()],
                "",
                "be careful",
                false,
            )
            .unwrap();

            let (message, loaded, missing) =
                build_bundle_invocation_message("/devflow", "ship it", &skills_dir).unwrap();
            assert_eq!(loaded, vec!["review".to_string(), "tdd".to_string()]);
            assert_eq!(missing, vec!["ghost".to_string()]);
            assert!(message.contains("skill bundle"), "{message}");
            assert!(message.contains("Skills loaded: review, tdd"), "{message}");
            assert!(
                message.contains("Skills missing (skipped): ghost"),
                "{message}"
            );
            assert!(
                message.contains("Bundle instruction: be careful"),
                "{message}"
            );
            assert!(message.contains("User instruction: ship it"), "{message}");
            assert!(message.contains("review body"), "{message}");
            assert!(message.contains("tdd body"), "{message}");

            assert!(build_bundle_invocation_message("/nope", "", &skills_dir).is_none());
        });
    }

    #[test]
    fn invocation_all_missing_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        with_bundles_dir(dir.path(), || {
            save_bundle("ghosts", &["x".to_string(), "y".to_string()], "", "", false).unwrap();
            let skills_dir = dir.path().join("skills");
            std::fs::create_dir_all(&skills_dir).unwrap();
            assert!(build_bundle_invocation_message("/ghosts", "", &skills_dir).is_none());
        });
    }

    #[test]
    fn bundles_slash_list_show_usage() {
        // P671: /bundles slash over a temp bundles dir.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("backend.yaml"),
            "name: Backend Dev\ndescription: feature work\nskills:\n  - review\n  - tdd\ninstruction: go\n",
        )
        .unwrap();
        with_bundles_dir(dir.path(), || {
            let out = run_slash("");
            assert!(out.contains("skill bundles (1)"), "{out}");
            assert!(out.contains("/backend-dev"), "{out}");

            let out = run_slash("show backend-dev");
            assert!(out.contains("Backend Dev"), "{out}");
            assert!(out.contains("- review"), "{out}");
            assert!(out.contains("instruction: go"), "{out}");

            let out = run_slash("show nope");
            assert!(out.contains("not found"), "{out}");

            let out = run_slash("bogus");
            assert!(out.contains("usage:"), "{out}");
        });

        let empty = tempfile::tempdir().unwrap();
        with_bundles_dir(empty.path(), || {
            let out = run_slash("");
            assert!(out.contains("no bundles installed"), "{out}");
        });
    }
}

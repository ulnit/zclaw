//! Config migration for retired models and deprecated settings
//! (hermes `hermes migrate` parity). Currently ships the xAI
//! May-15-2026 model-retirement migration: dry-run diagnosis by
//! default, `--apply` rewrites config.toml in place with an automatic
//! backup.

use std::collections::BTreeMap;
use std::path::Path;
use toml::Value;

pub const XAI_RETIREMENT_DATE: &str = "May 15, 2026";
pub const XAI_MIGRATION_GUIDE_URL: &str =
    "https://docs.x.ai/developers/migration/may-15-retirement";

/// One retired-model entry: replacement plus an optional
/// reasoning-effort note (non-reasoning variants migrating to a
/// reasoning-by-default model).
pub struct RetiredModel {
    pub replacement: &'static str,
    pub reasoning_effort: Option<&'static str>,
}

/// xAI models retired on May 15, 2026 (hermes `_RETIRED_MODELS`).
pub fn xai_retired_models() -> BTreeMap<&'static str, RetiredModel> {
    let mut map = BTreeMap::new();
    let entries: [(&str, &str, Option<&str>); 8] = [
        ("grok-4-0709", "grok-4.3", None),
        ("grok-4-fast-reasoning", "grok-4.3", None),
        ("grok-4-fast-non-reasoning", "grok-4.3", Some("none")),
        ("grok-4-1-fast-reasoning", "grok-4.3", None),
        ("grok-4-1-fast-non-reasoning", "grok-4.3", Some("none")),
        ("grok-code-fast-1", "grok-4.3", None),
        ("grok-3", "grok-4.3", None),
        ("grok-imagine-image-pro", "grok-imagine-image-quality", None),
    ];
    for (model, replacement, effort) in entries {
        map.insert(
            model,
            RetiredModel {
                replacement,
                reasoning_effort: effort,
            },
        );
    }
    map
}

/// Normalize a model id: strip known provider prefixes and lowercase
/// (hermes `_normalize`).
pub fn normalize_model(model: &str) -> String {
    let m = model.trim().to_lowercase();
    for prefix in ["xai/", "x-ai/", "xai:", "x-ai:"] {
        if let Some(rest) = m.strip_prefix(prefix) {
            return rest.to_string();
        }
    }
    m
}

/// Restore the original provider prefix onto a replacement model id.
pub fn with_original_prefix(original: &str, replacement: &str) -> String {
    let trimmed = original.trim();
    for prefix in ["xai/", "x-ai/", "xai:", "x-ai:"] {
        if trimmed.to_lowercase().starts_with(prefix) {
            return format!("{prefix}{replacement}");
        }
    }
    replacement.to_string()
}

/// A retired-model reference found in the config tree.
pub struct MigrationIssue {
    pub path: String,
    pub current: String,
    pub replacement: String,
    pub reasoning_effort: Option<&'static str>,
}

/// Recursively scan a TOML tree for retired xAI model references.
pub fn find_retired_xai_refs(value: &Value) -> Vec<MigrationIssue> {
    let retired = xai_retired_models();
    let mut issues = Vec::new();
    walk(value, String::new(), &retired, &mut issues);
    issues
}

fn walk(
    value: &Value,
    path: String,
    retired: &BTreeMap<&'static str, RetiredModel>,
    issues: &mut Vec<MigrationIssue>,
) {
    match value {
        Value::String(s) => {
            let norm = normalize_model(s);
            if let Some(entry) = retired.get(norm.as_str()) {
                issues.push(MigrationIssue {
                    path: if path.is_empty() {
                        "(root)".to_string()
                    } else {
                        path
                    },
                    current: s.clone(),
                    replacement: with_original_prefix(s, entry.replacement),
                    reasoning_effort: entry.reasoning_effort,
                });
            }
        }
        Value::Table(table) => {
            for (key, child) in table {
                let child_path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                walk(child, child_path, retired, issues);
            }
        }
        Value::Array(items) => {
            for (i, child) in items.iter().enumerate() {
                walk(child, format!("{path}[{i}]"), retired, issues);
            }
        }
        _ => {}
    }
}

/// Apply replacements in-place on the TOML tree. Returns the number of
/// values rewritten.
pub fn apply_xai_migration(value: &mut Value) -> usize {
    let retired = xai_retired_models();
    rewrite(value, &retired)
}

fn rewrite(value: &mut Value, retired: &BTreeMap<&'static str, RetiredModel>) -> usize {
    match value {
        Value::String(s) => {
            let norm = normalize_model(s);
            if let Some(entry) = retired.get(norm.as_str()) {
                *s = with_original_prefix(s, entry.replacement);
                1
            } else {
                0
            }
        }
        Value::Table(table) => {
            let mut count = 0;
            for (_, child) in table.iter_mut() {
                count += rewrite(child, retired);
            }
            count
        }
        Value::Array(items) => {
            let mut count = 0;
            for child in items.iter_mut() {
                count += rewrite(child, retired);
            }
            count
        }
        _ => 0,
    }
}

/// Run the xAI migration against a config file. Returns the diagnosed
/// issues and, when `apply` is set, the number of rewritten values.
/// With `backup`, the original file is copied to `<name>.toml.bak`
/// before rewriting.
pub fn run_xai_migration(
    path: &Path,
    apply: bool,
    backup: bool,
) -> Result<(Vec<MigrationIssue>, usize), String> {
    let value = crate::config_cmd::load_toml(path)?;
    let issues = find_retired_xai_refs(&value);
    if !apply || issues.is_empty() {
        return Ok((issues, 0));
    }
    if backup {
        let backup_path = path.with_extension("toml.bak");
        std::fs::copy(path, &backup_path)
            .map_err(|e| format!("backup {}: {e}", backup_path.display()))?;
    }
    let mut value = value;
    let count = apply_xai_migration(&mut value);
    crate::config_cmd::save_toml(path, &value)?;
    Ok((issues, count))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_provider_prefix() {
        assert_eq!(normalize_model("xai/grok-3"), "grok-3");
        assert_eq!(normalize_model("X-AI/Grok-4-0709"), "grok-4-0709");
        assert_eq!(normalize_model(" grok-code-fast-1 "), "grok-code-fast-1");
        assert_eq!(normalize_model("gpt-5"), "gpt-5");
    }

    #[test]
    fn prefix_preserved_on_replacement() {
        assert_eq!(
            with_original_prefix("xai/grok-3", "grok-4.3"),
            "xai/grok-4.3"
        );
        assert_eq!(with_original_prefix("grok-3", "grok-4.3"), "grok-4.3");
    }

    #[test]
    fn finds_and_rewrites_retired_refs() {
        let doc: Value = toml::from_str(
            r#"
            [model]
            model = "xai/grok-4-fast-non-reasoning"
            [profiles.fast]
            model = "grok-code-fast-1"
            [fallback]
            chain = ["grok-3", "claude-sonnet-4"]
            "#,
        )
        .unwrap();
        let issues = find_retired_xai_refs(&doc);
        assert_eq!(issues.len(), 3);
        let paths: Vec<&str> = issues.iter().map(|i| i.path.as_str()).collect();
        assert!(paths.contains(&"model.model"));
        assert!(paths.contains(&"profiles.fast.model"));
        assert!(paths.contains(&"fallback.chain[0]"));
        let non_reasoning = issues.iter().find(|i| i.path == "model.model").unwrap();
        assert_eq!(non_reasoning.reasoning_effort, Some("none"));
        assert_eq!(non_reasoning.replacement, "xai/grok-4.3");

        let mut doc = doc;
        assert_eq!(apply_xai_migration(&mut doc), 3);
        let remaining = find_retired_xai_refs(&doc);
        assert!(remaining.is_empty());
    }

    #[test]
    fn untouched_models_pass_through() {
        let doc: Value = toml::from_str(r#"model = "grok-4.3""#).unwrap();
        assert!(find_retired_xai_refs(&doc).is_empty());
    }
}

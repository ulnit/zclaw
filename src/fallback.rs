//! Fallback provider chain management — port of `hermes_cli/fallback_cmd.py`
//! (+ `fallback_config.get_fallback_chain`).
//!
//! Storage: `[model] fallbacks` in `config.toml` (a list of
//! `"provider:model"` specs — ulnclaw's equivalent of hermes'
//! `fallback_providers` list). The CLI mirrors hermes:
//! `list` (default), `add`, `remove`, `clear`; the interactive picker is
//! replaced by an explicit `provider:model` argument.

use std::path::Path;

use crate::agent::parse_fallback_spec;
use crate::config::UlncLawConfig;

/// One-line rendering of a fallback spec (hermes `_format_entry`).
pub fn format_entry(spec: &str) -> String {
    match parse_fallback_spec(spec) {
        Some((provider, model)) => format!("{model}  (via {provider})"),
        None => spec.to_string(),
    }
}

fn config_path(home: &Path) -> std::path::PathBuf {
    home.join("config.toml")
}

/// Read the current chain from `[model] fallbacks` (hermes
/// `get_fallback_chain` — ulnclaw has no legacy single-dict format).
pub fn read_chain(home: &Path) -> Vec<String> {
    match UlncLawConfig::load(Some(&config_path(home))) {
        Ok(config) => config.model.fallbacks,
        Err(_) => Vec::new(),
    }
}

/// Persist the chain to `[model] fallbacks` in config.toml.
///
/// Line-level edit: replaces the existing `fallbacks = [...]` line inside
/// the `[model]` section (or inserts one after the section header), keeping
/// the rest of the file — comments and ordering — intact.
pub fn save_chain(home: &Path, chain: &[String]) -> Result<(), String> {
    let path = config_path(home);
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let array_literal = format!(
        "fallbacks = [{}]",
        chain
            .iter()
            .map(|spec| format!("{spec:?}"))
            .collect::<Vec<_>>()
            .join(", ")
    );

    let lines: Vec<&str> = text.lines().collect();
    let mut model_start: Option<usize> = None;
    let mut model_end: usize = lines.len();
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed == "[model]" || trimmed.starts_with("[model.") {
            if trimmed == "[model]" {
                model_start = Some(i);
            }
        } else if trimmed.starts_with('[') && trimmed.ends_with(']') && model_start.is_some() {
            model_end = i;
            break;
        }
    }

    let mut new_lines: Vec<String> = Vec::new();
    match model_start {
        Some(start) => {
            let mut replaced = false;
            for (i, line) in lines.iter().enumerate() {
                let trimmed = line.trim();
                if i > start
                    && i < model_end
                    && (trimmed.starts_with("fallbacks") && trimmed.contains('='))
                {
                    new_lines.push(array_literal.clone());
                    replaced = true;
                } else {
                    new_lines.push((*line).to_string());
                }
            }
            if !replaced {
                new_lines.insert(start + 1, array_literal);
            }
        }
        None => {
            if !text.is_empty() && !text.ends_with('\n') {
                new_lines = lines.iter().map(|l| l.to_string()).collect();
                new_lines.push(String::new());
            } else {
                new_lines = lines.iter().map(|l| l.to_string()).collect();
            }
            new_lines.push("[model]".to_string());
            new_lines.push(array_literal);
        }
    }

    let mut out = new_lines.join("\n");
    if !text.is_empty() || !out.is_empty() {
        out.push('\n');
    }
    std::fs::write(&path, out).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(())
}

/// One-line description of the primary model (hermes `_describe_primary`).
fn describe_primary(config: &UlncLawConfig) -> String {
    format!("{}  (via {})", config.model.model, config.model.provider)
}

/// `ulnclaw fallback [list]` (hermes `cmd_fallback_list`).
pub fn list_fallbacks(home: &Path) -> String {
    let chain = read_chain(home);
    let mut out = String::new();
    out.push('\n');
    if chain.is_empty() {
        out.push_str("  No fallback providers configured.\n\n");
        out.push_str("  Add one with:  ulnclaw fallback add <provider:model>\n");
        out.push_str("  Example:       ulnclaw fallback add openrouter:openai/gpt-4o\n\n");
        return out;
    }
    if let Ok(config) = UlncLawConfig::load(Some(&config_path(home))) {
        out.push_str(&format!("  Primary:   {}\n\n", describe_primary(&config)));
    }
    let word = if chain.len() == 1 { "entry" } else { "entries" };
    out.push_str(&format!("  Fallback chain ({} {}):\n", chain.len(), word));
    for (i, spec) in chain.iter().enumerate() {
        out.push_str(&format!("    {}. {}\n", i + 1, format_entry(spec)));
    }
    out.push('\n');
    out.push_str(
        "  Tried in order when the primary fails (rate-limit, 5xx, connection errors).\n\n",
    );
    out
}

/// Same-deployment check (hermes `same_deployment`, simplified: ulnclaw
/// specs carry provider+model only).
fn same_deployment(a: &str, b: &str) -> bool {
    match (parse_fallback_spec(a), parse_fallback_spec(b)) {
        (Some((pa, ma)), Some((pb, mb))) => pa.eq_ignore_ascii_case(&pb) && ma == mb,
        _ => a.trim() == b.trim(),
    }
}

/// `ulnclaw fallback add <provider:model>` (hermes `cmd_fallback_add`;
/// rejects primaries-falling-back-to-themselves and exact duplicates).
pub fn add_fallback(home: &Path, spec: &str) -> Result<String, String> {
    let spec = spec.trim();
    if parse_fallback_spec(spec).is_none() {
        return Err(format!(
            "Invalid fallback spec: '{spec}'. Use <provider:model>, e.g. openrouter:openai/gpt-4o."
        ));
    }
    let mut out = String::new();
    out.push('\n');

    if let Ok(config) = UlncLawConfig::load(Some(&config_path(home))) {
        let primary_spec = format!("{}:{}", config.model.provider, config.model.model);
        if same_deployment(&primary_spec, spec) {
            out.push_str(&format!(
                "  Selected model matches the current primary ({}).\n",
                format_entry(spec)
            ));
            out.push_str("  A provider cannot be a fallback for itself — no change.\n");
            return Ok(out);
        }
    }

    let mut chain = read_chain(home);
    if chain.iter().any(|existing| same_deployment(existing, spec)) {
        out.push_str(&format!(
            "  {} is already in the fallback chain — skipped.\n",
            format_entry(spec)
        ));
        return Ok(out);
    }

    chain.push(spec.to_string());
    save_chain(home, &chain)?;

    out.push_str(&format!("  Added fallback: {}\n", format_entry(spec)));
    let word = if chain.len() == 1 { "entry" } else { "entries" };
    out.push_str(&format!(
        "  Chain is now {} {} long.\n\n",
        chain.len(),
        word
    ));
    out.push_str(
        "  Run 'ulnclaw fallback list' to view, or 'ulnclaw fallback remove' to delete.\n",
    );
    Ok(out)
}

/// `ulnclaw fallback remove <N|provider:model>` (hermes
/// `cmd_fallback_remove`, with the picker replaced by an explicit index or
/// exact spec).
pub fn remove_fallback(home: &Path, selector: &str) -> Result<String, String> {
    let mut chain = read_chain(home);
    if chain.is_empty() {
        return Ok("\n  No fallback providers configured — nothing to remove.\n".to_string());
    }
    let selector = selector.trim();
    let index = if let Ok(n) = selector.parse::<usize>() {
        if n == 0 || n > chain.len() {
            return Err(format!(
                "Invalid index {n}. The chain has {} entr{} — use 1-{}.",
                chain.len(),
                if chain.len() == 1 { "y" } else { "ies" },
                chain.len()
            ));
        }
        n - 1
    } else {
        match chain.iter().position(|spec| same_deployment(spec, selector)) {
            Some(i) => i,
            None => {
                return Err(format!(
                    "No fallback matching '{selector}'. Use an index (1-{}) or an exact provider:model spec.",
                    chain.len()
                ))
            }
        }
    };

    let removed = chain.remove(index);
    save_chain(home, &chain)?;

    let mut out = String::new();
    out.push('\n');
    out.push_str(&format!("  Removed fallback: {}\n", format_entry(&removed)));
    if chain.is_empty() {
        out.push_str("  Fallback chain is now empty.\n");
    } else {
        let word = if chain.len() == 1 { "entry" } else { "entries" };
        out.push_str(&format!("  Chain is now {} {} long.\n", chain.len(), word));
    }
    Ok(out)
}

/// `ulnclaw fallback clear` (hermes `cmd_fallback_clear`).
pub fn clear_fallbacks(home: &Path) -> Result<String, String> {
    let chain = read_chain(home);
    if chain.is_empty() {
        return Ok("\n  No fallback providers configured — nothing to clear.\n".to_string());
    }
    save_chain(home, &[])?;
    Ok("\n  Fallback chain cleared.\n".to_string())
}

/// Shared dispatch for the CLI (mirrors hermes `cmd_fallback`).
pub fn handle_fallback_command(
    home: &Path,
    args: &[String],
    assume_yes: bool,
) -> Result<String, String> {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("list");
    match sub {
        "" | "list" | "ls" => Ok(list_fallbacks(home)),
        "add" => {
            let Some(spec) = args.get(1) else {
                return Err("usage: ulnclaw fallback add <provider:model>".into());
            };
            add_fallback(home, spec)
        }
        "remove" | "rm" => {
            let Some(selector) = args.get(1) else {
                return Err("usage: ulnclaw fallback remove <N|provider:model>".into());
            };
            remove_fallback(home, selector)
        }
        "clear" => {
            if !assume_yes && std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                print!("  Clear all fallback entries? [y/N]: ");
                use std::io::Write;
                let _ = std::io::stdout().flush();
                let mut line = String::new();
                std::io::stdin()
                    .read_line(&mut line)
                    .map_err(|e| e.to_string())?;
                if !matches!(line.trim().to_lowercase().as_str(), "y" | "yes") {
                    return Ok("\n  Cancelled — no change.\n".to_string());
                }
            }
            clear_fallbacks(home)
        }
        other => Err(format!(
            "Unknown fallback subcommand: {other}\nUse one of: list, add, remove, clear"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_config(home: &Path, fallbacks_line: Option<&str>) {
        let mut text = String::from("[model]\nprovider = \"ollama\"\nmodel = \"qwen2.5:14b\"\n");
        if let Some(line) = fallbacks_line {
            text.push_str(line);
            text.push('\n');
        }
        text.push_str("\n[gateway]\nport = 8642\n");
        std::fs::write(home.join("config.toml"), text).unwrap();
    }

    #[test]
    fn format_entry_renders_provider_and_model() {
        assert_eq!(
            format_entry("openrouter:openai/gpt-4o"),
            "openai/gpt-4o  (via openrouter)"
        );
        assert_eq!(
            format_entry("ollama:qwen3:1.7b"),
            "qwen3:1.7b  (via ollama)"
        );
        assert_eq!(format_entry("not-a-spec"), "not-a-spec");
    }

    #[test]
    fn save_chain_preserves_other_sections() {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), None);
        save_chain(dir.path(), &["openrouter:openai/gpt-4o".to_string()]).unwrap();
        let text = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        assert!(text.contains("fallbacks = [\"openrouter:openai/gpt-4o\"]"));
        assert!(text.contains("[gateway]"));
        assert!(text.contains("port = 8642"));

        // In-place replace keeps the file otherwise intact.
        save_chain(dir.path(), &["a:b".to_string(), "c:d".to_string()]).unwrap();
        let text = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        assert!(text.contains("fallbacks = [\"a:b\", \"c:d\"]"));
        assert_eq!(text.matches("fallbacks").count(), 1);

        // Clear empties the array.
        save_chain(dir.path(), &[]).unwrap();
        let text = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        assert!(text.contains("fallbacks = []"));

        // No config file at all → created.
        let empty_dir = tempfile::tempdir().unwrap();
        save_chain(empty_dir.path(), &["x:y".to_string()]).unwrap();
        let text = std::fs::read_to_string(empty_dir.path().join("config.toml")).unwrap();
        assert!(text.contains("[model]"));
        assert!(text.contains("fallbacks = [\"x:y\"]"));
    }

    #[test]
    fn add_rejects_primary_and_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), None);

        // Same as primary → rejected.
        let out = add_fallback(dir.path(), "ollama:qwen2.5:14b").unwrap();
        assert!(out.contains("cannot be a fallback for itself"));
        assert!(read_chain(dir.path()).is_empty());

        // Valid add.
        let out = add_fallback(dir.path(), "openrouter:openai/gpt-4o").unwrap();
        assert!(out.contains("Added fallback"));
        assert_eq!(
            read_chain(dir.path()),
            vec!["openrouter:openai/gpt-4o".to_string()]
        );

        // Duplicate → skipped (case-insensitive provider match).
        let out = add_fallback(dir.path(), "OpenRouter:openai/gpt-4o").unwrap();
        assert!(out.contains("already in the fallback chain"));
        assert_eq!(read_chain(dir.path()).len(), 1);

        // Malformed spec → error.
        assert!(add_fallback(dir.path(), "no-colon-here").is_err());
    }

    #[test]
    fn remove_by_index_and_spec() {
        let dir = tempfile::tempdir().unwrap();
        write_config(
            dir.path(),
            Some("fallbacks = [\"a:m1\", \"b:m2\", \"c:m3\"]"),
        );

        let out = remove_fallback(dir.path(), "2").unwrap();
        assert!(out.contains("m2"));
        assert_eq!(
            read_chain(dir.path()),
            vec!["a:m1".to_string(), "c:m3".to_string()]
        );

        let out = remove_fallback(dir.path(), "c:m3").unwrap();
        assert!(out.contains("m3"));
        assert_eq!(read_chain(dir.path()), vec!["a:m1".to_string()]);

        assert!(remove_fallback(dir.path(), "9").is_err());
        assert!(remove_fallback(dir.path(), "zzz:nope").is_err());

        let out = remove_fallback(dir.path(), "1").unwrap();
        assert!(out.contains("empty"));
        let out = remove_fallback(dir.path(), "1").unwrap();
        assert!(out.contains("nothing to remove"));
    }

    #[test]
    fn clear_and_list_rendering() {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), Some("fallbacks = [\"a:m1\", \"b:m2\"]"));

        let listing = list_fallbacks(dir.path());
        assert!(listing.contains("Primary:"));
        assert!(listing.contains("Fallback chain (2 entries)"));
        assert!(listing.contains("1. m1  (via a)"));

        let out = clear_fallbacks(dir.path()).unwrap();
        assert!(out.contains("cleared"));
        assert!(read_chain(dir.path()).is_empty());

        let out = clear_fallbacks(dir.path()).unwrap();
        assert!(out.contains("nothing to clear"));

        let listing = list_fallbacks(dir.path());
        assert!(listing.contains("No fallback providers configured"));
    }

    #[test]
    fn dispatch_routes_subcommands() {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), None);
        let args: Vec<String> = vec!["add".into(), "p:m".into()];
        let out = handle_fallback_command(dir.path(), &args, true).unwrap();
        assert!(out.contains("Added fallback"));
        let out = handle_fallback_command(dir.path(), &[], true).unwrap();
        assert!(out.contains("Fallback chain (1 entry)"));
        assert!(handle_fallback_command(dir.path(), &["bogus".into()], true).is_err());
    }
}

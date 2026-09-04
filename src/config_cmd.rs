//! Configuration management CLI — port of `hermes config` (show / edit /
//! get / set / unset / path / env-path) from `hermes_cli/config.py`,
//! adapted to TOML storage.
//!
//! Keys use dotted paths into `config.toml` (`model.provider`,
//! `gateway.port`); ALL_CAPS keys resolve against the environment / `.env`
//! file (`OPENROUTER_API_KEY`), mirroring hermes' unified key handling.

use std::path::{Path, PathBuf};

use toml::Value;

/// Top-level sections the running version recognizes (for the unknown-key
/// notice on `set`; hermes prints the same advisory, values save either way).
pub const KNOWN_SECTIONS: &[&str] = &[
    "model",
    "timezone",
    "agent",
    "delegation",
    "terminal",
    "checkpoints",
    "memory",
    "web",
    "enabled_toolsets",
    "disabled_toolsets",
    "profiles",
    "mcp",
    "gateway",
    "approvals",
    "auxiliary",
    "display",
    "updates",
    "logging",
];

pub fn config_path() -> PathBuf {
    crate::config::ulnclaw_home().join("config.toml")
}

pub fn env_path() -> PathBuf {
    crate::config::ulnclaw_home().join(".env")
}

pub(crate) fn load_toml(path: &Path) -> Result<Value, String> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    if text.trim().is_empty() {
        return Ok(Value::Table(toml::map::Map::new()));
    }
    text.parse::<Value>()
        .map_err(|e| format!("✗ Cannot parse {}: {e}", path.display()))
}

pub(crate) fn save_toml(path: &Path, value: &Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let rendered = toml::to_string_pretty(value).map_err(|e| e.to_string())?;
    std::fs::write(path, rendered).map_err(|e| format!("✗ Cannot write {}: {e}", path.display()))
}

/// Hermes `_is_env_config_key`: ALL_CAPS (with underscores/digits) names are
/// environment / .env keys, not config sections.
pub fn is_env_config_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && key
            .chars()
            .next()
            .map(|c| c.is_ascii_uppercase())
            .unwrap_or(false)
}

pub fn get_nested<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    let mut current = value;
    for part in key.split('.') {
        current = current.get(part)?;
    }
    Some(current)
}

pub fn set_nested(value: &mut Value, key: &str, new_value: Value) -> Result<(), String> {
    let parts: Vec<&str> = key.split('.').collect();
    let mut current = value;
    for (i, part) in parts.iter().enumerate() {
        let is_last = i == parts.len() - 1;
        let table = current
            .as_table_mut()
            .ok_or_else(|| format!("✗ Cannot set '{key}': intermediate value is not a table"))?;
        if is_last {
            table.insert(part.to_string(), new_value);
            return Ok(());
        }
        current = table
            .entry(part.to_string())
            .or_insert_with(|| Value::Table(toml::map::Map::new()));
    }
    Ok(())
}

pub fn unset_nested(value: &mut Value, key: &str) -> bool {
    let parts: Vec<&str> = key.split('.').collect();
    fn walk(value: &mut Value, parts: &[&str]) -> bool {
        if parts.is_empty() {
            return false;
        }
        let Some(table) = value.as_table_mut() else {
            return false;
        };
        if parts.len() == 1 {
            return table.remove(parts[0]).is_some();
        }
        let Some(child) = table.get_mut(parts[0]) else {
            return false;
        };
        walk(child, &parts[1..])
    }
    walk(value, &parts)
}

/// Parse a CLI scalar into the most specific TOML type (hermes coerces
/// booleans/numbers the same way).
pub fn parse_scalar(raw: &str) -> Value {
    let trimmed = raw.trim();
    if trimmed == "true" || trimmed == "false" {
        return Value::Boolean(trimmed == "true");
    }
    if let Ok(int) = trimmed.parse::<i64>() {
        return Value::Integer(int);
    }
    if let Ok(float) = trimmed.parse::<f64>() {
        if trimmed.contains('.') || trimmed.contains('e') || trimmed.contains('E') {
            return Value::Float(float);
        }
    }
    // Inline TOML arrays/tables: try parsing as a value ("[a, b]", "{k=v}").
    if (trimmed.starts_with('[') && trimmed.ends_with(']'))
        || (trimmed.starts_with('{') && trimmed.ends_with('}'))
    {
        if let Ok(parsed) = format!("v = {trimmed}").parse::<toml::Table>() {
            if let Some(value) = parsed.get("v") {
                return value.clone();
            }
        }
    }
    Value::String(raw.to_string())
}

/// Convert a TOML value into a JSON value (manual walk — the toml crate's
/// Serialize impl cannot target serde_json for all variants).
fn toml_to_json(value: &Value) -> serde_json::Value {
    match value {
        Value::String(text) => serde_json::Value::String(text.clone()),
        Value::Integer(int) => serde_json::Value::Number((*int).into()),
        Value::Float(float) => serde_json::Number::from_f64(*float)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Value::Boolean(flag) => serde_json::Value::Bool(*flag),
        Value::Datetime(datetime) => serde_json::Value::String(datetime.to_string()),
        Value::Array(items) => serde_json::Value::Array(items.iter().map(toml_to_json).collect()),
        Value::Table(table) => serde_json::Value::Object(
            table
                .iter()
                .map(|(key, child)| (key.clone(), toml_to_json(child)))
                .collect(),
        ),
    }
}

fn redact_leaf(key: &str, value: &str) -> Option<String> {
    let lower = key.to_lowercase();
    if lower.contains("key") || lower.contains("token") || lower.contains("secret") {
        Some(crate::status::redact_key(value))
    } else {
        None
    }
}

fn redact_tree(value: &mut Value) {
    match value {
        Value::Table(table) => {
            for (key, child) in table.iter_mut() {
                if let Value::String(text) = child {
                    if let Some(masked) = redact_leaf(key, text) {
                        *text = masked;
                        continue;
                    }
                }
                redact_tree(child);
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_tree(item);
            }
        }
        _ => {}
    }
}

/// `ulnclaw config show` (hermes `show_config`, redaction-aware).
pub fn show_config() -> Result<String, String> {
    let path = config_path();
    let mut value = load_toml(&path)?;
    redact_tree(&mut value);

    let mut out = String::new();
    out.push('\n');
    out.push_str("┌─────────────────────────────────────────────────────────┐\n");
    out.push_str("│              ⚕ ulnclaw Configuration                    │\n");
    out.push_str("└─────────────────────────────────────────────────────────┘\n");
    out.push('\n');
    out.push_str("◆ Paths\n");
    out.push_str(&format!("  Config:       {}\n", path.display()));
    out.push_str(&format!("  Secrets:      {}\n", env_path().display()));
    out.push_str(&format!(
        "  Home:         {}\n",
        crate::config::ulnclaw_home().display()
    ));
    out.push('\n');
    out.push_str("◆ Configuration (secrets redacted)\n");
    let rendered = if value.as_table().map(|t| t.is_empty()).unwrap_or(true) {
        "  (empty — run 'ulnclaw init' to write defaults)".to_string()
    } else {
        toml::to_string_pretty(&value)
            .map_err(|e| e.to_string())?
            .lines()
            .map(|line| format!("  {line}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    out.push_str(&rendered);
    out.push('\n');
    Ok(out)
}

/// `ulnclaw config get <key> [--json]` (hermes `get_config_value`).
pub fn get_config_value(key: &str, as_json: bool) -> Result<String, String> {
    if is_env_config_key(key) {
        match crate::config::get_env_value(&key.to_uppercase()) {
            Some(value) => return Ok(value),
            None => return Err(format!("Config key not set: {key}")),
        }
    }
    let value = load_toml(&config_path())?;
    let Some(found) = get_nested(&value, key) else {
        return Err(format!("Config key not set: {key}"));
    };
    if as_json {
        let json = serde_json::to_string_pretty(&toml_to_json(found)).map_err(|e| e.to_string())?;
        return Ok(json);
    }
    Ok(match found {
        Value::String(text) => text.clone(),
        Value::Integer(int) => int.to_string(),
        Value::Float(float) => float.to_string(),
        Value::Boolean(flag) => flag.to_string(),
        Value::Datetime(datetime) => datetime.to_string(),
        other => {
            // Tables/arrays: the document serializer needs a table root, so
            // wrap bare arrays in a throwaway key and unwrap the rendering.
            let mut rendered = if other.is_array() {
                let mut wrapper = toml::map::Map::new();
                wrapper.insert("v".to_string(), other.clone());
                let text =
                    toml::to_string_pretty(&Value::Table(wrapper)).map_err(|e| e.to_string())?;
                text.trim_start_matches("v = ").to_string()
            } else {
                toml::to_string_pretty(other).map_err(|e| e.to_string())?
            };
            while rendered.ends_with('\n') {
                rendered.pop();
            }
            rendered
        }
    })
}

/// `ulnclaw config set <key> <value> [--force]` (hermes `set_config_value`).
/// Env-style keys are written to `.env`; everything else to `config.toml`.
pub fn set_config_value(key: &str, raw_value: &str, force: bool) -> Result<String, String> {
    if is_env_config_key(key) {
        let name = key.to_uppercase();
        set_env_value(&name, raw_value)?;
        return Ok(format!("✓ Set {name} in {}", env_path().display()));
    }

    let path = config_path();
    let mut value = load_toml(&path)?;
    let parsed = parse_scalar(raw_value);
    set_nested(&mut value, key, parsed)?;
    save_toml(&path, &value)?;

    let mut out = format!("✓ Set {key} = {raw_value} in {}\n", path.display());
    let top = key.split('.').next().unwrap_or(key);
    if !force && !KNOWN_SECTIONS.contains(&top) {
        out.push_str(&format!(
            "  Note: '{top}' is not a section this ulnclaw version recognizes.\n  \
             The value is saved; newer versions may use it. (--force silences this notice)\n"
        ));
    }
    Ok(out)
}

/// `ulnclaw config unset <key>` (hermes `unset_config_value`).
pub fn unset_config_value(key: &str) -> Result<String, String> {
    if is_env_config_key(key) {
        let name = key.to_uppercase();
        if !remove_env_value(&name)? {
            return Err(format!("Config key not set: {key}"));
        }
        return Ok(format!("✓ Unset {name} from {}", env_path().display()));
    }
    let path = config_path();
    let mut value = load_toml(&path)?;
    if !unset_nested(&mut value, key) {
        return Err(format!("Config key not set: {key}"));
    }
    save_toml(&path, &value)?;
    Ok(format!("✓ Unset {key} from {}", path.display()))
}

pub fn set_env_value(name: &str, value: &str) -> Result<(), String> {
    let path = env_path();
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut lines: Vec<String> = Vec::new();
    let mut replaced = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if !trimmed.is_empty() && !trimmed.starts_with('#') {
            if let Some(existing_name) = trimmed.split_once('=') {
                if existing_name.0.trim() == name {
                    lines.push(format!("{name}={value}"));
                    replaced = true;
                    continue;
                }
            }
        }
        lines.push(line.to_string());
    }
    if !replaced {
        if !lines.is_empty() && !text.ends_with('\n') {
            lines.push(String::new());
        }
        lines.push(format!("{name}={value}"));
    }
    std::fs::write(&path, lines.join("\n") + "\n")
        .map_err(|e| format!("✗ Cannot write {}: {e}", path.display()))
}

pub(crate) fn remove_env_value(name: &str) -> Result<bool, String> {
    let path = env_path();
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut removed = false;
    let mut lines: Vec<String> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        let is_target = !trimmed.is_empty()
            && !trimmed.starts_with('#')
            && trimmed
                .split_once('=')
                .map(|(lhs, _)| lhs.trim() == name)
                .unwrap_or(false);
        if is_target {
            removed = true;
            continue;
        }
        lines.push(line.to_string());
    }
    if removed {
        std::fs::write(&path, lines.join("\n") + "\n")
            .map_err(|e| format!("✗ Cannot write {}: {e}", path.display()))?;
    }
    Ok(removed)
}

/// Dispatch for `ulnclaw config <action>` (hermes `config_command`).
pub fn handle_config_command(
    args: &[String],
    as_json: bool,
    force: bool,
) -> Result<String, String> {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("show");
    match sub {
        "show" => show_config(),
        "path" => Ok(config_path().display().to_string()),
        "env-path" => Ok(env_path().display().to_string()),
        "edit" => {
            let path = config_path();
            if !path.exists() {
                crate::config::UlncLawConfig::write_default_if_missing().map_err(|e| e.to_string())?;
            }
            let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
            let status = std::process::Command::new(&editor)
                .arg(&path)
                .status()
                .map_err(|e| format!("✗ Cannot launch editor '{editor}': {e}"))?;
            if status.success() {
                Ok(String::new())
            } else {
                Err(format!("editor exited with {status}"))
            }
        }
        "get" => {
            let Some(key) = args.get(1) else {
                return Ok(
                    "Usage: ulnclaw config get <key> [--json]\n\nExamples:\n  \
                     ulnclaw config get model\n  ulnclaw config get terminal.backend\n  \
                     ulnclaw config get OPENROUTER_API_KEY"
                        .to_string(),
                );
            };
            get_config_value(key, as_json)
        }
        "set" => {
            let (Some(key), Some(value)) = (args.get(1), args.get(2)) else {
                return Ok(
                    "Usage: ulnclaw config set [--force] <key> <value>\n\nExamples:\n  \
                     ulnclaw config set model.provider openrouter\n  \
                     ulnclaw config set terminal.backend docker\n  \
                     ulnclaw config set OPENROUTER_API_KEY sk-or-..."
                        .to_string(),
                );
            };
            set_config_value(key, value, force)
        }
        "unset" => {
            let Some(key) = args.get(1) else {
                return Ok(
                    "Usage: ulnclaw config unset <key>\n\nExamples:\n  \
                     ulnclaw config unset model.api_key\n  ulnclaw config unset OPENROUTER_API_KEY"
                        .to_string(),
                );
            };
            unset_config_value(key)
        }
        other => Err(format!(
            "Unknown config subcommand: {other}\nUse one of: show, get, set, unset, path, env-path, edit"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_home<F: FnOnce()>(dir: &Path, f: F) {
        let _guard = crate::models_dev::test_env_lock();
        let prev = std::env::var("ULNCLAW_HOME").ok();
        std::env::set_var("ULNCLAW_HOME", dir);
        f();
        match prev {
            Some(v) => std::env::set_var("ULNCLAW_HOME", v),
            None => std::env::remove_var("ULNCLAW_HOME"),
        }
    }

    #[test]
    fn env_key_detection_and_scalar_parsing() {
        assert!(is_env_config_key("OPENROUTER_API_KEY"));
        assert!(is_env_config_key("ULNCLAW_GATEWAY_KEY"));
        assert!(!is_env_config_key("model.provider"));
        assert!(!is_env_config_key("Model"));
        assert_eq!(parse_scalar("true"), Value::Boolean(true));
        assert_eq!(parse_scalar("42"), Value::Integer(42));
        assert_eq!(parse_scalar("1.5"), Value::Float(1.5));
        assert_eq!(parse_scalar("docker"), Value::String("docker".into()));
        let array = parse_scalar("[\"a\", \"b\"]");
        assert!(
            array.as_array().map(|a| a.len() == 2).unwrap_or(false),
            "{array:?}"
        );
    }

    #[test]
    fn nested_get_set_unset() {
        let mut doc: Value = toml::from_str("[model]\nprovider = \"ollama\"\n").unwrap();
        assert_eq!(
            get_nested(&doc, "model.provider").and_then(|v| v.as_str()),
            Some("ollama")
        );
        assert!(get_nested(&doc, "model.nope").is_none());

        set_nested(&mut doc, "model.model", Value::String("qwen".into())).unwrap();
        set_nested(&mut doc, "gateway.port", Value::Integer(9999)).unwrap();
        assert_eq!(
            get_nested(&doc, "gateway.port").and_then(|v| v.as_integer()),
            Some(9999)
        );

        assert!(unset_nested(&mut doc, "model.model"));
        assert!(get_nested(&doc, "model.model").is_none());
        assert!(!unset_nested(&mut doc, "model.model"));
        assert!(!unset_nested(&mut doc, "gateway.nope"));
    }

    #[test]
    fn get_set_unset_roundtrip_on_home() {
        let dir = tempfile::tempdir().unwrap();
        with_home(dir.path(), || {
            std::fs::write(
                config_path(),
                "[model]\nprovider = \"ollama\"\nmodel = \"qwen2.5:14b\"\n",
            )
            .unwrap();

            assert_eq!(get_config_value("model.provider", false).unwrap(), "ollama");
            let json = get_config_value("model", true).unwrap();
            assert!(json.contains("qwen2.5:14b"));
            assert!(get_config_value("model.missing", false).is_err());

            set_config_value("gateway.port", "9123", true).unwrap();
            assert_eq!(get_config_value("gateway.port", false).unwrap(), "9123");

            // Unknown section produces the advisory without --force.
            let out = set_config_value("custom.thing", "abc", false).unwrap();
            assert!(out.contains("not a section this ulnclaw version recognizes"));
            assert_eq!(get_config_value("custom.thing", false).unwrap(), "abc");

            unset_config_value("custom.thing").unwrap();
            assert!(get_config_value("custom.thing", false).is_err());
            assert!(unset_config_value("custom.thing").is_err());

            // Env-style keys land in .env.
            set_config_value("TEST_VENDOR_API_KEY", "sekrit", true).unwrap();
            let env_text = std::fs::read_to_string(env_path()).unwrap();
            assert!(env_text.contains("TEST_VENDOR_API_KEY=sekrit"));
            unset_config_value("TEST_VENDOR_API_KEY").unwrap();
            let env_text = std::fs::read_to_string(env_path()).unwrap();
            assert!(!env_text.contains("TEST_VENDOR_API_KEY"));
        });
    }

    #[test]
    fn show_redacts_secrets() {
        let dir = tempfile::tempdir().unwrap();
        with_home(dir.path(), || {
            std::fs::write(
                config_path(),
                "[model]\nprovider = \"openai\"\napi_key = \"sk-1234567890abcdef\"\n",
            )
            .unwrap();
            let out = show_config().unwrap();
            assert!(out.contains("ulnclaw Configuration"));
            assert!(
                !out.contains("sk-1234567890abcdef"),
                "secret leaked:\n{out}"
            );
            assert!(out.contains("sk-…cdef"));
            assert!(out.contains("Paths"));
        });
    }

    #[test]
    fn env_var_get_prefers_process_env() {
        let dir = tempfile::tempdir().unwrap();
        with_home(dir.path(), || {
            std::fs::write(env_path(), "ULNCLAW_TEST_CFG_KEY=from-file\n").unwrap();
            assert_eq!(
                get_config_value("ULNCLAW_TEST_CFG_KEY", false).unwrap(),
                "from-file"
            );
            std::env::set_var("ULNCLAW_TEST_CFG_KEY", "from-env");
            assert_eq!(
                get_config_value("ULNCLAW_TEST_CFG_KEY", false).unwrap(),
                "from-env"
            );
            std::env::remove_var("ULNCLAW_TEST_CFG_KEY");
        });
    }
}

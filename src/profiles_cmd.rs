//! Named-profile management over `[profiles.*]` in config.toml
//! (hermes profiles parity, lean port). Hermes models a profile as a
//! full per-profile home directory; ulnclaw profiles are config
//! overrides (model + toolsets) applied via `--profile`, kanban
//! assignment or `/p/<profile>` gateway multiplexing. These helpers
//! back both the `ulnclaw profiles` CLI and the gateway
//! `/api/profiles*` endpoints so the two surfaces stay consistent.

use toml::Value;

/// Profile-name rule: ASCII alphanumeric lead, then alnum/`_`/`-`,
/// max 64 chars (keeps TOML table keys + `/p/<name>` route segments
/// sane).
pub fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.chars().enumerate().all(|(index, ch)| {
            if index == 0 {
                ch.is_ascii_alphanumeric()
            } else {
                ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'
            }
        })
}

/// Load config.toml as a raw TOML document.
fn load_root() -> Result<Value, String> {
    crate::config_cmd::load_toml(&crate::config_cmd::config_path())
}

fn save_root(root: &Value) -> Result<(), String> {
    crate::config_cmd::save_toml(&crate::config_cmd::config_path(), root)
}

fn profiles_table_mut(root: &mut Value) -> Option<&mut toml::Table> {
    root.as_table_mut()
        .and_then(|table| table.get_mut("profiles"))
        .and_then(|profiles| profiles.as_table_mut())
}

/// One profile's fields for create/replace.
#[derive(Debug, Default)]
pub struct ProfileSpec {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub temperature: Option<f32>,
    /// `Some` = set (empty clears); `None` = leave the key alone.
    pub enabled_toolsets: Option<Vec<String>>,
    pub disabled_toolsets: Option<Vec<String>>,
}

/// Build the `[profiles.<name>]` TOML table for one spec.
pub fn build_profile_table(spec: &ProfileSpec) -> toml::Table {
    let mut profile_table = toml::Table::new();
    if let (Some(provider), Some(model)) = (spec.provider.as_deref(), spec.model.as_deref()) {
        let mut model_table = toml::Table::new();
        model_table.insert("provider".into(), Value::String(provider.to_string()));
        model_table.insert("model".into(), Value::String(model.to_string()));
        if let Some(base_url) = spec.base_url.as_deref().filter(|raw| !raw.is_empty()) {
            model_table.insert("base_url".into(), Value::String(base_url.to_string()));
        }
        if let Some(temperature) = spec.temperature {
            model_table.insert("temperature".into(), Value::Float(temperature as f64));
        }
        profile_table.insert("model".into(), Value::Table(model_table));
    }
    for (key, toolsets) in [
        ("enabled_toolsets", &spec.enabled_toolsets),
        ("disabled_toolsets", &spec.disabled_toolsets),
    ] {
        if let Some(toolsets) = toolsets {
            let values: Vec<Value> = toolsets
                .iter()
                .map(|entry| entry.trim())
                .filter(|entry| !entry.is_empty())
                .map(|entry| Value::String(entry.to_string()))
                .collect();
            profile_table.insert(key.into(), Value::Array(values));
        }
    }
    profile_table
}

/// Parsed profiles from config.toml (typed, sorted by name).
pub fn list_profiles() -> Result<Vec<(String, crate::config::ProfileOverride)>, String> {
    let path = crate::config_cmd::config_path();
    let config = crate::config::UlncLawConfig::load(Some(&path)).map_err(|e| e.to_string())?;
    let mut rows: Vec<(String, crate::config::ProfileOverride)> =
        config.profiles.into_iter().collect();
    rows.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(rows)
}

/// Create or replace `[profiles.<name>]`; returns whether it was new.
pub fn save_profile(name: &str, spec: &ProfileSpec) -> Result<bool, String> {
    if !is_valid_name(name) {
        return Err("profile name must start with a letter/digit and use only letters, digits, '-' or '_' (max 64)".into());
    }
    if spec.provider.is_some() != spec.model.is_some() {
        return Err("provider and model must be set together".into());
    }
    let mut root = load_root()?;
    let table = build_profile_table(spec);
    let created = {
        let Some(root_table) = root.as_table_mut() else {
            return Err("config root is not a table".into());
        };
        let profiles = root_table
            .entry("profiles")
            .or_insert_with(|| Value::Table(toml::map::Map::new()));
        let Some(profiles) = profiles.as_table_mut() else {
            return Err("[profiles] in config.toml is not a table".into());
        };
        profiles
            .insert(name.to_string(), Value::Table(table))
            .is_none()
    };
    save_root(&root)?;
    Ok(created)
}

/// Rename `[profiles.<old>]` to `<new>`.
pub fn rename_profile(old: &str, new: &str) -> Result<(), String> {
    if !is_valid_name(new) {
        return Err("profile name must start with a letter/digit and use only letters, digits, '-' or '_' (max 64)".into());
    }
    if old == new {
        return Err("new name is identical to the current name".into());
    }
    let mut root = load_root()?;
    let Some(profiles) = profiles_table_mut(&mut root) else {
        return Err(format!("no profile named '{old}'"));
    };
    let Some(value) = profiles.remove(old) else {
        return Err(format!("no profile named '{old}'"));
    };
    if profiles.contains_key(new) {
        profiles.insert(old.to_string(), value);
        return Err(format!("profile '{new}' already exists"));
    }
    profiles.insert(new.to_string(), value);
    save_root(&root)?;
    Ok(())
}

/// Delete `[profiles.<name>]`.
pub fn delete_profile(name: &str) -> Result<(), String> {
    let mut root = load_root()?;
    let removed = profiles_table_mut(&mut root)
        .and_then(|profiles| profiles.remove(name))
        .is_some();
    if !removed {
        return Err(format!("no profile named '{name}'"));
    }
    save_root(&root)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_validation() {
        assert!(is_valid_name("work"));
        assert!(is_valid_name("a-b_c9"));
        assert!(!is_valid_name(""));
        assert!(!is_valid_name("-lead"));
        assert!(!is_valid_name("_lead"));
        assert!(!is_valid_name("has space"));
        assert!(!is_valid_name(&"x".repeat(65)));
    }

    #[test]
    fn spec_requires_provider_with_model() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = crate::models_dev::test_env_lock();
        let prev = std::env::var("ULNCLAW_HOME").ok();
        std::env::set_var("ULNCLAW_HOME", tmp.path());
        let spec = ProfileSpec {
            provider: Some("openai".into()),
            ..Default::default()
        };
        assert!(save_profile("work", &spec).is_err());
        match prev {
            Some(value) => std::env::set_var("ULNCLAW_HOME", value),
            None => std::env::remove_var("ULNCLAW_HOME"),
        }
    }

    #[test]
    fn save_list_rename_delete_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = crate::models_dev::test_env_lock();
        let prev = std::env::var("ULNCLAW_HOME").ok();
        std::env::set_var("ULNCLAW_HOME", tmp.path());

        let spec = ProfileSpec {
            provider: Some("openai".into()),
            model: Some("gpt-test".into()),
            enabled_toolsets: Some(vec!["terminal".into(), "web".into()]),
            ..Default::default()
        };
        assert!(save_profile("work", &spec).unwrap());
        assert!(!save_profile("work", &spec).unwrap());

        let rows = list_profiles().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "work");
        assert_eq!(rows[0].1.model.as_ref().unwrap().model, "gpt-test");
        assert_eq!(
            rows[0].1.enabled_toolsets.as_ref().unwrap(),
            &vec!["terminal".to_string(), "web".to_string()]
        );

        rename_profile("work", "office").unwrap();
        assert!(rename_profile("nope", "x").is_err());
        let rows = list_profiles().unwrap();
        assert_eq!(rows[0].0, "office");

        delete_profile("office").unwrap();
        assert!(delete_profile("office").is_err());
        assert!(list_profiles().unwrap().is_empty());

        match prev {
            Some(value) => std::env::set_var("ULNCLAW_HOME", value),
            None => std::env::remove_var("ULNCLAW_HOME"),
        }
    }
}

//! `ulnclaw model` — interactive provider + model picker (lean port of
//! hermes `cmd_model` / `select_provider_and_model`).
//!
//! Hermes' flow: require a TTY, pick a provider, prompt credentials,
//! pick a model from the models.dev catalog (with manual fallback),
//! persist `model.provider` + `model.model`. The `--refresh` flag clears
//! the picker cache before selection. This port reuses the models.dev
//! catalog module and the setup-wizard prompt primitives.

use crate::config::UlncLawConfig;
use crate::config_cmd;
use crate::models_dev as md;
use crate::setup_cmd;

/// Cap the numbered model list so the picker stays usable on huge catalogs.
const MAX_MODEL_ROWS: usize = 40;

/// models.dev catalog id for a ulnclaw provider slug (testable wrapper
/// around `models_dev::provider_to_models_dev`).
/// OpenAI flagship prefixes eligible for Priority Processing (hermes
/// `_OPENAI_FAST_MODE_PREFIXES`).
const OPENAI_FAST_MODE_PREFIXES: &[&str] = &["gpt-", "o1", "o3", "o4"];

/// Whether the `/fast` toggle should be offered for this model (hermes
/// `model_supports_fast_mode`): OpenAI-flagship chat models only. The
/// Codex series is excluded (its Responses API path does not accept
/// `service_tier`), and vendor prefixes like `openai/gpt-5` are
/// stripped before matching.
pub fn model_supports_fast_mode(model_id: &str) -> bool {
    let raw = model_id.trim().to_ascii_lowercase();
    let base = raw.rsplit('/').next().unwrap_or(&raw);
    let base = base.split(':').next().unwrap_or(base);
    if base.is_empty() || base.contains("codex") {
        return false;
    }
    OPENAI_FAST_MODE_PREFIXES
        .iter()
        .any(|prefix| base.starts_with(prefix))
}

pub fn models_dev_id_for(provider: &str) -> Option<&'static str> {
    md::provider_to_models_dev(provider)
}

/// Agentic model ids for a provider from the models.dev catalog, capped
/// at [`MAX_MODEL_ROWS`]. Empty when the provider has no catalog entry or
/// the catalog is unavailable offline.
pub fn catalog_models(provider: &str, allow_network: bool) -> Vec<String> {
    let Some(catalog_id) = models_dev_id_for(provider) else {
        return Vec::new();
    };
    let mut models = md::list_agentic_models(catalog_id);
    models.truncate(MAX_MODEL_ROWS);
    let _ = allow_network;
    models
}

/// Provider picker entries `(id, label)`: the current provider first
/// (marked), the built-in roster, then user-defined `[providers.<slug>]`
/// entries.
pub fn build_provider_entries(config: &UlncLawConfig) -> Vec<(String, String)> {
    let mut entries: Vec<(String, String)> = Vec::new();
    let current = config.model.provider.clone();
    let mut push = |id: &str, label: &str| {
        if !entries.iter().any(|(e, _)| e == id) {
            let display = if id == current {
                format!("{label} (current)")
            } else {
                label.to_string()
            };
            entries.push((id.to_string(), display));
        }
    };
    for (id, label) in setup_cmd::provider_choices() {
        push(id, label);
    }
    let mut custom: Vec<&String> = config.providers.keys().collect();
    custom.sort();
    for slug in custom {
        push(slug, &format!("custom: {slug}"));
    }
    // Current provider wins position 0 even when it's a custom slug.
    if let Some(pos) = entries.iter().position(|(id, _)| *id == current) {
        if pos != 0 {
            let entry = entries.remove(pos);
            entries.insert(0, entry);
        }
    }
    entries
}

/// Persist the provider/model choice (base_url left untouched).
pub fn apply_model_choice(
    doc: &mut toml::Value,
    provider: &str,
    model: &str,
) -> Result<(), String> {
    config_cmd::set_nested(
        doc,
        "model.provider",
        toml::Value::String(provider.to_string()),
    )?;
    config_cmd::set_nested(doc, "model.model", toml::Value::String(model.to_string()))
}

/// Render the confirmation line shown after a successful switch.
pub fn render_switch_summary(
    provider: &str,
    model: &str,
    base_url: &str,
    key_state: &str,
) -> String {
    format!(
        "✓ Model switched\n  Provider: {provider}\n  Model:    {model}\n  Endpoint: {base_url}\n  API key:  {key_state}\n"
    )
}

/// Entry point for `ulnclaw model [--refresh]`.
pub fn run_model_picker(refresh: bool) -> Result<(), String> {
    if !setup_cmd::is_interactive_stdin() {
        return Err(
            "`ulnclaw model` needs an interactive TTY. Set config directly instead:\n  \
             ulnclaw config set model.provider <provider>\n  ulnclaw config set model.model <model>"
                .to_string(),
        );
    }

    if refresh {
        md::fetch_models_dev_opts(true, true);
        println!("  Cleared model picker cache.");
    }

    let config = UlncLawConfig::load(None).map_err(|e| e.to_string())?;
    let entries = build_provider_entries(&config);

    println!();
    println!("  ─── Model & Provider ───");
    let labels: Vec<&str> = entries.iter().map(|(_, l)| l.as_str()).collect();
    let pick = setup_cmd::prompt_choice("Which provider should ulnclaw use?", &labels, 0)?;
    let provider = entries[pick].0.clone();

    // Credentials: prompt into `.env` when the provider needs a key.
    let mut key_state = "not required".to_string();
    if let Some(var) = setup_cmd::api_key_env_var(&provider) {
        let existing = crate::config::get_env_value(var).unwrap_or_default();
        if existing.trim().is_empty() {
            let key = setup_cmd::prompt_hidden(&format!("{var}"))?;
            if key.trim().is_empty() {
                key_state = "MISSING — set before chatting".to_string();
            } else {
                config_cmd::set_env_value(var, key.trim())?;
                key_state = format!("saved to {}", config_cmd::env_path().display());
            }
        } else {
            key_state = "configured".to_string();
        }
    }

    // Model: catalog picker with manual fallback.
    let current_model = config.model.model.clone();
    let models = catalog_models(&provider, false);
    let model = if models.is_empty() {
        let default = if current_model.is_empty() {
            setup_cmd::default_model_for(&provider).to_string()
        } else {
            current_model.clone()
        };
        println!("  ℹ No models.dev catalog entry for '{provider}' — enter the model name.");
        setup_cmd::prompt_line("Model name", &default)?
    } else {
        let default_idx = models.iter().position(|m| *m == current_model).unwrap_or(0);
        let mut rows: Vec<String> = models.clone();
        rows.push("Type a model name manually…".to_string());
        let row_refs: Vec<&str> = rows.iter().map(String::as_str).collect();
        let pick = setup_cmd::prompt_choice("Which model?", &row_refs, default_idx)?;
        if pick == rows.len() - 1 {
            setup_cmd::prompt_line("Model name", &current_model)?
        } else {
            models[pick].clone()
        }
    };

    let config_path = config_cmd::config_path();
    let mut doc = config_cmd::load_toml(&config_path)?;
    apply_model_choice(&mut doc, &provider, &model)?;
    config_cmd::save_toml(&config_path, &doc)?;

    let updated = UlncLawConfig::load(None).map_err(|e| e.to_string())?;
    print!(
        "{}",
        render_switch_summary(&provider, &model, &updated.resolve_base_url(), &key_state)
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_dev_mapping_covers_builtin_providers() {
        assert_eq!(models_dev_id_for("openai"), Some("openai"));
        assert_eq!(models_dev_id_for("anthropic"), Some("anthropic"));
        assert_eq!(models_dev_id_for("openrouter"), Some("openrouter"));
        assert_eq!(models_dev_id_for("dashscope"), Some("alibaba"));
        assert_eq!(models_dev_id_for("ollama"), None);
    }

    #[test]
    fn provider_entries_current_first_and_custom_included() {
        let mut config = UlncLawConfig::default();
        config.model.provider = "anthropic".to_string();
        config.providers.insert(
            "mybox".to_string(),
            crate::config::CustomProviderConfig::default(),
        );
        let entries = build_provider_entries(&config);
        assert_eq!(entries[0].0, "anthropic");
        assert!(entries[0].1.contains("(current)"));
        assert!(entries.iter().any(|(id, _)| id == "mybox"));
        // Built-ins all present exactly once.
        let ids: Vec<&str> = entries.iter().map(|(id, _)| id.as_str()).collect();
        for builtin in [
            "openai",
            "anthropic",
            "openrouter",
            "dashscope",
            "ollama",
            "llamacpp",
            "custom",
        ] {
            assert_eq!(
                ids.iter().filter(|id| **id == builtin).count(),
                1,
                "{builtin}"
            );
        }
    }

    #[test]
    fn provider_entries_default_config_starts_openai() {
        let config = UlncLawConfig::default();
        let entries = build_provider_entries(&config);
        assert_eq!(entries[0].0, config.model.provider);
        assert!(entries[0].1.contains("(current)"));
    }

    #[test]
    fn apply_model_choice_writes_both_keys() {
        let mut doc: toml::Value = toml::from_str("").unwrap();
        apply_model_choice(&mut doc, "openrouter", "openrouter/auto").unwrap();
        assert_eq!(
            config_cmd::get_nested(&doc, "model.provider").and_then(|v| v.as_str()),
            Some("openrouter")
        );
        assert_eq!(
            config_cmd::get_nested(&doc, "model.model").and_then(|v| v.as_str()),
            Some("openrouter/auto")
        );
    }

    #[test]
    fn render_switch_summary_includes_state() {
        let out = render_switch_summary(
            "openai",
            "gpt-5.2",
            "https://api.openai.com/v1",
            "configured",
        );
        assert!(out.contains("openai"));
        assert!(out.contains("gpt-5.2"));
        assert!(out.contains("configured"));
    }

    #[test]
    fn catalog_models_truncates_and_handles_unknown() {
        // Unknown provider → empty regardless of catalog state.
        assert!(catalog_models("no-such-provider", false).is_empty());
        // Known provider → bounded list (may be empty offline; must not panic).
        let models = catalog_models("openai", false);
        assert!(models.len() <= MAX_MODEL_ROWS);
    }

    #[test]
    fn picker_refuses_non_tty() {
        // Only assert in non-interactive harnesses (CI, piped runs).
        if crate::setup_cmd::is_interactive_stdin() {
            return;
        }
        let err = run_model_picker(false).unwrap_err();
        assert!(err.contains("interactive TTY"), "{err}");
    }

    #[test]
    fn fast_mode_gate_matches_openai_flagships() {
        assert!(model_supports_fast_mode("gpt-5"));
        assert!(model_supports_fast_mode("gpt-4.1-mini"));
        assert!(model_supports_fast_mode("o3"));
        assert!(model_supports_fast_mode("o4-mini"));
        // Vendor prefixes and case are normalized.
        assert!(model_supports_fast_mode("openai/gpt-5"));
        assert!(model_supports_fast_mode("OpenAI/GPT-5"));
        // Codex series routes a different API path.
        assert!(!model_supports_fast_mode("codex-mini-latest"));
        assert!(!model_supports_fast_mode("gpt-5-codex"));
        // Non-OpenAI families are not eligible.
        assert!(!model_supports_fast_mode("claude-opus-4-6"));
        assert!(!model_supports_fast_mode("llama3"));
        assert!(!model_supports_fast_mode(""));
    }
}

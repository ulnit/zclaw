//! Auxiliary (per-task) provider routing — port of the task-resolution layer
//! of hermes' `agent/auxiliary_client.py` (`_resolve_task_provider_model` +
//! `get_text_auxiliary_client`).
//!
//! Auxiliary LLM calls (context compression, vision analysis, ...) can be
//! routed to a different provider/model than the main conversation via
//! `[auxiliary.<task>]` config sections. Unset fields and `"auto"` inherit
//! the main runtime, exactly like hermes.

use crate::config::{AuxiliaryTaskConfig, UlncLawConfig};
use crate::error::{AgentError, Result};
use crate::provider::anthropic::AnthropicProvider;
use crate::provider::openai::OpenAiProvider;
use crate::provider::Provider;
use std::sync::Arc;

/// Auxiliary task: context compression summary call.
pub const TASK_COMPRESSION: &str = "compression";
/// Auxiliary task: image analysis (vision).
pub const TASK_VISION: &str = "vision";
pub const TASK_APPROVAL: &str = "approval";
/// Auxiliary task: session title generation (hermes
/// `title_generator.generate_title` task name).
pub const TASK_TITLE_GENERATION: &str = "title_generation";

/// Canonical auxiliary task slots ulnclaw knows (lean subset of hermes'
/// `_AUX_TASK_SLOTS`; only tasks ulnclaw actually routes). Used by
/// `GET /api/model/auxiliary` and the auxiliary scope of `/api/model/set`.
pub const TASK_SLOTS: &[&str] = &[
    TASK_VISION,
    TASK_COMPRESSION,
    TASK_APPROVAL,
    TASK_TITLE_GENERATION,
];

/// Where the resolved auxiliary runtime came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuxSource {
    /// No task override — the main runtime is used as-is.
    Main,
    /// A `[auxiliary.<task>]` entry changed the provider and/or model.
    TaskConfig,
}

/// Resolved auxiliary runtime for one task.
#[derive(Clone)]
pub struct AuxTaskResolution {
    /// Provider instance to call (may be the shared main provider).
    pub provider: Arc<dyn Provider>,
    /// Model id to use for the task call.
    pub model: String,
    /// Whether the resolution came from task config or the main runtime.
    pub source: AuxSource,
}

/// Providers that run locally and need no API key (mirrors `build_provider`).
pub fn is_keyless(provider: &str) -> bool {
    matches!(provider, "ollama" | "llamacpp" | "llama_cpp" | "local")
}

/// Build a provider instance for an explicit (provider, model, endpoint,
/// key) tuple. Shared by auxiliary task resolution and MoA slot runtimes.
pub fn build_task_provider(
    provider_name: &str,
    model: &str,
    base_url: &str,
    api_key: Option<&str>,
    max_retries: usize,
) -> Result<Arc<dyn Provider>> {
    if provider_name == "anthropic" {
        let mut builder = AnthropicProvider::builder()
            .endpoint(base_url)
            .model(model)
            .name(provider_name)
            .max_retries(max_retries);
        if let Some(key) = api_key {
            builder = builder.api_key(key);
        }
        return Ok(Arc::new(builder.build()?));
    }
    let mut builder = OpenAiProvider::builder()
        .endpoint(base_url)
        .model(model)
        .name(provider_name)
        .max_retries(max_retries);
    if let Some(key) = api_key {
        builder = builder.api_key(key);
    }
    Ok(Arc::new(builder.build()?))
}

/// Resolve the (provider, model) runtime for an auxiliary `task`.
///
/// Priority (hermes `_resolve_task_provider_model`):
///   1. `auxiliary.<task>.{provider, model, base_url, api_key, key_env}`
///   2. `"auto"` / blank → inherit the main runtime
///
/// The shared `main` provider instance is reused whenever the task does not
/// override anything; a model-only override builds a fresh instance of the
/// main provider (ulnclaw providers bake their model in).
pub fn resolve_aux_task(
    config: &UlncLawConfig,
    task: &str,
    main: Arc<dyn Provider>,
) -> Result<AuxTaskResolution> {
    let empty = AuxiliaryTaskConfig::default();
    let task_cfg = config.auxiliary.get(task).unwrap_or(&empty);

    let task_provider = task_cfg.provider();
    let task_model = task_cfg.model();
    let task_base_url = task_cfg.base_url();
    let task_key = task_cfg.resolved_api_key();

    // Nothing overridden at all → main runtime, main model.
    if task_provider.is_none()
        && task_model.is_none()
        && task_base_url.is_none()
        && task_key.is_none()
    {
        return Ok(AuxTaskResolution {
            provider: main,
            model: config.model.model.clone(),
            source: AuxSource::Main,
        });
    }

    let resolved_provider = task_provider
        .clone()
        .unwrap_or_else(|| config.model.provider.clone());
    let resolved_model = task_model
        .clone()
        .unwrap_or_else(|| config.model.model.clone());

    // Endpoint/key overrides or a different provider → dedicated client.
    let needs_dedicated = task_provider.is_some()
        || task_base_url.is_some()
        || task_key.is_some()
        || task_model.is_some();

    if !needs_dedicated {
        return Ok(AuxTaskResolution {
            provider: main,
            model: resolved_model,
            source: AuxSource::Main,
        });
    }

    let api_key = task_key.or_else(|| config.resolve_api_key());
    if api_key.is_none() && !is_keyless(&resolved_provider) {
        return Err(AgentError::config(format!(
            "auxiliary.{}: no API key (set api_key, key_env, or the main provider key)",
            task
        )));
    }
    let base_url = task_base_url.unwrap_or_else(|| config.resolve_base_url());
    let provider = build_task_provider(
        &resolved_provider,
        &resolved_model,
        &base_url,
        api_key.as_deref(),
        config.model.max_retries,
    )?;
    Ok(AuxTaskResolution {
        provider,
        model: resolved_model,
        source: AuxSource::TaskConfig,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ModelConfig;

    fn main_provider() -> Arc<dyn Provider> {
        Arc::new(
            OpenAiProvider::builder()
                .endpoint("http://localhost:9/v1")
                .model("main-model")
                .name("openai")
                .api_key("k")
                .build()
                .unwrap(),
        )
    }

    fn config_with(model: ModelConfig, toml_aux: &str) -> UlncLawConfig {
        let mut config = UlncLawConfig::default();
        config.model = model;
        if !toml_aux.is_empty() {
            config.auxiliary = toml::from_str(toml_aux).unwrap();
        }
        config
    }

    fn main_model_config() -> ModelConfig {
        ModelConfig {
            provider: "openai".into(),
            model: "main-model".into(),
            base_url: Some("http://localhost:9/v1".into()),
            api_key: Some("main-key".into()),
            ..ModelConfig::default()
        }
    }

    #[test]
    fn no_task_config_inherits_main() {
        let config = config_with(main_model_config(), "");
        let main = main_provider();
        let resolution = resolve_aux_task(&config, TASK_COMPRESSION, main.clone()).unwrap();
        assert_eq!(resolution.source, AuxSource::Main);
        assert_eq!(resolution.model, "main-model");
        assert!(Arc::ptr_eq(&resolution.provider, &main));
    }

    #[test]
    fn auto_values_inherit_main() {
        let config = config_with(
            main_model_config(),
            r#"[compression]
provider = "auto"
model = "auto"
"#,
        );
        let main = main_provider();
        let resolution = resolve_aux_task(&config, TASK_COMPRESSION, main.clone()).unwrap();
        assert_eq!(resolution.source, AuxSource::Main);
        assert!(Arc::ptr_eq(&resolution.provider, &main));
    }

    #[test]
    fn model_only_override_builds_fresh_instance() {
        let config = config_with(
            main_model_config(),
            r#"[compression]
model = "cheap-model"
"#,
        );
        let main = main_provider();
        let resolution = resolve_aux_task(&config, TASK_COMPRESSION, main.clone()).unwrap();
        assert_eq!(resolution.source, AuxSource::TaskConfig);
        assert_eq!(resolution.model, "cheap-model");
        assert!(!Arc::ptr_eq(&resolution.provider, &main));
        assert_eq!(resolution.provider.model(), "cheap-model");
        // Same endpoint as main (inherited base_url).
        assert_eq!(resolution.provider.name(), "openai");
    }

    #[test]
    fn provider_override_with_key_env() {
        // SAFETY: single-threaded test process; unique var name.
        std::env::set_var("ULNCLAW_TEST_AUX_KEY", "task-key");
        let config = config_with(
            main_model_config(),
            r#"[vision]
provider = "openai"
model = "gpt-vision"
base_url = "http://localhost:9/v1"
key_env = "ULNCLAW_TEST_AUX_KEY"
"#,
        );
        let resolution = resolve_aux_task(&config, TASK_VISION, main_provider()).unwrap();
        assert_eq!(resolution.source, AuxSource::TaskConfig);
        assert_eq!(resolution.model, "gpt-vision");
        std::env::remove_var("ULNCLAW_TEST_AUX_KEY");
    }

    #[test]
    fn keyless_provider_needs_no_key() {
        let mut model = main_model_config();
        model.api_key = None;
        let config = config_with(
            model,
            r#"[compression]
provider = "ollama"
model = "qwen3:1.7b"
"#,
        );
        let resolution = resolve_aux_task(&config, TASK_COMPRESSION, main_provider()).unwrap();
        assert_eq!(resolution.model, "qwen3:1.7b");
        assert_eq!(resolution.provider.name(), "ollama");
    }

    #[test]
    fn missing_key_for_cloud_provider_is_config_error() {
        // Serialize env state: other tests set OPENAI_API_KEY, which
        // would satisfy resolution and mask the missing-key error.
        let _guard = crate::models_dev::test_env_lock();
        let prev = std::env::var("OPENAI_API_KEY").ok();
        std::env::remove_var("OPENAI_API_KEY");
        let mut model = main_model_config();
        model.api_key = None;
        let config = config_with(
            model,
            r#"[compression]
provider = "openai"
model = "gpt-5.2"
"#,
        );
        let error = resolve_aux_task(&config, TASK_COMPRESSION, main_provider())
            .err()
            .unwrap();
        assert!(error.to_string().contains("auxiliary.compression"));
        match prev {
            Some(v) => std::env::set_var("OPENAI_API_KEY", v),
            None => std::env::remove_var("OPENAI_API_KEY"),
        }
    }
}

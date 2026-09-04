//! Video generation provider layer — port of hermes
//! `agent/video_gen_provider.py` + `agent/video_gen_registry.py`
//! (v2026.8.3).
//!
//! One unified tool (`video_generate`) covers text-to-video,
//! image-to-video, and reference-to-video; the router is the presence of
//! `image_url`. Video edit/extend are intentionally NOT part of this
//! surface — provider-specific tools ship separately (hermes
//! `xai_video_edit` / `xai_video_extend`).
//!
//! hermes discovers providers through plugins; ulnclaw compiles backends
//! in-tree and gates them on credentials via `is_available()`, which keeps
//! the registry semantics identical (single-available-provider
//! auto-select, configured-provider fail-closed).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{json, Value};

/// Common aspect ratios across providers (hermes `COMMON_ASPECT_RATIOS`).
/// The schema advertises this set as an enum hint; providers may accept a
/// narrower or wider set — they own clamping.
pub const COMMON_ASPECT_RATIOS: &[&str] = &["16:9", "9:16", "1:1", "4:3", "3:4", "3:2", "2:3"];
pub const DEFAULT_ASPECT_RATIO: &str = "16:9";

pub const COMMON_RESOLUTIONS: &[&str] = &["480p", "540p", "720p", "1080p"];
pub const DEFAULT_RESOLUTION: &str = "720p";

/// Unified generation request (the kwargs hermes passes to
/// `VideoGenProvider.generate`; `None` entries are dropped before the
/// call).
#[derive(Debug, Clone, Default)]
pub struct VideoGenParams {
    /// Resolved model id (explicit arg > config > provider default).
    pub model: String,
    /// True when the caller passed an explicit `model` argument.
    pub model_override_explicit: bool,
    /// Public HTTPS image URL — routes to image-to-video when present.
    pub image_url: Option<String>,
    /// Style/character reference image URLs (reference-to-video).
    pub reference_image_urls: Option<Vec<String>>,
    /// Desired duration in seconds (providers clamp).
    pub duration: Option<i64>,
    /// Output aspect ratio (providers clamp to their set).
    pub aspect_ratio: String,
    /// Output resolution bin (providers clamp).
    pub resolution: String,
    /// Content to avoid (Pixverse/Kling style; ignored elsewhere).
    pub negative_prompt: Option<String>,
    /// Audio generation toggle (Veo3/Pixverse; ignored elsewhere).
    pub audio: Option<bool>,
    /// Reproducibility seed (provider-dependent).
    pub seed: Option<i64>,
}

/// Pluggable video generation backend (hermes `VideoGenProvider` ABC).
#[async_trait]
pub trait VideoGenProvider: Send + Sync {
    /// Stable short identifier used in `video_gen.provider` config
    /// (lowercase, no spaces: `xai`, `fal`, ...).
    fn name(&self) -> &str;

    /// Human-readable label (hermes `display_name`, defaults to
    /// capitalized name).
    fn display_name(&self) -> String {
        let name = self.name();
        let mut chars = name.chars();
        match chars.next() {
            Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
            None => String::new(),
        }
    }

    /// True when this provider can service calls (credential check).
    fn is_available(&self) -> bool {
        true
    }

    /// Default model when neither the call nor the config pins one.
    fn default_model(&self) -> String;

    /// Run one generation; returns a `success_response` /
    /// `error_response` payload.
    async fn generate(&self, prompt: &str, params: VideoGenParams) -> Value;
}

/// Uniform success payload (hermes `success_response`).
pub fn success_response(
    video: &str,
    model: &str,
    prompt: &str,
    modality: &str,
    aspect_ratio: &str,
    duration: i64,
    provider: &str,
    extra: Option<Value>,
) -> Value {
    let mut payload = json!({
        "success": true,
        "video": video,
        "model": model,
        "prompt": prompt,
        "modality": modality,
        "aspect_ratio": aspect_ratio,
        "duration": duration,
        "provider": provider,
    });
    if let Some(Value::Object(extra)) = extra {
        for (key, value) in extra {
            payload
                .as_object_mut()
                .expect("payload is an object")
                .entry(key)
                .or_insert(value);
        }
    }
    payload
}

/// Uniform error payload (hermes `error_response`).
pub fn error_response(
    error: &str,
    error_type: &str,
    provider: &str,
    model: &str,
    prompt: &str,
) -> Value {
    json!({
        "success": false,
        "video": null,
        "error": error,
        "error_type": error_type,
        "model": model,
        "prompt": prompt,
        "aspect_ratio": "",
        "provider": provider,
    })
}

// ---------------------------------------------------------------------------
// Registry (hermes video_gen_registry)
// ---------------------------------------------------------------------------

static REGISTRY: Mutex<Option<HashMap<String, Arc<dyn VideoGenProvider>>>> = Mutex::new(None);

fn with_registry<F, T>(func: F) -> T
where
    F: FnOnce(&mut HashMap<String, Arc<dyn VideoGenProvider>>) -> T,
{
    let mut guard = REGISTRY.lock().expect("video_gen registry poisoned");
    func(guard.get_or_insert_with(HashMap::new))
}

/// Register a provider; re-registration overwrites (hermes
/// `register_provider`).
pub fn register_provider(provider: Arc<dyn VideoGenProvider>) {
    let name = provider.name().trim().to_string();
    assert!(
        !name.is_empty(),
        "video gen provider name must be non-empty"
    );
    with_registry(|map| {
        map.insert(name, provider);
    });
}

/// All registered providers sorted by name (hermes `list_providers`).
pub fn list_providers() -> Vec<Arc<dyn VideoGenProvider>> {
    let mut items = with_registry(|map| map.values().cloned().collect::<Vec<_>>());
    items.sort_by(|a, b| a.name().cmp(b.name()));
    items
}

/// Provider registered under `name` (hermes `get_provider`).
pub fn get_provider(name: &str) -> Option<Arc<dyn VideoGenProvider>> {
    with_registry(|map| map.get(name.trim()).cloned())
}

/// Resolve the currently-active provider (hermes `get_active_provider`):
///   1. `video_gen.provider` from config — fail closed when the name is
///      not registered;
///   2. otherwise, exactly one *available* provider auto-selects;
///   3. otherwise `None` (the tool surfaces a helpful error).
pub fn get_active_provider(
    config: &crate::config::UlncLawConfig,
) -> Option<Arc<dyn VideoGenProvider>> {
    let configured = config
        .video_gen
        .provider
        .as_deref()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());

    let snapshot = with_registry(|map| map.clone());

    if let Some(configured) = configured {
        if let Some(provider) = snapshot.get(&configured) {
            return Some(provider.clone());
        }
        tracing::debug!(
            "video_gen.provider='{}' configured but not registered; failing closed",
            configured
        );
        return None;
    }

    let available: Vec<_> = snapshot
        .values()
        .filter(|p| {
            // Wrap is_available so a buggy provider can't kill resolution.
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| p.is_available()))
                .unwrap_or(false)
        })
        .collect();
    if available.len() == 1 {
        return Some(available[0].clone());
    }
    None
}

/// Configured `video_gen.model` override (hermes
/// `_read_configured_video_model`).
pub fn configured_model(config: &crate::config::UlncLawConfig) -> Option<String> {
    config
        .video_gen
        .model
        .as_deref()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Drop all registrations (tests).
#[doc(hidden)]
pub fn reset_for_tests() {
    with_registry(|map| map.clear());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::UlncLawConfig;

    /// The registry is process-global; serialize the tests that mutate it.
    static REGISTRY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn registry_lock() -> std::sync::MutexGuard<'static, ()> {
        REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    struct StubProvider {
        name: &'static str,
        available: bool,
    }

    #[async_trait]
    impl VideoGenProvider for StubProvider {
        fn name(&self) -> &str {
            self.name
        }
        fn is_available(&self) -> bool {
            self.available
        }
        fn default_model(&self) -> String {
            format!("{}-model", self.name)
        }
        async fn generate(&self, prompt: &str, params: VideoGenParams) -> Value {
            success_response(
                "https://example.test/video.mp4",
                &params.model,
                prompt,
                "text",
                "",
                0,
                self.name,
                None,
            )
        }
    }

    #[test]
    fn registry_roundtrip_and_active_resolution() {
        let _guard = registry_lock();
        reset_for_tests();
        register_provider(Arc::new(StubProvider {
            name: "alpha",
            available: true,
        }));
        register_provider(Arc::new(StubProvider {
            name: "beta",
            available: false,
        }));

        assert_eq!(list_providers().len(), 2);
        assert!(get_provider("alpha").is_some());
        assert!(get_provider("missing").is_none());

        // Two registered but only one available → auto-select.
        let config = UlncLawConfig::default();
        let active = get_active_provider(&config).expect("single available auto-selects");
        assert_eq!(active.name(), "alpha");

        // Configured name wins.
        let mut config = UlncLawConfig::default();
        config.video_gen.provider = Some("beta".to_string());
        let active = get_active_provider(&config).expect("configured resolves");
        assert_eq!(active.name(), "beta");

        // Configured-but-unregistered fails closed.
        config.video_gen.provider = Some("ghost".to_string());
        assert!(get_active_provider(&config).is_none());

        reset_for_tests();
    }

    #[test]
    fn no_available_provider_resolves_none() {
        let _guard = registry_lock();
        reset_for_tests();
        register_provider(Arc::new(StubProvider {
            name: "a",
            available: true,
        }));
        register_provider(Arc::new(StubProvider {
            name: "b",
            available: true,
        }));
        let config = UlncLawConfig::default();
        assert!(get_active_provider(&config).is_none());
        reset_for_tests();
    }

    #[test]
    fn response_shapes() {
        let ok = success_response("v.mp4", "m", "p", "text", "16:9", 5, "xai", None);
        assert_eq!(ok["success"], json!(true));
        assert_eq!(ok["video"], json!("v.mp4"));
        let err = error_response("boom", "provider_error", "xai", "m", "p");
        assert_eq!(err["success"], json!(false));
        assert_eq!(err["error"], json!("boom"));
    }
}

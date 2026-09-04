//! Provider/model inventory for `GET /api/model/options` — port of
//! hermes' `hermes_cli/inventory.py` substrate (`build_model_options_payload`).
//!
//! Builds the `{providers, model, provider}` picker payload from:
//!
//! 1. the configured current provider (enriched from the models.dev
//!    catalog: model list, per-model capabilities/costs),
//! 2. user-defined `[providers.<slug>]` config entries (hermes v12+
//!    keyed `providers:` form), live-probed for OpenAI-compatible
//!    endpoints under the hermes probe policy,
//! 3. canonical providers whose API-key env var is present,
//! 4. (with `include_unconfigured`) skeleton rows for the remaining
//!    canonical providers with `auth_type`/`key_env`/`warning` hints.
//!
//! Not ported: hermes' credential-pool rows (ulnclaw keeps one key per
//! provider) and the Nous free-tier gating (no Nous Portal provider).

use serde_json::{json, Value};

use crate::config::{CustomProviderConfig, UlncLawConfig};

/// Probe budget for live `/models` discovery (hermes keeps picker opens
/// snappy; offline endpoints must not block the dialog).
const PROBE_TIMEOUT_MS: u64 = 2000;

/// How many models per lab the featured shortlist keeps (hermes
/// `_FEATURED_PER_LAB`).
const FEATURED_PER_LAB: usize = 5;

/// A canonical provider known to ulnclaw's runtime (subset of hermes'
/// `CANONICAL_PROVIDERS` that ulnclaw can actually speak to: OpenAI
/// compatible endpoints + the native Anthropic dialect + keyless local
/// servers).
pub struct CanonicalProvider {
    pub slug: &'static str,
    pub label: &'static str,
    /// API-key env var; empty for keyless local endpoints.
    pub key_env: &'static str,
    pub default_endpoint: &'static str,
    /// Wire dialect: `openai` or `anthropic`.
    pub mode: &'static str,
}

pub const CANONICAL_PROVIDERS: &[CanonicalProvider] = &[
    CanonicalProvider {
        slug: "openai",
        label: "OpenAI",
        key_env: "OPENAI_API_KEY",
        default_endpoint: "https://api.openai.com/v1",
        mode: "openai",
    },
    CanonicalProvider {
        slug: "anthropic",
        label: "Anthropic",
        key_env: "ANTHROPIC_API_KEY",
        default_endpoint: "https://api.anthropic.com",
        mode: "anthropic",
    },
    CanonicalProvider {
        slug: "openrouter",
        label: "OpenRouter",
        key_env: "OPENROUTER_API_KEY",
        default_endpoint: "https://openrouter.ai/api/v1",
        mode: "openai",
    },
    CanonicalProvider {
        slug: "dashscope",
        label: "DashScope (Qwen)",
        key_env: "DASHSCOPE_API_KEY",
        default_endpoint: "https://dashscope.aliyuncs.com/compatible-mode/v1",
        mode: "openai",
    },
    CanonicalProvider {
        slug: "deepseek",
        label: "DeepSeek",
        key_env: "DEEPSEEK_API_KEY",
        default_endpoint: "https://api.deepseek.com/v1",
        mode: "openai",
    },
    CanonicalProvider {
        slug: "xai",
        label: "xAI",
        key_env: "XAI_API_KEY",
        default_endpoint: "https://api.x.ai/v1",
        mode: "openai",
    },
    CanonicalProvider {
        slug: "gemini",
        label: "Google AI Studio",
        key_env: "GOOGLE_API_KEY",
        default_endpoint: "https://generativelanguage.googleapis.com/v1beta/openai",
        mode: "openai",
    },
    CanonicalProvider {
        slug: "groq",
        label: "Groq",
        key_env: "GROQ_API_KEY",
        default_endpoint: "https://api.groq.com/openai/v1",
        mode: "openai",
    },
    CanonicalProvider {
        slug: "mistral",
        label: "Mistral",
        key_env: "MISTRAL_API_KEY",
        default_endpoint: "https://api.mistral.ai/v1",
        mode: "openai",
    },
    CanonicalProvider {
        slug: "fireworks",
        label: "Fireworks AI",
        key_env: "FIREWORKS_API_KEY",
        default_endpoint: "https://api.fireworks.ai/inference/v1",
        mode: "openai",
    },
    CanonicalProvider {
        slug: "togetherai",
        label: "Together AI",
        key_env: "TOGETHER_API_KEY",
        default_endpoint: "https://api.together.xyz/v1",
        mode: "openai",
    },
    CanonicalProvider {
        slug: "perplexity",
        label: "Perplexity",
        key_env: "PERPLEXITY_API_KEY",
        default_endpoint: "https://api.perplexity.ai",
        mode: "openai",
    },
    CanonicalProvider {
        slug: "cohere",
        label: "Cohere",
        key_env: "COHERE_API_KEY",
        default_endpoint: "https://api.cohere.ai/compatibility/v1",
        mode: "openai",
    },
    CanonicalProvider {
        slug: "ollama",
        label: "Ollama",
        key_env: "",
        default_endpoint: "http://localhost:11434/v1",
        mode: "openai",
    },
    CanonicalProvider {
        slug: "llamacpp",
        label: "llama.cpp",
        key_env: "",
        default_endpoint: "http://localhost:8080/v1",
        mode: "openai",
    },
];

pub fn canonical_by_slug(slug: &str) -> Option<&'static CanonicalProvider> {
    CANONICAL_PROVIDERS
        .iter()
        .find(|p| p.slug.eq_ignore_ascii_case(slug.trim()))
}

/// Env vars whose presence authenticates a canonical provider row.
pub fn canonical_key_envs() -> Vec<&'static str> {
    CANONICAL_PROVIDERS
        .iter()
        .filter(|p| !p.key_env.is_empty())
        .map(|p| p.key_env)
        .collect()
}

#[derive(Debug, Clone, Default)]
pub struct InventoryOptions {
    /// Bust the models.dev cache and probe every configured endpoint.
    pub refresh: bool,
    /// Append skeleton rows for unconfigured canonical providers.
    pub include_unconfigured: bool,
    /// Keep only rows backed by explicit user configuration.
    pub explicit_only: bool,
}

/// Snapshot of everything the inventory needs (hermes `ConfigContext`).
#[derive(Debug, Clone, Default)]
pub struct InventoryInput {
    pub current_provider: String,
    pub current_model: String,
    pub current_base_url: String,
    /// API key for the current provider (used by live probes).
    pub current_api_key: Option<String>,
    /// True when `[model] base_url` was explicitly configured (probes
    /// only run against user-set endpoints, never derived defaults).
    pub current_base_url_explicit: bool,
    /// `[providers.<slug>]` entries, sorted by slug for determinism.
    pub providers: Vec<(String, CustomProviderConfig)>,
    pub excluded_providers: Vec<String>,
    /// `[moa] presets` names — surface the virtual `moa` provider row.
    pub moa_presets: Vec<String>,
}

impl InventoryInput {
    /// Build the input from disk config; callers with a live runtime
    /// override `current_provider`/`current_model` afterwards.
    pub fn from_config(cfg: &UlncLawConfig) -> Self {
        let mut providers: Vec<(String, CustomProviderConfig)> = cfg
            .providers
            .iter()
            .map(|(slug, entry)| (slug.trim().to_lowercase(), entry.clone()))
            .filter(|(slug, _)| !slug.is_empty())
            .collect();
        providers.sort_by(|a, b| a.0.cmp(&b.0));
        Self {
            current_provider: cfg.model.provider.clone(),
            current_model: cfg.model.model.clone(),
            current_base_url: cfg.resolve_base_url(),
            current_api_key: cfg.model.api_key.clone().filter(|k| !k.trim().is_empty()),
            current_base_url_explicit: cfg
                .model
                .base_url
                .as_ref()
                .map(|u| !u.trim().is_empty())
                .unwrap_or(false),
            providers,
            excluded_providers: cfg
                .model_catalog
                .excluded_providers
                .iter()
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .collect(),
            moa_presets: {
                let mut names: Vec<String> = cfg.moa.presets.keys().cloned().collect();
                names.sort();
                names
            },
        }
    }
}

/// One provider row under construction.
#[derive(Debug, Clone)]
struct Row {
    slug: String,
    name: String,
    models: Vec<String>,
    is_user_defined: bool,
    authenticated: bool,
    is_current: bool,
    /// `current` | `config` | `env` | `canonical`.
    source: &'static str,
    mode: String,
    base_url: String,
    key_env: String,
    /// Models came from a live `/models` probe.
    probed: bool,
}

impl Row {
    fn new(slug: &str, source: &'static str) -> Self {
        Self {
            slug: slug.to_string(),
            name: String::new(),
            models: Vec::new(),
            is_user_defined: false,
            authenticated: false,
            is_current: false,
            source,
            mode: "openai".to_string(),
            base_url: String::new(),
            key_env: String::new(),
            probed: false,
        }
    }
}

/// `GET {base}/models` against an OpenAI-compatible endpoint.
fn probe_openai_models(base_url: &str, api_key: Option<&str>) -> Option<Vec<String>> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_millis(PROBE_TIMEOUT_MS))
        .build()
        .ok()?;
    let mut req = client.get(&url);
    if let Some(key) = api_key.filter(|k| !k.is_empty()) {
        req = req.bearer_auth(key);
    }
    let resp = req.send().ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body: Value = resp.json().ok()?;
    let ids: Vec<String> = body
        .get("data")?
        .as_array()?
        .iter()
        .filter_map(|m| m.get("id").and_then(|v| v.as_str()).map(str::to_string))
        .filter(|id| !id.is_empty())
        .collect();
    if ids.is_empty() {
        None
    } else {
        Some(ids)
    }
}

/// hermes `_format_price_per_mtok`: $/Mtok with 2 decimals, extended
/// precision below one cent, `free` for zero.
pub fn format_price_per_mtok(per_mtok: f64) -> String {
    if per_mtok <= 0.0 {
        return "free".to_string();
    }
    if per_mtok >= 0.01 {
        return format!("${:.2}", per_mtok);
    }
    let formatted = format!("{:.6}", per_mtok);
    let trimmed = formatted
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_string();
    format!("${}", trimmed)
}

/// Per-model capability/cost enrichment shared by every row (same shape
/// the single-row handler produced before the inventory port).
fn enrich_from_catalog(row: &mut Row) {
    if row.name.is_empty() {
        if let Some(info) = crate::models_dev::get_provider_info(&row.slug, false) {
            row.name = info.name;
        }
    }
    if row.models.is_empty() {
        let models = crate::models_dev::list_provider_models(&row.slug);
        if !models.is_empty() {
            row.models = models;
        }
    }
}

/// Featured shortlist for multi-lab aggregator rows (hermes
/// `_apply_featured`): newest `FEATURED_PER_LAB` per vendor lab by
/// models.dev `release_date`, ranked among the row's OWN models.
fn featured_models(slug: &str, models: &[String]) -> Vec<String> {
    let mut by_lab: std::collections::HashMap<String, Vec<(usize, String, String)>> =
        std::collections::HashMap::new();
    for (pos, model) in models.iter().enumerate() {
        let Some((lab, _rest)) = model.split_once('/') else {
            // No vendor prefix → single-namespace provider, not an
            // aggregator: no shortlist.
            return Vec::new();
        };
        if lab.is_empty() {
            return Vec::new();
        }
        let date = crate::models_dev::get_model_info(slug, model)
            .map(|info| info.release_date)
            .unwrap_or_default();
        by_lab
            .entry(lab.to_string())
            .or_default()
            .push((pos, date, model.clone()));
    }
    if by_lab.len() < 2 {
        return Vec::new();
    }
    let mut featured = Vec::new();
    for entries in by_lab.values() {
        let mut ranked = entries.clone();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| (a.0 as i64).cmp(&(b.0 as i64))));
        featured.extend(ranked.into_iter().take(FEATURED_PER_LAB).map(|e| e.2));
    }
    let order: std::collections::HashMap<&str, usize> = models
        .iter()
        .enumerate()
        .map(|(i, m)| (m.as_str(), i))
        .collect();
    featured.sort_by_key(|m| order.get(m.as_str()).copied().unwrap_or(usize::MAX));
    featured
}

/// Render one row to its JSON shape (catalog metadata, capabilities,
/// pricing, featured).
fn render_row(row: &Row, cache_fresh: bool) -> Value {
    let mut out = json!({
        "slug": row.slug,
        "models": row.models,
        "total_models": row.models.len(),
        "is_user_defined": row.is_user_defined,
        "authenticated": row.authenticated,
        "source": row.source,
    });
    if row.is_current {
        out["current"] = json!(true);
    }
    if !row.name.is_empty() {
        out["name"] = json!(row.name);
    }
    if !row.base_url.is_empty() {
        out["base_url"] = json!(row.base_url);
    }
    if row.mode == "anthropic" {
        out["mode"] = json!("anthropic");
    }
    if row.probed {
        out["probed"] = json!(true);
    }

    // Catalog metadata + per-model enrichment (models.dev).
    if !row.models.is_empty() {
        if let Some(info) = crate::models_dev::get_provider_info(&row.slug, false) {
            if row.name.is_empty() {
                out["name"] = json!(info.name);
            }
            if !info.api.is_empty() {
                out["api"] = json!(info.api);
            }
            if !info.doc.is_empty() {
                out["doc"] = json!(info.doc);
            }
        }
        if !row.probed {
            out["catalog"] = json!("models.dev");
            out["catalog_stale"] = json!(!cache_fresh);
        }
        let mut capabilities = serde_json::Map::new();
        let mut pricing = serde_json::Map::new();
        for model_id in &row.models {
            let Some(info) = crate::models_dev::get_model_info(&row.slug, model_id) else {
                continue;
            };
            let mut caps = json!({
                "reasoning": info.reasoning,
                "tools": info.tool_call,
                "vision": info.supports_vision(),
                "context_window": info.context_window,
                "max_output_tokens": info.max_output,
            });
            if !info.family.is_empty() {
                caps["family"] = json!(info.family);
            }
            if info.has_cost_data() {
                caps["cost"] = json!({
                    "input_per_mtok": info.cost_input,
                    "output_per_mtok": info.cost_output,
                });
                pricing.insert(
                    model_id.clone(),
                    json!({
                        "input": format_price_per_mtok(info.cost_input),
                        "output": format_price_per_mtok(info.cost_output),
                    }),
                );
            }
            capabilities.insert(model_id.clone(), caps);
        }
        if !capabilities.is_empty() {
            out["capabilities"] = Value::Object(capabilities);
        }
        if !pricing.is_empty() {
            out["pricing"] = Value::Object(pricing);
        }
        let featured = featured_models(&row.slug, &row.models);
        if !featured.is_empty() {
            out["featured_models"] = json!(featured);
        }
    }
    out
}

/// Build the `{providers, model, provider}` payload (hermes
/// `build_model_options_payload`). Runs on a blocking thread: catalog
/// reads are memory-cached, probes are bounded by `PROBE_TIMEOUT_MS`.
pub fn build_model_options_payload(input: &InventoryInput, opts: &InventoryOptions) -> Value {
    // Prime the catalog: force_refresh busts the TTL cache (hermes
    // `refresh` semantics); later typed lookups hit the in-memory cache.
    crate::models_dev::fetch_models_dev_opts(opts.refresh, true);
    let cache = crate::models_dev::cache_info();
    let mut rows: Vec<Row> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    // 1. Current configured provider row.
    let current_slug = input.current_provider.trim().to_lowercase();
    if !current_slug.is_empty() {
        let mut row = Row::new(&current_slug, "current");
        row.name.clone_from(&row.slug);
        row.is_user_defined = true;
        row.authenticated = true;
        row.is_current = true;
        row.base_url = input.current_base_url.clone();
        if let Some(canon) = canonical_by_slug(&current_slug) {
            row.name = canon.label.to_string();
            row.mode = canon.mode.to_string();
        }
        row.models = vec![input.current_model.clone()];
        // Catalog models replace the lone configured model when known
        // (hermes curated-catalog semantics); the catalog name wins.
        let catalog_models = crate::models_dev::list_provider_models(&current_slug);
        if !catalog_models.is_empty() {
            row.models = catalog_models;
            if let Some(info) = crate::models_dev::get_provider_info(&current_slug, false) {
                if !info.name.is_empty() {
                    row.name = info.name;
                }
            }
        }
        enrich_from_catalog(&mut row);
        // Probe policy (hermes): normal opens probe only the current
        // custom endpoint; refresh probes everything.
        if row.mode == "openai"
            && input.current_base_url_explicit
            && !input.current_base_url.is_empty()
        {
            if let Some(models) =
                probe_openai_models(&input.current_base_url, input.current_api_key.as_deref())
            {
                row.models = models;
                row.probed = true;
            }
        }
        seen.insert(current_slug.clone());
        rows.push(row);
    }

    // 2. `[providers.<slug>]` config rows (hermes keyed providers form).
    for (slug, entry) in &input.providers {
        if seen.contains(slug) {
            continue;
        }
        let mut row = Row::new(slug, "config");
        row.is_user_defined = true;
        row.mode = entry.mode();
        row.base_url = entry.base_url().unwrap_or_default();
        if let Some(canon) = canonical_by_slug(slug) {
            row.name = canon.label.to_string();
            if row.base_url.is_empty() {
                row.base_url = canon.default_endpoint.to_string();
            }
        } else {
            row.name.clone_from(slug);
        }
        if let Some(model) = entry.model() {
            row.models = vec![model];
        }
        let has_key = entry.resolved_api_key().is_some();
        row.authenticated = has_key || row.mode == "openai" && !row.base_url.is_empty();
        enrich_from_catalog(&mut row);
        let probe = opts.refresh || row.is_current;
        if probe && row.mode == "openai" && !row.base_url.is_empty() {
            let key = entry.resolved_api_key();
            if let Some(models) = probe_openai_models(&row.base_url, key.as_deref()) {
                row.models = models;
                row.probed = true;
            }
        }
        seen.insert(slug.clone());
        rows.push(row);
    }

    // Virtual MoA row (hermes `_moa_provider_row`): preset names as the
    // model list, present whenever the user configured presets.
    if !input.moa_presets.is_empty() && !seen.contains("moa") {
        let mut row = Row::new("moa", "config");
        row.name = "Mixture of Agents".to_string();
        row.authenticated = true;
        row.models = input.moa_presets.clone();
        seen.insert("moa".to_string());
        rows.push(row);
    }

    // 3. Canonical providers authenticated by their env key.
    for canon in CANONICAL_PROVIDERS {
        let slug = canon.slug.to_string();
        if seen.contains(&slug) {
            continue;
        }
        if canon.key_env.is_empty() {
            // Keyless local servers (ollama/llama.cpp) surface only as
            // `[providers.<slug>]` config rows or skeletons — ambient
            // endpoint probing would make the inventory non-deterministic.
            continue;
        }
        if crate::config::get_env_value(canon.key_env).is_none() {
            continue;
        }
        let mut row = Row::new(&slug, "env");
        row.name = canon.label.to_string();
        row.authenticated = true;
        row.mode = canon.mode.to_string();
        row.base_url = canon.default_endpoint.to_string();
        row.key_env = canon.key_env.to_string();
        enrich_from_catalog(&mut row);
        seen.insert(slug);
        rows.push(row);
    }

    // 4. Skeleton rows for the remaining canonical providers.
    if opts.include_unconfigured {
        for canon in CANONICAL_PROVIDERS {
            let slug = canon.slug.to_string();
            if seen.contains(&slug) {
                continue;
            }
            let mut row = Row::new(&slug, "canonical");
            row.name = canon.label.to_string();
            row.authenticated = false;
            row.key_env = canon.key_env.to_string();
            seen.insert(slug);
            rows.push(row);
        }
    }

    // Excluded providers (hermes `model_catalog.excluded_providers`).
    if !input.excluded_providers.is_empty() {
        rows.retain(|r| {
            !input
                .excluded_providers
                .iter()
                .any(|x| x == &r.slug.to_lowercase())
        });
    }

    // Explicit-only: keep rows backed by explicit user configuration.
    if opts.explicit_only {
        rows.retain(|r| r.is_user_defined || r.is_current || r.source == "env" || r.slug == "moa");
    }

    // Canonical declaration order first, custom rows last (hermes
    // `_reorder_canonical`, keyed on slug membership).
    let order: std::collections::HashMap<&str, usize> = CANONICAL_PROVIDERS
        .iter()
        .enumerate()
        .map(|(i, p)| (p.slug, i))
        .collect();
    rows.sort_by_key(|r| order.get(r.slug.as_str()).copied().unwrap_or(usize::MAX));
    // hermes puts the MoA row first when present.
    if let Some(pos) = rows.iter().position(|r| r.slug == "moa") {
        let moa = rows.remove(pos);
        rows.insert(0, moa);
    }

    let providers: Vec<Value> = rows
        .iter()
        .map(|row| {
            let mut value = render_row(row, cache.fresh);
            if row.source == "canonical" && !row.authenticated {
                // Picker setup hints (hermes `_apply_picker_hints`).
                let auth_type = if row.key_env.is_empty() {
                    "none"
                } else {
                    "api_key"
                };
                value["auth_type"] = json!(auth_type);
                if !row.key_env.is_empty() {
                    value["key_env"] = json!(row.key_env);
                }
                value["warning"] = json!(if row.key_env.is_empty() {
                    "start the server, then refresh".to_string()
                } else {
                    format!("paste {} to activate", row.key_env)
                });
            } else if !row.key_env.is_empty() {
                value["key_env"] = json!(row.key_env);
            }
            value
        })
        .collect();

    let mut payload = json!({
        "providers": providers,
        "model": input.current_model,
        "provider": input.current_provider,
    });
    payload["catalog_cache"] = json!({
        "providers": cache.providers,
        "age_secs": cache.age_secs.round() as u64,
        "fresh": cache.fresh,
    });
    payload
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Save/remove/restore the canonical key env vars + ULNCLAW_HOME so
    /// inventory tests never see ambient credentials.
    struct EnvScrub {
        saved: Vec<(String, Option<String>)>,
        home: Option<String>,
    }

    impl EnvScrub {
        fn new(home: &std::path::Path) -> Self {
            let mut saved = Vec::new();
            for name in canonical_key_envs() {
                saved.push((name.to_string(), std::env::var(name).ok()));
                std::env::remove_var(name);
            }
            let home_saved = std::env::var("ULNCLAW_HOME").ok();
            std::env::set_var("ULNCLAW_HOME", home);
            Self {
                saved,
                home: home_saved,
            }
        }
    }

    impl Drop for EnvScrub {
        fn drop(&mut self) {
            for (name, value) in self.saved.drain(..) {
                match value {
                    Some(v) => std::env::set_var(name, v),
                    None => std::env::remove_var(name),
                }
            }
            match self.home.take() {
                Some(v) => std::env::set_var("ULNCLAW_HOME", v),
                None => std::env::remove_var("ULNCLAW_HOME"),
            }
        }
    }

    fn fixture_registry(dir: &std::path::Path) {
        let fixture = dir.join("models-dev.json");
        std::fs::write(
            &fixture,
            json!({
                "openrouter": {
                    "name": "OpenRouter",
                    "api": "https://openrouter.ai/api/v1",
                    "doc": "https://openrouter.ai/docs",
                    "models": {
                        "openai/gpt-5": {
                            "tool_call": true, "reasoning": true,
                            "limit": {"context": 200000, "output": 16000},
                            "cost": {"input": 1.25, "output": 10.0},
                            "release_date": "2025-08-07"
                        },
                        "openai/gpt-5-mini": {
                            "tool_call": true,
                            "release_date": "2025-08-07"
                        },
                        "anthropic/claude-4": {
                            "tool_call": true, "reasoning": true,
                            "release_date": "2025-09-01"
                        }
                    }
                },
                "openai": {
                    "name": "OpenAI",
                    "api": "https://api.openai.com/v1",
                    "doc": "https://platform.openai.com/docs",
                    "models": {
                        "gpt-5": {
                            "tool_call": true, "reasoning": true,
                            "limit": {"context": 128000, "output": 8192},
                            "cost": {"input": 0.5, "output": 1.5}
                        }
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
        std::env::set_var(
            crate::models_dev::MODELS_DEV_URL_ENV,
            format!("file://{}", fixture.display()),
        );
        std::env::set_var(
            crate::models_dev::MODELS_DEV_CACHE_ENV,
            dir.join("cache.json").display().to_string(),
        );
        crate::models_dev::reset_cache_for_tests();
    }

    fn base_input() -> InventoryInput {
        InventoryInput {
            current_provider: "openrouter".to_string(),
            current_model: "openai/gpt-5".to_string(),
            current_base_url: "https://openrouter.ai/api/v1".to_string(),
            current_api_key: None,
            current_base_url_explicit: false,
            moa_presets: Vec::new(),
            providers: Vec::new(),
            excluded_providers: Vec::new(),
        }
    }

    #[test]
    fn test_price_formatting() {
        assert_eq!(format_price_per_mtok(0.0), "free");
        assert_eq!(format_price_per_mtok(3.0), "$3.00");
        assert_eq!(format_price_per_mtok(0.0018), "$0.0018");
        assert_eq!(format_price_per_mtok(0.001), "$0.001");
        assert_eq!(format_price_per_mtok(180.0), "$180.00");
    }

    #[test]
    fn test_current_row_catalog_enrichment() {
        let _guard = crate::models_dev::test_env_lock();
        let dir = tempfile::tempdir().unwrap();
        let _scrub = EnvScrub::new(dir.path());
        fixture_registry(dir.path());

        let payload = build_model_options_payload(&base_input(), &InventoryOptions::default());
        let providers = payload["providers"].as_array().unwrap();
        assert_eq!(providers.len(), 1);
        let row = &providers[0];
        assert_eq!(row["slug"], "openrouter");
        assert_eq!(row["current"], true);
        assert_eq!(row["authenticated"], true);
        assert_eq!(row["name"], "OpenRouter");
        assert_eq!(row["catalog"], "models.dev");
        assert_eq!(row["total_models"], 3);
        assert_eq!(row["capabilities"]["openai/gpt-5"]["reasoning"], true);
        assert_eq!(row["pricing"]["openai/gpt-5"]["input"], "$1.25");
        // Multi-lab aggregator row gets a featured shortlist.
        let featured = row["featured_models"].as_array().unwrap();
        assert!(featured.iter().any(|m| m == "anthropic/claude-4"));
        assert!(payload["catalog_cache"]["providers"].as_u64().unwrap() >= 1);

        std::env::remove_var(crate::models_dev::MODELS_DEV_URL_ENV);
        std::env::remove_var(crate::models_dev::MODELS_DEV_CACHE_ENV);
        crate::models_dev::reset_cache_for_tests();
    }

    #[test]
    fn test_env_authenticated_canonical_rows_and_order() {
        let _guard = crate::models_dev::test_env_lock();
        let dir = tempfile::tempdir().unwrap();
        let _scrub = EnvScrub::new(dir.path());
        fixture_registry(dir.path());
        std::env::set_var("OPENAI_API_KEY", "sk-test");

        let payload = build_model_options_payload(&base_input(), &InventoryOptions::default());
        let providers = payload["providers"].as_array().unwrap();
        let slugs: Vec<&str> = providers
            .iter()
            .map(|r| r["slug"].as_str().unwrap())
            .collect();
        // Canonical order: openai before openrouter; openai row is
        // env-authenticated with catalog models.
        assert_eq!(slugs, vec!["openai", "openrouter"]);
        let openai = &providers[0];
        assert_eq!(openai["source"], "env");
        assert_eq!(openai["authenticated"], true);
        assert_eq!(openai["key_env"], "OPENAI_API_KEY");
        assert_eq!(openai["total_models"], 1);

        std::env::remove_var(crate::models_dev::MODELS_DEV_URL_ENV);
        std::env::remove_var(crate::models_dev::MODELS_DEV_CACHE_ENV);
        crate::models_dev::reset_cache_for_tests();
    }

    #[test]
    fn test_include_unconfigured_skeletons_and_explicit_filter() {
        let _guard = crate::models_dev::test_env_lock();
        let dir = tempfile::tempdir().unwrap();
        let _scrub = EnvScrub::new(dir.path());
        fixture_registry(dir.path());
        std::env::set_var("OPENAI_API_KEY", "sk-test");

        let payload = build_model_options_payload(
            &base_input(),
            &InventoryOptions {
                include_unconfigured: true,
                ..Default::default()
            },
        );
        let providers = payload["providers"].as_array().unwrap();
        assert_eq!(providers.len(), CANONICAL_PROVIDERS.len());
        let skeleton = providers.iter().find(|r| r["slug"] == "anthropic").unwrap();
        assert_eq!(skeleton["authenticated"], false);
        assert_eq!(skeleton["auth_type"], "api_key");
        assert_eq!(skeleton["key_env"], "ANTHROPIC_API_KEY");
        assert_eq!(skeleton["warning"], "paste ANTHROPIC_API_KEY to activate");
        let keyless = providers.iter().find(|r| r["slug"] == "ollama").unwrap();
        assert_eq!(keyless["auth_type"], "none");

        // explicit_only drops skeleton rows but keeps env + current rows.
        let payload = build_model_options_payload(
            &base_input(),
            &InventoryOptions {
                include_unconfigured: true,
                explicit_only: true,
                ..Default::default()
            },
        );
        let slugs: Vec<&str> = payload["providers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["slug"].as_str().unwrap())
            .collect();
        assert_eq!(slugs, vec!["openai", "openrouter"]);

        std::env::remove_var(crate::models_dev::MODELS_DEV_URL_ENV);
        std::env::remove_var(crate::models_dev::MODELS_DEV_CACHE_ENV);
        crate::models_dev::reset_cache_for_tests();
    }

    #[test]
    fn test_config_provider_rows_and_exclusion() {
        let _guard = crate::models_dev::test_env_lock();
        let dir = tempfile::tempdir().unwrap();
        let _scrub = EnvScrub::new(dir.path());
        fixture_registry(dir.path());

        let mut input = base_input();
        input.providers = vec![(
            "local".to_string(),
            CustomProviderConfig {
                base_url: Some("http://127.0.0.1:9999/v1".to_string()),
                model: Some("my-model".to_string()),
                ..Default::default()
            },
        )];
        input.excluded_providers = vec!["openrouter".to_string()];

        let payload = build_model_options_payload(&input, &InventoryOptions::default());
        let slugs: Vec<&str> = payload["providers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["slug"].as_str().unwrap())
            .collect();
        // openrouter excluded; custom row last (non-canonical).
        assert_eq!(slugs, vec!["local"]);
        let local = &payload["providers"][0];
        assert_eq!(local["is_user_defined"], true);
        assert_eq!(local["authenticated"], true);
        assert_eq!(local["base_url"], "http://127.0.0.1:9999/v1");
        assert_eq!(local["models"], json!(["my-model"]));

        std::env::remove_var(crate::models_dev::MODELS_DEV_URL_ENV);
        std::env::remove_var(crate::models_dev::MODELS_DEV_CACHE_ENV);
        crate::models_dev::reset_cache_for_tests();
    }

    #[test]
    fn test_moa_virtual_row_first_and_explicit() {
        let _guard = crate::models_dev::test_env_lock();
        let dir = tempfile::tempdir().unwrap();
        let _scrub = EnvScrub::new(dir.path());
        fixture_registry(dir.path());

        let mut input = base_input();
        input.moa_presets = vec!["default".to_string(), "fast".to_string()];

        let payload = build_model_options_payload(
            &input,
            &InventoryOptions {
                explicit_only: true,
                ..Default::default()
            },
        );
        let providers = payload["providers"].as_array().unwrap();
        assert_eq!(providers[0]["slug"], "moa");
        assert_eq!(providers[0]["name"], "Mixture of Agents");
        assert_eq!(providers[0]["models"], json!(["default", "fast"]));

        std::env::remove_var(crate::models_dev::MODELS_DEV_URL_ENV);
        std::env::remove_var(crate::models_dev::MODELS_DEV_CACHE_ENV);
        crate::models_dev::reset_cache_for_tests();
    }

    #[test]
    fn test_probe_openai_models_live() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0u8; 2048];
            let _ = sock.read(&mut buf);
            let body = r#"{"data":[{"id":"model-a"},{"id":"model-b"}]}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(resp.as_bytes());
        });
        let models = probe_openai_models(&format!("http://{}/v1", addr), Some("k")).unwrap();
        server.join().unwrap();
        assert_eq!(models, vec!["model-a".to_string(), "model-b".to_string()]);
    }
}

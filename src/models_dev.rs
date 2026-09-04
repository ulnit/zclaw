//! models.dev registry integration — multi-provider model catalog.
//!
//! Port of hermes `agent/models_dev.py` (v2026.8.3). Fetches
//! `https://models.dev/api.json` — a community-maintained database of
//! thousands of models across 100+ providers — providing:
//!
//! - **Provider metadata**: name, base URL, env vars, documentation link
//! - **Model metadata**: context window, max output, cost/M tokens,
//!   capabilities (reasoning, tools, vision, PDF, audio), modalities,
//!   knowledge cutoff, open-weights flag, family grouping, status
//!
//! Data resolution order:
//!   1. In-memory cache (fresh, or stale served immediately while a single
//!      background thread refreshes)
//!   2. Disk cache (`$ULNCLAW_HOME/models_dev_cache.json` — any age; stale
//!      data is served rather than blocking callers on the network)
//!   3. Network fetch — only when no cache exists at all; failed refreshes
//!      back off for 5 minutes process-wide
//!
//! Latency-sensitive callers pass `allow_network = false` and never touch
//! the network. The registry URL can be overridden with
//! `ULNCLAW_MODELS_DEV_URL` (http(s) endpoints or `file://` paths for
//! mirrors/tests).

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use regex::Regex;
use serde::Serialize;
use serde_json::Value;

pub const DEFAULT_MODELS_DEV_URL: &str = "https://models.dev/api.json";
/// Env override for the registry URL (http(s):// or file://).
pub const MODELS_DEV_URL_ENV: &str = "ULNCLAW_MODELS_DEV_URL";

const CACHE_TTL_SECS: f64 = 3600.0; // 1 hour in-memory
const RETRY_DELAY_SECS: f64 = 300.0; // 5 minutes after a failed refresh
const CONNECT_TIMEOUT_SECS: u64 = 5;
const TOTAL_TIMEOUT_SECS: u64 = 15;

// ---------------------------------------------------------------------------
// Process-wide cache state
// ---------------------------------------------------------------------------

struct CacheState {
    cache: Option<Value>,
    cache_time: f64,
    retry_after: f64,
    refresh_in_flight: bool,
}

impl CacheState {
    fn new() -> Self {
        Self {
            cache: None,
            cache_time: 0.0,
            retry_after: 0.0,
            refresh_in_flight: false,
        }
    }
}

/// Process-wide lock for tests that mutate the registry env vars
/// (`ULNCLAW_MODELS_DEV_URL` / `ULNCLAW_MODELS_DEV_CACHE`). Any test
/// touching those vars or resetting the cache must hold this.
pub fn test_env_lock() -> MutexGuard<'static, ()> {
    static TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn state() -> MutexGuard<'static, CacheState> {
    static STATE: OnceLock<Mutex<CacheState>> = OnceLock::new();
    STATE
        .get_or_init(|| Mutex::new(CacheState::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Serialize foreground/background refresh commits (hermes
/// `_models_dev_fetch_lock`).
fn fetch_lock() -> MutexGuard<'static, ()> {
    static FETCH_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    FETCH_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Clear the in-memory cache. Test hook (also used by `models refresh`
/// diagnostics); disk cache is untouched.
pub fn reset_cache_for_tests() {
    let mut st = state();
    st.cache = None;
    st.cache_time = 0.0;
    st.retry_after = 0.0;
    st.refresh_in_flight = false;
}

// ---------------------------------------------------------------------------
// Registry URL + disk cache
// ---------------------------------------------------------------------------

/// Registry URL: `ULNCLAW_MODELS_DEV_URL` override or the public default.
pub fn models_dev_url() -> String {
    std::env::var(MODELS_DEV_URL_ENV)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_MODELS_DEV_URL.to_string())
}

/// Env override for the disk cache path (defaults to
/// `$ULNCLAW_HOME/models_dev_cache.json`). Lets tests and mirrors pin the
/// cache without redirecting the whole home directory.
pub const MODELS_DEV_CACHE_ENV: &str = "ULNCLAW_MODELS_DEV_CACHE";

fn cache_path() -> PathBuf {
    std::env::var(MODELS_DEV_CACHE_ENV)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::config::ulnclaw_home().join("models_dev_cache.json"))
}

fn load_disk_cache() -> Option<Value> {
    let data = std::fs::read_to_string(cache_path()).ok()?;
    let value: Value = serde_json::from_str(&data).ok()?;
    if value.as_object().map_or(true, |o| o.is_empty()) {
        return None;
    }
    Some(value)
}

/// Age (seconds) of the disk cache file, or None if missing. Negative age
/// (clock skew) is treated as unknown freshness so callers fall through to
/// the network instead of serving potentially-bad data forever.
fn disk_cache_age_secs() -> Option<f64> {
    let meta = std::fs::metadata(cache_path()).ok()?;
    let mtime = meta.modified().ok()?;
    let age = now_secs()
        - mtime
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .ok()?;
    if age < 0.0 {
        return None;
    }
    Some(age)
}

fn save_disk_cache(data: &Value) {
    let path = cache_path();
    let Some(parent) = path.parent() else { return };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let tmp = path.with_extension("json.tmp");
    if let Ok(json) = serde_json::to_vec(data) {
        if std::fs::write(&tmp, &json).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

// ---------------------------------------------------------------------------
// Network fetch
// ---------------------------------------------------------------------------

fn is_non_empty_object(v: &Value) -> bool {
    v.as_object().map_or(false, |o| !o.is_empty())
}

/// Fetch the live models.dev registry without touching local caches.
/// Errors on network failures and on an empty/invalid registry payload.
fn fetch_from_network(url: &str) -> Result<Value, String> {
    if let Some(path) = url.strip_prefix("file://") {
        let data = std::fs::read_to_string(path).map_err(|e| format!("file read: {e}"))?;
        let value: Value = serde_json::from_str(&data).map_err(|e| format!("json parse: {e}"))?;
        if !is_non_empty_object(&value) {
            return Err("models.dev returned an empty or invalid registry".into());
        }
        return Ok(value);
    }
    fetch_with_reqwest(url)
}

/// Reqwest portion of the registry fetch. The blocking client owns an
/// internal tokio runtime; building AND dropping it on a plain OS thread
/// keeps it out of async caller contexts (gateway dispatch, `ulnclaw
/// model`), avoiding the "Cannot drop a runtime in a context where
/// blocking is not allowed" panic.
fn fetch_with_reqwest(url: &str) -> Result<Value, String> {
    let url_owned = url.to_string();
    std::thread::scope(|scope| {
        scope
            .spawn(move || {
                // Tuple-style (connect, read) timeouts from hermes: a flat
                // timeout let a blackholed connect stall the critical path.
                // 5s connect fails fast on unreachable hosts; 15s total
                // still tolerates a slow registry response.
                let client = reqwest::blocking::Client::builder()
                    .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
                    .timeout(Duration::from_secs(TOTAL_TIMEOUT_SECS))
                    .build()
                    .map_err(|e| format!("client: {e}"))?;
                let resp = client
                    .get(&url_owned)
                    .send()
                    .and_then(|r| r.error_for_status())
                    .map_err(|e| format!("fetch: {e}"))?;
                let value: Value = resp.json().map_err(|e| format!("json parse: {e}"))?;
                if !is_non_empty_object(&value) {
                    return Err("models.dev returned an empty or invalid registry".into());
                }
                Ok(value)
            })
            .join()
            .map_err(|_| "models.dev fetch thread panicked".to_string())?
    })
}

/// Give stale cache data a short in-memory grace before retrying refresh.
/// Only moves the timestamp forward so a fresh background commit is never
/// rewound (hermes `_mark_stale_cache_grace`).
fn mark_stale_cache_grace(st: &mut CacheState) {
    let grace = now_secs() - CACHE_TTL_SECS + RETRY_DELAY_SECS;
    if grace > st.cache_time {
        st.cache_time = grace;
    }
}

/// Persist a freshly fetched registry: disk + in-mem + clear backoff.
/// Callers must hold the fetch lock (hermes `_commit_registry`).
fn commit_registry(st: &mut CacheState, data: Value) {
    save_disk_cache(&data);
    let providers = data.as_object().map_or(0, |o| o.len());
    st.cache = Some(data);
    st.cache_time = now_secs();
    st.retry_after = 0.0;
    tracing::debug!("Refreshed models.dev registry: {providers} providers");
}

/// Record a failed refresh: arm the process-wide 5-minute backoff
/// (hermes `_note_refresh_failure`).
fn note_refresh_failure(st: &mut CacheState, err: &str) {
    st.retry_after = now_secs() + RETRY_DELAY_SECS;
    tracing::debug!(
        "models.dev refresh failed; retry suppressed for {}s: {err}",
        RETRY_DELAY_SECS as u64
    );
}

/// Best-effort refresh after serving stale cache data (hermes
/// `_background_refresh_models_dev` + `_start_background_refresh_models_dev`).
fn start_background_refresh() {
    let retry_after = state().retry_after;
    if now_secs() < retry_after {
        return;
    }
    {
        let mut st = state();
        if st.refresh_in_flight {
            return;
        }
        st.refresh_in_flight = true;
    }
    std::thread::Builder::new()
        .name("models-dev-refresh".into())
        .spawn(|| {
            let url = models_dev_url();
            let _guard = fetch_lock();
            let mut st = state();
            match fetch_from_network(&url) {
                Ok(data) => commit_registry(&mut st, data),
                Err(e) => note_refresh_failure(&mut st, &e),
            }
            st.refresh_in_flight = false;
        })
        .ok();
}

// ---------------------------------------------------------------------------
// Public fetch entry point
// ---------------------------------------------------------------------------

/// Fetch the models.dev registry. Cache hierarchy: in-mem → disk → network.
///
/// Returns the full registry object keyed by provider ID, or an empty object
/// on failure. Mirrors hermes `fetch_models_dev`:
///
/// - `allow_network = false`: return any memory/disk cache regardless of
///   age; never make a request (latency-sensitive paths).
/// - Fresh in-memory cache (< 1h) wins with no I/O.
/// - Stale cache is returned immediately and refreshed by a single
///   background thread; callers never block on the network while any cache
///   exists.
/// - No cache at all → singleflight foreground fetch; failures back off
///   5 minutes process-wide, then fall back to any stale disk cache.
/// - `force_refresh = true` bypasses cache fast paths and the backoff.
pub fn fetch_models_dev_opts(force_refresh: bool, allow_network: bool) -> Value {
    if !allow_network {
        let mut st = state();
        if st.cache.is_some() {
            return st.cache.clone().unwrap_or(Value::Null);
        }
        if let Some(disk) = load_disk_cache() {
            let age = disk_cache_age_secs();
            st.cache_time = age.map(|a| now_secs() - a).unwrap_or(0.0);
            st.cache = Some(disk);
            return st.cache.clone().unwrap_or(Value::Null);
        }
        return Value::Object(Default::default());
    }

    // Stage 1: fresh in-memory cache wins (hot path, no I/O).
    if !force_refresh {
        let st = state();
        if st.cache.is_some() && now_secs() - st.cache_time < CACHE_TTL_SECS {
            return st.cache.clone().unwrap_or(Value::Null);
        }
    }

    // Stage 2: stale in-memory cache beats a foreground network timeout;
    // refresh it in the background.
    if !force_refresh {
        let has_cache = {
            let mut st = state();
            if st.cache.is_some() {
                mark_stale_cache_grace(&mut st);
                true
            } else {
                false
            }
        };
        if has_cache {
            let data = state().cache.clone().unwrap_or(Value::Null);
            start_background_refresh();
            tracing::debug!("Using stale in-memory models.dev cache; refreshing in background");
            return data;
        }
    }

    // Stage 3: disk cache short-circuits the network call on cold starts.
    if !force_refresh {
        if let Some(age) = disk_cache_age_secs() {
            if let Some(disk) = load_disk_cache() {
                let fresh = age < CACHE_TTL_SECS;
                {
                    let mut st = state();
                    st.cache = Some(disk);
                    if fresh {
                        // Anchor in-mem TTL to the disk file's age so we
                        // don't extend an aging cache by another full hour.
                        st.cache_time = now_secs() - age;
                    } else {
                        mark_stale_cache_grace(&mut st);
                    }
                }
                if !fresh {
                    start_background_refresh();
                }
                let data = state().cache.clone().unwrap_or(Value::Null);
                return data;
            }
        }
    }

    // Failed automatic refreshes are process-wide; don't make every caller
    // retry the same unreachable endpoint while no usable cache exists.
    if !force_refresh && now_secs() < state().retry_after {
        return state()
            .cache
            .clone()
            .unwrap_or(Value::Object(Default::default()));
    }

    // Stage 4: singleflight foreground fetch — only reached with no cache
    // (or force_refresh). Recheck after locking: another caller may have
    // refreshed or armed backoff while we waited.
    let _guard = fetch_lock();
    {
        let st = state();
        if !force_refresh {
            if st.cache.is_some() {
                return st.cache.clone().unwrap_or(Value::Null);
            }
            if now_secs() < st.retry_after {
                return Value::Object(Default::default());
            }
        }
    }

    let url = models_dev_url();
    match fetch_from_network(&url) {
        Ok(data) => {
            let mut st = state();
            commit_registry(&mut st, data);
            st.cache.clone().unwrap_or(Value::Null)
        }
        Err(e) => {
            let mut st = state();
            note_refresh_failure(&mut st, &e);
            // Stage 5: network failed — return any stale disk cache.
            if st.cache.is_none() {
                if let Some(disk) = load_disk_cache() {
                    tracing::debug!("Loaded stale models.dev disk cache after failed refresh");
                    st.cache = Some(disk);
                    st.cache_time = 0.0;
                }
            }
            st.cache
                .clone()
                .unwrap_or(Value::Object(Default::default()))
        }
    }
}

/// Convenience wrapper matching hermes' default call (network allowed).
pub fn fetch_models_dev() -> Value {
    fetch_models_dev_opts(false, true)
}

/// Snapshot of cache freshness for status surfaces (gateway/CLI).
#[derive(Debug, Clone, Serialize)]
pub struct CacheInfo {
    pub providers: usize,
    pub age_secs: f64,
    pub fresh: bool,
}

pub fn cache_info() -> CacheInfo {
    let st = state();
    let providers = st
        .cache
        .as_ref()
        .and_then(|v| v.as_object())
        .map_or(0, |o| o.len());
    let age = if st.cache.is_some() {
        (now_secs() - st.cache_time).max(0.0)
    } else {
        0.0
    };
    CacheInfo {
        providers,
        age_secs: age,
        fresh: st.cache.is_some() && age < CACHE_TTL_SECS,
    }
}

// ---------------------------------------------------------------------------
// Typed metadata
// ---------------------------------------------------------------------------

/// Full metadata for a single model from models.dev (hermes `ModelInfo`).
#[derive(Debug, Clone, Serialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub family: String,
    /// models.dev provider ID (e.g. "anthropic")
    pub provider_id: String,
    pub reasoning: bool,
    pub tool_call: bool,
    /// supports image/file attachments (vision)
    pub attachment: bool,
    pub temperature: bool,
    pub structured_output: bool,
    pub open_weights: bool,
    pub input_modalities: Vec<String>,
    pub output_modalities: Vec<String>,
    pub context_window: u64,
    pub max_output: u64,
    pub max_input: Option<u64>,
    /// Cost per million tokens (USD)
    pub cost_input: f64,
    pub cost_output: f64,
    pub cost_cache_read: Option<f64>,
    pub cost_cache_write: Option<f64>,
    pub knowledge_cutoff: String,
    pub release_date: String,
    /// "alpha", "beta", "deprecated", or ""
    pub status: String,
}

impl ModelInfo {
    pub fn has_cost_data(&self) -> bool {
        self.cost_input > 0.0 || self.cost_output > 0.0
    }

    pub fn supports_vision(&self) -> bool {
        self.attachment || self.input_modalities.iter().any(|m| m == "image")
    }

    pub fn supports_pdf(&self) -> bool {
        self.input_modalities.iter().any(|m| m == "pdf")
    }

    pub fn supports_audio_input(&self) -> bool {
        self.input_modalities.iter().any(|m| m == "audio")
    }

    /// Human-readable cost string, e.g. `$3.00/M in, $15.00/M out`.
    pub fn format_cost(&self) -> String {
        if !self.has_cost_data() {
            return "unknown".into();
        }
        let mut parts = vec![
            format!("${:.2}/M in", self.cost_input),
            format!("${:.2}/M out", self.cost_output),
        ];
        if let Some(read) = self.cost_cache_read {
            parts.push(format!("cache read ${read:.2}/M"));
        }
        parts.join(", ")
    }

    /// Human-readable capabilities, e.g. `reasoning, tools, vision, PDF`.
    pub fn format_capabilities(&self) -> String {
        let mut caps = Vec::new();
        if self.reasoning {
            caps.push("reasoning");
        }
        if self.tool_call {
            caps.push("tools");
        }
        if self.supports_vision() {
            caps.push("vision");
        }
        if self.supports_pdf() {
            caps.push("PDF");
        }
        if self.supports_audio_input() {
            caps.push("audio");
        }
        if self.structured_output {
            caps.push("structured output");
        }
        if self.open_weights {
            caps.push("open weights");
        }
        if caps.is_empty() {
            "basic".into()
        } else {
            caps.join(", ")
        }
    }
}

/// Full metadata for a provider from models.dev (hermes `ProviderInfo`).
#[derive(Debug, Clone, Serialize)]
pub struct ProviderInfo {
    /// models.dev provider ID
    pub id: String,
    pub name: String,
    /// env var names for the API key
    pub env: Vec<String>,
    /// base URL
    pub api: String,
    /// documentation URL
    pub doc: String,
    pub model_count: usize,
}

/// Structured capability metadata for a model (hermes `ModelCapabilities`).
#[derive(Debug, Clone, Serialize)]
pub struct ModelCapabilities {
    pub supports_tools: bool,
    pub supports_vision: bool,
    pub supports_reasoning: bool,
    pub context_window: u64,
    pub max_output_tokens: u64,
    pub model_family: String,
}

// ---------------------------------------------------------------------------
// Provider ID mapping: local provider names ↔ models.dev IDs (hermes
// PROVIDER_TO_MODELS_DEV)
// ---------------------------------------------------------------------------

pub fn provider_to_models_dev(provider: &str) -> Option<&'static str> {
    match provider {
        "openrouter" => Some("openrouter"),
        "novita" => Some("novita-ai"),
        "anthropic" => Some("anthropic"),
        "openai" => Some("openai"),
        "openai-codex" => Some("openai"),
        "zai" => Some("zai"),
        "kimi" => Some("kimi-for-coding"),
        "kimi-coding" => Some("kimi-for-coding"),
        "moonshot" => Some("kimi-for-coding"),
        "stepfun" => Some("stepfun"),
        "kimi-coding-cn" => Some("kimi-for-coding"),
        "minimax" => Some("minimax"),
        "minimax-oauth" => Some("minimax"),
        "minimax-cn" => Some("minimax-cn"),
        "deepseek" => Some("deepseek"),
        "alibaba" => Some("alibaba"),
        "dashscope" => Some("alibaba"),
        "qwen-oauth" => Some("alibaba"),
        "copilot" => Some("github-copilot"),
        "ai-gateway" => Some("vercel"),
        "opencode-zen" => Some("opencode"),
        "opencode-go" => Some("opencode-go"),
        "kilocode" => Some("kilo"),
        "fireworks" => Some("fireworks-ai"),
        "huggingface" => Some("huggingface"),
        "gemini" => Some("google"),
        "google" => Some("google"),
        "xai" => Some("xai"),
        "xai-oauth" => Some("xai"),
        "xiaomi" => Some("xiaomi"),
        "nvidia" => Some("nvidia"),
        "groq" => Some("groq"),
        "mistral" => Some("mistral"),
        "togetherai" => Some("togetherai"),
        "perplexity" => Some("perplexity"),
        "cohere" => Some("cohere"),
        "ollama-cloud" => Some("ollama-cloud"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Raw-entry helpers
// ---------------------------------------------------------------------------

fn provider_models<'a>(
    data: &'a Value,
    mdev_id: &str,
) -> Option<&'a serde_json::Map<String, Value>> {
    data.get(mdev_id)?.get("models")?.as_object()
}

fn find_model_entry<'a>(
    models: &'a serde_json::Map<String, Value>,
    model: &str,
) -> Option<(&'a String, &'a Value)> {
    if let Some(entry) = models.get(model) {
        if entry.is_object() {
            return Some((models.keys().find(|k| *k == model).unwrap(), entry));
        }
    }
    let lower = model.to_lowercase();
    models
        .iter()
        .find(|(id, v)| id.to_lowercase() == lower && v.is_object())
}

fn positive_int(v: Option<&Value>) -> Option<u64> {
    let n = v?.as_f64()?;
    if n > 0.0 {
        Some(n as u64)
    } else {
        None
    }
}

fn extract_context(entry: &Value) -> Option<u64> {
    positive_int(entry.get("limit").and_then(|l| l.get("context")))
}

// ---------------------------------------------------------------------------
// Queries
// ---------------------------------------------------------------------------

/// Resolve a local provider name to its models.dev models map.
/// Mapped names resolve through `provider_to_models_dev`; unmapped names
/// fall back to an identity lookup so provider names that already match a
/// models.dev slug (custom/local catalogs included) still resolve.
fn get_provider_models(provider: &str) -> Option<(String, Value)> {
    let mdev_id = provider_to_models_dev(provider)
        .map(str::to_string)
        .unwrap_or_else(|| provider.to_string());
    let data = fetch_models_dev();
    let models = provider_models(&data, &mdev_id)?.clone();
    Some((mdev_id, Value::Object(models)))
}

/// Look up context_length for a provider+model combo in models.dev.
///
/// Returns the context window in tokens, or None if not found. Handles
/// case-insensitive matching, filters out context=0 entries, and tries
/// `:cloud` / `-cloud` suffixed keys (some providers store suffixed IDs in
/// models.dev while live APIs return bare names).
pub fn lookup_models_dev_context(provider: &str, model: &str) -> Option<u64> {
    let mdev_id = provider_to_models_dev(provider)
        .map(str::to_string)
        .unwrap_or_else(|| provider.to_string());
    let data = fetch_models_dev();
    let models = provider_models(&data, &mdev_id)?;

    if let Some(entry) = models.get(model) {
        if let Some(ctx) = extract_context(entry) {
            return Some(ctx);
        }
    }
    let lower = model.to_lowercase();
    for (id, entry) in models {
        if id.to_lowercase() == lower {
            if let Some(ctx) = extract_context(entry) {
                return Some(ctx);
            }
        }
    }
    for suffix in [":cloud", "-cloud"] {
        let suffixed = format!("{model}{suffix}");
        if let Some(entry) = models.get(&suffixed) {
            if let Some(ctx) = extract_context(entry) {
                return Some(ctx);
            }
        }
        let suffixed_lower = format!("{lower}{suffix}");
        for (id, entry) in models {
            if id.to_lowercase() == suffixed_lower {
                if let Some(ctx) = extract_context(entry) {
                    return Some(ctx);
                }
            }
        }
    }
    None
}

/// Look up full capability metadata from the models.dev cache (hermes
/// `get_model_capabilities`). Returns None if the model is not found.
pub fn get_model_capabilities(provider: &str, model: &str) -> Option<ModelCapabilities> {
    let mdev_id = provider_to_models_dev(provider)
        .map(str::to_string)
        .unwrap_or_else(|| provider.to_string());
    let data = fetch_models_dev();
    let models = provider_models(&data, &mdev_id)?;
    let (_, entry) = find_model_entry(models, model)?;

    let supports_tools = entry
        .get("tool_call")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    // Vision: prefer explicit modalities.input; the older `attachment` flag
    // can be stale or too broad for image routing.
    let input_mods = entry
        .get("modalities")
        .and_then(|m| m.get("input"))
        .and_then(|m| m.as_array());
    let supports_vision = match input_mods {
        Some(mods) => mods.iter().any(|m| m.as_str() == Some("image")),
        None => entry
            .get("attachment")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    };
    let supports_reasoning = entry
        .get("reasoning")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let limit = entry.get("limit");
    let context_window = positive_int(limit.and_then(|l| l.get("context"))).unwrap_or(200_000);
    let max_output_tokens = positive_int(limit.and_then(|l| l.get("output"))).unwrap_or(8192);
    let model_family = entry
        .get("family")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    Some(ModelCapabilities {
        supports_tools,
        supports_vision,
        supports_reasoning,
        context_window,
        max_output_tokens,
        model_family,
    })
}

// Patterns that indicate non-agentic or noise models (TTS, embedding,
// dated preview snapshots, live/streaming-only, image-only).
fn noise_patterns() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)-tts\b|embedding|live-|-(preview|exp)-\d{2,4}[-_]|-image\b|-image-preview\b|-customtools\b",
        )
        .expect("static regex compiles")
    })
}

/// Google's live Gemini catalogs include stale slugs and low-TPM Gemma
/// models; keep their metadata queryable but hide them from catalogs.
fn google_hidden_models() -> &'static [&'static str] {
    &[
        "gemma-4-31b-it",
        "gemma-4-26b-it",
        "gemma-4-26b-a4b-it",
        "gemma-3-1b",
        "gemma-3-1b-it",
        "gemma-3-2b",
        "gemma-3-2b-it",
        "gemma-3-4b",
        "gemma-3-4b-it",
        "gemma-3-12b",
        "gemma-3-12b-it",
        "gemma-3-27b",
        "gemma-3-27b-it",
        "gemini-1.5-flash",
        "gemini-1.5-pro",
        "gemini-1.5-flash-8b",
        "gemini-2.0-flash",
        "gemini-2.0-flash-lite",
    ]
}

fn should_hide_from_provider_catalog(provider: &str, model_id: &str) -> bool {
    let provider_lower = provider.trim().to_lowercase();
    let model_lower = model_id.trim().to_lowercase();
    if matches!(provider_lower.as_str(), "gemini" | "google") {
        return google_hidden_models().contains(&model_lower.as_str());
    }
    false
}

/// All model IDs for a provider from models.dev (hidden noise excluded).
pub fn list_provider_models(provider: &str) -> Vec<String> {
    let Some((_, models)) = get_provider_models(provider.trim()) else {
        return Vec::new();
    };
    let Some(models) = models.as_object() else {
        return Vec::new();
    };
    models
        .keys()
        .filter(|id| !should_hide_from_provider_catalog(provider, id))
        .cloned()
        .collect()
}

/// Model IDs suitable for agentic use (hermes `list_agentic_models`):
/// tool_call=true, minus hidden and noise models.
pub fn list_agentic_models(provider: &str) -> Vec<String> {
    let Some((_, models)) = get_provider_models(provider.trim()) else {
        return Vec::new();
    };
    let Some(models) = models.as_object() else {
        return Vec::new();
    };
    let mut result = Vec::new();
    for (id, entry) in models {
        if !entry.is_object() {
            continue;
        }
        if should_hide_from_provider_catalog(provider, id) {
            continue;
        }
        if !entry
            .get("tool_call")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            continue;
        }
        if noise_patterns().is_match(id) {
            continue;
        }
        result.push(id.clone());
    }
    result
}

/// Full provider metadata (hermes `get_provider_info`). Accepts either a
/// local provider name or a models.dev ID.
pub fn get_provider_info(provider_id: &str, allow_network: bool) -> Option<ProviderInfo> {
    let mdev_id = provider_to_models_dev(provider_id)
        .map(|s| s.to_string())
        .unwrap_or_else(|| provider_id.to_string());
    let data = fetch_models_dev_opts(false, allow_network);
    let raw = data.get(&mdev_id)?;
    if !raw.is_object() {
        return None;
    }
    Some(parse_provider_info(&mdev_id, raw))
}

/// Full model metadata (hermes `get_model_info`). Accepts local or
/// models.dev provider IDs; exact match then case-insensitive fallback.
pub fn get_model_info(provider_id: &str, model_id: &str) -> Option<ModelInfo> {
    let mdev_id = provider_to_models_dev(provider_id)
        .map(|s| s.to_string())
        .unwrap_or_else(|| provider_id.to_string());
    let data = fetch_models_dev();
    let models = provider_models(&data, &mdev_id)?;
    let (id, raw) = find_model_entry(models, model_id)?;
    Some(parse_model_info(id, raw, &mdev_id))
}

/// Every provider in the catalog (for `ulnclaw models providers`).
pub fn list_providers(allow_network: bool) -> Vec<ProviderInfo> {
    let data = fetch_models_dev_opts(false, allow_network);
    let Some(obj) = data.as_object() else {
        return Vec::new();
    };
    obj.iter()
        .filter(|(_, v)| v.is_object())
        .map(|(id, raw)| parse_provider_info(id, raw))
        .collect()
}

// ---------------------------------------------------------------------------
// Raw JSON → dataclasses (hermes `_parse_model_info` / `_parse_provider_info`)
// ---------------------------------------------------------------------------

fn string_list(v: Option<&Value>) -> Vec<String> {
    v.and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|s| s.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

pub fn parse_model_info(model_id: &str, raw: &Value, provider_id: &str) -> ModelInfo {
    let limit = raw.get("limit").filter(|v| v.is_object());
    let cost = raw.get("cost").filter(|v| v.is_object());
    let modalities = raw.get("modalities").filter(|v| v.is_object());

    let cost_f64 = |key: &str| -> Option<f64> {
        let c = cost?;
        if c.get(key).map_or(true, |v| v.is_null()) {
            return None;
        }
        c.get(key)?.as_f64()
    };

    ModelInfo {
        id: model_id.to_string(),
        name: raw
            .get("name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(model_id)
            .to_string(),
        family: raw
            .get("family")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        provider_id: provider_id.to_string(),
        reasoning: raw
            .get("reasoning")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        tool_call: raw
            .get("tool_call")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        attachment: raw
            .get("attachment")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        temperature: raw
            .get("temperature")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        structured_output: raw
            .get("structured_output")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        open_weights: raw
            .get("open_weights")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        input_modalities: string_list(modalities.and_then(|m| m.get("input"))),
        output_modalities: string_list(modalities.and_then(|m| m.get("output"))),
        context_window: positive_int(limit.and_then(|l| l.get("context"))).unwrap_or(0),
        max_output: positive_int(limit.and_then(|l| l.get("output"))).unwrap_or(0),
        max_input: positive_int(limit.and_then(|l| l.get("input"))),
        cost_input: cost
            .and_then(|c| c.get("input"))
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0),
        cost_output: cost
            .and_then(|c| c.get("output"))
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0),
        cost_cache_read: cost_f64("cache_read"),
        cost_cache_write: cost_f64("cache_write"),
        knowledge_cutoff: raw
            .get("knowledge")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        release_date: raw
            .get("release_date")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        status: raw
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    }
}

pub fn parse_provider_info(provider_id: &str, raw: &Value) -> ProviderInfo {
    let model_count = raw
        .get("models")
        .and_then(|m| m.as_object())
        .map_or(0, |o| o.len());
    ProviderInfo {
        id: provider_id.to_string(),
        name: raw
            .get("name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(provider_id)
            .to_string(),
        env: string_list(raw.get("env")),
        api: raw
            .get("api")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        doc: raw
            .get("doc")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        model_count,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> Value {
        json!({
            "openai": {
                "name": "OpenAI",
                "env": ["OPENAI_API_KEY"],
                "api": "https://api.openai.com/v1",
                "doc": "https://platform.openai.com/docs",
                "models": {
                    "gpt-5": {
                        "name": "GPT-5",
                        "family": "gpt-5",
                        "reasoning": true,
                        "tool_call": true,
                        "attachment": true,
                        "temperature": true,
                        "structured_output": true,
                        "modalities": {"input": ["text", "image"], "output": ["text"]},
                        "limit": {"context": 400000, "output": 128000},
                        "cost": {"input": 1.25, "output": 10.0, "cache_read": 0.125},
                        "knowledge": "2025-05",
                        "release_date": "2025-08-07",
                        "status": ""
                    },
                    "GPT-5-Mini": {
                        "tool_call": true,
                        "limit": {"context": 200000, "output": 64000},
                        "cost": {"input": 0.25, "output": 2.0}
                    },
                    "gpt-5-tts": {"tool_call": true},
                    "gpt-embedding-2": {"tool_call": true},
                    "gpt-live-preview": {"tool_call": true},
                    "gpt-5-image": {"tool_call": true},
                    "gpt-preview-2401-x": {"tool_call": true},
                    "no-tools-model": {"tool_call": false},
                    "zero-context-audio": {"limit": {"context": 0}}
                }
            },
            "ollama-cloud": {
                "name": "Ollama Cloud",
                "env": ["OLLAMA_CLOUD_API_KEY"],
                "api": "https://ollama.com/cloud",
                "models": {
                    "kimi-k2.6:cloud": {
                        "tool_call": true,
                        "limit": {"context": 262144, "output": 32768}
                    }
                }
            },
            "google": {
                "name": "Google Gemini API",
                "env": ["GEMINI_API_KEY"],
                "api": "https://generativelanguage.googleapis.com/v1beta",
                "models": {
                    "gemini-3-pro": {"tool_call": true, "limit": {"context": 1048576}},
                    "gemini-2.0-flash": {"tool_call": true},
                    "gemma-3-4b-it": {"tool_call": true}
                }
            },
            "acme-custom": {
                "name": "Acme",
                "api": "https://acme.example/v1",
                "models": {
                    "acme-chat": {"tool_call": true, "limit": {"context": 32768}}
                }
            }
        })
    }

    #[test]
    fn provider_mapping() {
        assert_eq!(provider_to_models_dev("openai"), Some("openai"));
        assert_eq!(provider_to_models_dev("kilocode"), Some("kilo"));
        assert_eq!(provider_to_models_dev("gemini"), Some("google"));
        assert_eq!(provider_to_models_dev("xai-oauth"), Some("xai"));
        assert_eq!(provider_to_models_dev("my-local-llm"), None);
    }

    #[test]
    fn noise_and_hide_filters() {
        let re = noise_patterns();
        for noisy in [
            "gpt-5-tts",
            "gpt-5-tts-hd",
            "text-embedding-3-large",
            "gpt-live-preview",
            "gpt-5-image",
            "gpt-image-preview-x",
            "model-customtools",
            "claude-exp-2504-x",
            "gemini-preview-2401_",
        ] {
            assert!(re.is_match(noisy), "should be noise: {noisy}");
        }
        for clean in ["gpt-5", "gpt-5-mini", "claude-4-opus", "imagegen-pro"] {
            assert!(!re.is_match(clean), "should not be noise: {clean}");
        }
        assert!(should_hide_from_provider_catalog(
            "google",
            "gemini-2.0-flash"
        ));
        assert!(should_hide_from_provider_catalog("Gemini", "gemma-3-4b-it"));
        assert!(!should_hide_from_provider_catalog("google", "gemini-3-pro"));
        assert!(!should_hide_from_provider_catalog(
            "openai",
            "gemini-2.0-flash"
        ));
    }

    #[test]
    fn parse_model_info_shapes() {
        let raw = json!({
            "name": "GPT-5",
            "family": "gpt-5",
            "reasoning": true,
            "tool_call": true,
            "attachment": false,
            "structured_output": true,
            "modalities": {"input": ["text", "image", "pdf"], "output": ["text"]},
            "limit": {"context": 400000, "output": 128000, "input": 272000},
            "cost": {"input": 1.25, "output": 10.0, "cache_read": 0.125, "cache_write": 1.25},
            "knowledge": "2025-05",
            "release_date": "2025-08-07",
            "status": "beta"
        });
        let info = parse_model_info("gpt-5", &raw, "openai");
        assert_eq!(info.name, "GPT-5");
        assert_eq!(info.context_window, 400000);
        assert_eq!(info.max_output, 128000);
        assert_eq!(info.max_input, Some(272000));
        assert!(info.supports_vision());
        assert!(info.supports_pdf());
        assert!(!info.supports_audio_input());
        assert_eq!(
            info.format_cost(),
            "$1.25/M in, $10.00/M out, cache read $0.12/M"
        );
        assert_eq!(
            info.format_capabilities(),
            "reasoning, tools, vision, PDF, structured output"
        );
        assert_eq!(info.status, "beta");

        let bare = parse_model_info("mystery", &json!({}), "acme");
        assert_eq!(bare.name, "mystery");
        assert_eq!(bare.format_cost(), "unknown");
        assert_eq!(bare.format_capabilities(), "basic");
    }

    #[test]
    fn parse_provider_info_defaults() {
        let info = parse_provider_info("acme", &json!({"models": {"a": {}, "b": {}}}));
        assert_eq!(info.name, "acme");
        assert_eq!(info.model_count, 2);
        assert!(info.env.is_empty());
    }

    fn with_catalog_env<F: FnOnce()>(dir: &std::path::Path, body: F) {
        let fixture_path = dir.join("api.json");
        std::fs::write(&fixture_path, fixture().to_string()).unwrap();
        std::env::set_var(
            MODELS_DEV_URL_ENV,
            format!("file://{}", fixture_path.display()),
        );
        std::env::set_var(
            MODELS_DEV_CACHE_ENV,
            dir.join("models_dev_cache.json").display().to_string(),
        );
        reset_cache_for_tests();
        body();
    }

    #[test]
    fn catalog_pipeline_and_queries() {
        let _guard = test_env_lock();
        let dir = tempfile::tempdir().unwrap();
        with_catalog_env(dir.path(), || {
            // Cold foreground fetch from the file:// mirror.
            let data = fetch_models_dev();
            assert_eq!(data.as_object().unwrap().len(), 4);
            assert!(cache_path().exists(), "disk cache must be written");
            assert!(cache_info().fresh);

            // Context lookups: exact, case-insensitive, zero filtered, suffix.
            assert_eq!(lookup_models_dev_context("openai", "gpt-5"), Some(400000));
            assert_eq!(
                lookup_models_dev_context("openai", "gpt-5-mini"),
                Some(200000)
            );
            assert_eq!(
                lookup_models_dev_context("openai", "zero-context-audio"),
                None
            );
            assert_eq!(
                lookup_models_dev_context("ollama-cloud", "kimi-k2.6"),
                Some(262144)
            );
            assert_eq!(lookup_models_dev_context("unknown-provider", "x"), None);

            // Capabilities.
            let caps = get_model_capabilities("openai", "gpt-5").unwrap();
            assert!(caps.supports_tools && caps.supports_vision && caps.supports_reasoning);
            assert_eq!(caps.context_window, 400000);
            assert_eq!(caps.max_output_tokens, 128000);
            assert_eq!(caps.model_family, "gpt-5");
            let defaults = get_model_capabilities("openai", "no-tools-model").unwrap();
            assert!(!defaults.supports_tools);
            assert_eq!(defaults.context_window, 200000);
            assert_eq!(defaults.max_output_tokens, 8192);
            assert!(get_model_capabilities("openai", "missing").is_none());

            // Agentic catalog: noise + non-tool models excluded.
            let agentic = list_agentic_models("openai");
            assert!(agentic.contains(&"gpt-5".to_string()));
            assert!(agentic.contains(&"GPT-5-Mini".to_string()));
            assert!(!agentic.contains(&"no-tools-model".to_string()));
            for noisy in [
                "gpt-5-tts",
                "gpt-embedding-2",
                "gpt-live-preview",
                "gpt-5-image",
                "gpt-preview-2401-x",
            ] {
                assert!(!agentic.contains(&noisy.to_string()), "{noisy} leaked");
            }

            // Google hidden models stay out of provider catalogs.
            let goog = list_provider_models("google");
            assert!(goog.contains(&"gemini-3-pro".to_string()));
            assert!(!goog.contains(&"gemini-2.0-flash".to_string()));
            assert!(!goog.contains(&"gemma-3-4b-it".to_string()));
            assert!(
                list_agentic_models("gemini").is_empty()
                    || !list_agentic_models("gemini").contains(&"gemini-2.0-flash".to_string())
            );

            // Provider info: mapped name + unmapped identity fallback.
            let openai_info = get_provider_info("openai", false).unwrap();
            assert_eq!(openai_info.name, "OpenAI");
            assert_eq!(openai_info.env, vec!["OPENAI_API_KEY".to_string()]);
            assert_eq!(openai_info.model_count, 9);
            let acme = get_provider_info("acme-custom", false).unwrap();
            assert_eq!(acme.id, "acme-custom");
            assert!(get_provider_info("not-a-provider", false).is_none());

            // Model info: cost/capability formatting + case-insensitive match
            // through an unmapped provider ID.
            let info = get_model_info("openai", "gpt-5").unwrap();
            assert_eq!(
                info.format_cost(),
                "$1.25/M in, $10.00/M out, cache read $0.12/M"
            );
            assert!(info.format_capabilities().contains("reasoning"));
            let ci = get_model_info("acme-custom", "ACME-CHAT").unwrap();
            assert_eq!(ci.id, "acme-chat");
            assert!(get_model_info("openai", "missing").is_none());

            assert_eq!(list_providers(false).len(), 4);

            // Memory reset → allow_network=false reloads the disk cache.
            reset_cache_for_tests();
            let offline = fetch_models_dev_opts(false, false);
            assert_eq!(offline.as_object().unwrap().len(), 4);
            assert_eq!(cache_info().providers, 4);

            // force_refresh re-reads the source even with a fresh cache.
            let forced = fetch_models_dev_opts(true, true);
            assert_eq!(forced.as_object().unwrap().len(), 4);
        });
    }

    #[test]
    fn failure_backoff_and_recovery() {
        let _guard = test_env_lock();
        let dir = tempfile::tempdir().unwrap();
        with_catalog_env(dir.path(), || {
            // Prime a good cache first, then point the URL at a missing file.
            assert_eq!(fetch_models_dev().as_object().unwrap().len(), 4);
            reset_cache_for_tests();
            std::fs::remove_file(cache_path()).ok();
            std::env::set_var(
                MODELS_DEV_URL_ENV,
                format!("file://{}/missing.json", dir.path().display()),
            );

            // Foreground fetch fails → empty registry + 5-minute backoff.
            let empty = fetch_models_dev();
            assert!(empty.as_object().unwrap().is_empty());

            // Backoff: even a fixed URL is not retried without force.
            let fixture_path = dir.path().join("api.json");
            std::env::set_var(
                MODELS_DEV_URL_ENV,
                format!("file://{}", fixture_path.display()),
            );
            let still_empty = fetch_models_dev();
            assert!(still_empty.as_object().unwrap().is_empty());

            // allow_network=false never fetches, even outside backoff.
            assert!(fetch_models_dev_opts(false, false)
                .as_object()
                .unwrap()
                .is_empty());

            // force_refresh bypasses the backoff and recovers.
            let recovered = fetch_models_dev_opts(true, true);
            assert_eq!(recovered.as_object().unwrap().len(), 4);
            assert!(cache_path().exists());
        });
    }

    #[test]
    fn force_refresh_failure_falls_back_to_disk_cache() {
        let _guard = test_env_lock();
        let dir = tempfile::tempdir().unwrap();
        with_catalog_env(dir.path(), || {
            // Establish the disk cache from the good mirror.
            assert_eq!(fetch_models_dev().as_object().unwrap().len(), 4);
            // Wipe memory and break the URL. A forced refresh skips the
            // cache fast paths, fails on the network, and falls back to the
            // stale disk cache (hermes stage 5).
            reset_cache_for_tests();
            std::env::set_var(
                MODELS_DEV_URL_ENV,
                format!("file://{}/missing.json", dir.path().display()),
            );
            let data = fetch_models_dev_opts(true, true);
            assert_eq!(data.as_object().unwrap().len(), 4);
            assert!(
                cache_path().exists(),
                "fallback must not destroy the disk cache"
            );
        });
    }
}

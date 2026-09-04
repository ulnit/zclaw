//! FAL + DeepInfra video generation backends — ports of hermes
//! `plugins/video_gen/fal/__init__.py` and
//! `plugins/video_gen/deepinfra/__init__.py` (v2026.8.3).
//!
//! FAL: multi-family queue backend (text/image routing per family,
//! capability-driven payload construction, direct `FAL_KEY` credentials or
//! the Nous managed `fal-queue` gateway).
//!
//! DeepInfra: OpenAI-compatible `/videos` async jobs (create → poll →
//! download), the hermes `OpenAICompatibleVideoGenProvider` shape.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::video_gen::{error_response, success_response, VideoGenParams, VideoGenProvider};

// ===========================================================================
// FAL (hermes plugins/video_gen/fal)
// ===========================================================================

/// One FAL model family (hermes `FAL_FAMILIES` entry).
struct FalFamily {
    /// Family label used by the hermes `list_models` picker surface.
    #[allow(dead_code)]
    display: &'static str,
    text_endpoint: &'static str,
    image_endpoint: &'static str,
    aspect_ratios: Option<&'static [&'static str]>,
    resolutions: Option<&'static [&'static str]>,
    /// Either an enum of durations or a `(min, max)` range (gap > 1).
    durations: FalDurations,
    duration_suffix: &'static str,
    audio: bool,
    negative: bool,
    image_param_key: &'static str,
}

#[derive(Clone, Copy)]
enum FalDurations {
    None,
    Enum(&'static [i64]),
    Range(i64, i64),
}

fn fal_families() -> Vec<(&'static str, FalFamily)> {
    vec![
        (
            "ltx-2.3",
            FalFamily {
                display: "LTX 2.3 (22B)",
                text_endpoint: "fal-ai/ltx-2.3-22b/text-to-video",
                image_endpoint: "fal-ai/ltx-2.3-22b/image-to-video",
                aspect_ratios: None,
                resolutions: None,
                durations: FalDurations::None,
                duration_suffix: "",
                audio: true,
                negative: true,
                image_param_key: "image_url",
            },
        ),
        (
            "pixverse-v6",
            FalFamily {
                display: "Pixverse v6",
                text_endpoint: "fal-ai/pixverse/v6/text-to-video",
                image_endpoint: "fal-ai/pixverse/v6/image-to-video",
                aspect_ratios: None,
                resolutions: Some(&["360p", "540p", "720p", "1080p"]),
                durations: FalDurations::Range(1, 15),
                duration_suffix: "",
                audio: true,
                negative: true,
                image_param_key: "image_url",
            },
        ),
        (
            "veo3.1",
            FalFamily {
                display: "Veo 3.1",
                text_endpoint: "fal-ai/veo3.1",
                image_endpoint: "fal-ai/veo3.1/image-to-video",
                aspect_ratios: Some(&["16:9", "9:16"]),
                resolutions: Some(&["720p", "1080p", "4k"]),
                durations: FalDurations::Enum(&[4, 6, 8]),
                duration_suffix: "s",
                audio: true,
                negative: true,
                image_param_key: "image_url",
            },
        ),
        (
            "seedance-2.0",
            FalFamily {
                display: "Seedance 2.0",
                text_endpoint: "bytedance/seedance-2.0/text-to-video",
                image_endpoint: "bytedance/seedance-2.0/image-to-video",
                aspect_ratios: Some(&["21:9", "16:9", "4:3", "1:1", "3:4", "9:16"]),
                resolutions: Some(&["480p", "720p", "1080p"]),
                durations: FalDurations::Range(4, 15),
                duration_suffix: "",
                audio: true,
                negative: false,
                image_param_key: "image_url",
            },
        ),
        (
            "kling-v3-4k",
            FalFamily {
                display: "Kling v3 4K",
                text_endpoint: "fal-ai/kling-video/v3/4k/text-to-video",
                image_endpoint: "fal-ai/kling-video/v3/4k/image-to-video",
                aspect_ratios: Some(&["16:9", "9:16", "1:1"]),
                resolutions: None,
                durations: FalDurations::Range(3, 15),
                duration_suffix: "",
                audio: true,
                negative: true,
                image_param_key: "start_image_url",
            },
        ),
        (
            "happy-horse",
            FalFamily {
                display: "Happy Horse 1.0",
                text_endpoint: "alibaba/happy-horse/text-to-video",
                image_endpoint: "alibaba/happy-horse/image-to-video",
                aspect_ratios: None,
                resolutions: None,
                durations: FalDurations::None,
                duration_suffix: "",
                audio: false,
                negative: false,
                image_param_key: "image_url",
            },
        ),
    ]
}

const FAL_DEFAULT_MODEL: &str = "pixverse-v6";
const FAL_QUEUE_ORIGIN: &str = "https://queue.fal.run";
const FAL_POLL_INTERVAL_SECONDS: u64 = 5;
const FAL_POLL_DEADLINE_SECONDS: u64 = 900;

fn find_family(id: &str) -> Option<FalFamily> {
    fal_families()
        .into_iter()
        .find(|(fid, _)| *fid == id)
        .map(|(_, family)| family)
}

/// Decide which FAL family to use (hermes `_resolve_family`): explicit arg
/// → `FAL_VIDEO_MODEL` env → `video_gen.fal.model` → `video_gen.model` →
/// default.
fn resolve_fal_family(
    explicit: Option<&str>,
    config: &crate::config::UlncLawConfig,
) -> (String, FalFamily) {
    let mut candidates: Vec<Option<String>> = vec![
        explicit
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        crate::config::get_env_value("FAL_VIDEO_MODEL")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        config
            .video_gen
            .fal
            .model
            .as_deref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        config
            .video_gen
            .model
            .as_deref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
    ];
    // Dedup-free scan: first candidate naming a known family wins.
    for candidate in candidates.drain(..) {
        if let Some(name) = candidate {
            if let Some(family) = find_family(&name) {
                return (name, family);
            }
        }
    }
    (
        FAL_DEFAULT_MODEL.to_string(),
        find_family(FAL_DEFAULT_MODEL).expect("default family exists"),
    )
}

/// Clamp/resolve duration per family (hermes `_clamp_duration`).
fn fal_clamp_duration(family: &FalFamily, duration: Option<i64>) -> Option<i64> {
    match family.durations {
        FalDurations::None => duration,
        FalDurations::Range(lo, hi) => match duration {
            // Range families omit the field when unset so the endpoint
            // applies its own default.
            None => None,
            Some(value) => Some(value.clamp(lo, hi)),
        },
        FalDurations::Enum(values) => match duration {
            // Enum families keep sending their first entry as the default.
            None => values.first().copied(),
            Some(value) => {
                if values.contains(&value) {
                    Some(value)
                } else {
                    values
                        .iter()
                        .copied()
                        .min_by_key(|allowed| (allowed - value).abs())
                }
            }
        },
    }
}

/// Build a family-specific payload, dropping keys the family doesn't
/// declare (hermes `_build_payload`).
fn fal_build_payload(
    family: &FalFamily,
    prompt: &str,
    image_url: Option<&str>,
    duration: Option<i64>,
    aspect_ratio: &str,
    resolution: &str,
    negative_prompt: Option<&str>,
    audio: Option<bool>,
    seed: Option<i64>,
) -> Value {
    let mut payload = json!({});
    let obj = payload.as_object_mut().expect("object");

    if !prompt.is_empty() {
        obj.insert("prompt".to_string(), json!(prompt));
    }
    if let Some(image_url) = image_url {
        obj.insert(family.image_param_key.to_string(), json!(image_url));
    }
    if let Some(seed) = seed {
        obj.insert("seed".to_string(), json!(seed));
    }
    if let Some(ratios) = family.aspect_ratios {
        if ratios.contains(&aspect_ratio) {
            obj.insert("aspect_ratio".to_string(), json!(aspect_ratio));
        }
    }
    if let Some(resolutions) = family.resolutions {
        if resolutions.contains(&resolution) {
            obj.insert("resolution".to_string(), json!(resolution));
        }
    }
    let clamped = fal_clamp_duration(family, duration);
    if let Some(clamped) = clamped {
        if !matches!(family.durations, FalDurations::None) {
            // FAL exposes duration as a string in the queue API ("8" not
            // 8); some families need a unit suffix ("4s").
            obj.insert(
                "duration".to_string(),
                json!(format!("{}{}", clamped, family.duration_suffix)),
            );
        }
    }
    if family.audio {
        if let Some(audio) = audio {
            obj.insert("generate_audio".to_string(), json!(audio));
        }
    }
    if family.negative {
        if let Some(negative_prompt) = negative_prompt {
            if !negative_prompt.is_empty() {
                obj.insert("negative_prompt".to_string(), json!(negative_prompt));
            }
        }
    }
    payload
}

/// FAL credentials: direct `FAL_KEY` preferred; otherwise the Nous managed
/// `fal-queue` gateway when a Nous token is present (hermes
/// `_resolve_managed_fal_video_gateway` — the prefers-gateway knob is not
/// ported).
enum FalRoute {
    Direct(String),
    Managed(String),
}

fn fal_route() -> Option<FalRoute> {
    if let Some(key) = crate::config::get_env_value("FAL_KEY") {
        let key = key.trim().to_string();
        if !key.is_empty() {
            return Some(FalRoute::Direct(key));
        }
    }
    if crate::managed_gateway::managed_nous_tools_enabled() {
        if let Ok(origin) = crate::managed_gateway::build_vendor_gateway_url("fal-queue") {
            return Some(FalRoute::Managed(origin));
        }
    }
    None
}

/// Submit + wait for a FAL queue job (hermes `_submit_fal_video_request` +
/// `handle.get()`; the fal_client SDK's queue REST protocol).
async fn fal_submit_and_wait(endpoint: &str, arguments: &Value) -> Result<Value, String> {
    let Some(route) = fal_route() else {
        return Err(
            "No FAL backend available. Either set FAL_KEY or sign in to Nous for managed \
             gateway access."
                .to_string(),
        );
    };

    let (origin, auth_header, auth_value) = match &route {
        FalRoute::Direct(key) => (
            FAL_QUEUE_ORIGIN.to_string(),
            "Authorization",
            format!("Key {}", key),
        ),
        FalRoute::Managed(origin) => {
            let bearer = crate::managed_gateway::read_nous_access_token().ok_or_else(|| {
                "Nous managed FAL gateway requires a sign-in (no token available)".to_string()
            })?;
            (
                origin.clone(),
                "Authorization",
                format!("Bearer {}", bearer),
            )
        }
    };

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| e.to_string())?;

    let submit_url = format!("{}/{}", origin, endpoint);
    let response = client
        .post(&submit_url)
        .header(auth_header, &auth_value)
        .header("x-idempotency-key", uuid::Uuid::new_v4().to_string())
        .json(arguments)
        .send()
        .await
        .map_err(|e| format!("FAL submit failed: {}", e))?;
    let status = response.status();
    if status.is_client_error() {
        let detail = response.text().await.unwrap_or_default();
        let truncated: String = detail.chars().take(300).collect();
        return Err(format!(
            "FAL rejected the request (HTTP {}): {}",
            status, truncated
        ));
    }
    if !status.is_success() {
        return Err(format!("FAL submit failed (HTTP {})", status));
    }
    let submission: Value = response
        .json()
        .await
        .map_err(|e| format!("FAL submit returned an unreadable body: {}", e))?;
    let request_id = submission
        .get("request_id")
        .and_then(|r| r.as_str())
        .ok_or_else(|| "FAL submit did not return a request_id".to_string())?
        .to_string();

    // Poll status until terminal, then fetch the result.
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(FAL_POLL_DEADLINE_SECONDS);
    loop {
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "FAL job {} did not finish within {}s",
                request_id, FAL_POLL_DEADLINE_SECONDS
            ));
        }
        tokio::time::sleep(std::time::Duration::from_secs(FAL_POLL_INTERVAL_SECONDS)).await;

        let status_url = format!("{}/{}/requests/{}/status", origin, endpoint, request_id);
        let status_response = client
            .get(&status_url)
            .header(auth_header, &auth_value)
            .send()
            .await
            .map_err(|e| format!("FAL status poll failed: {}", e))?;
        if !status_response.status().is_success() {
            continue;
        }
        let status_body: Value = match status_response.json().await {
            Ok(body) => body,
            Err(_) => continue,
        };
        let state = status_body
            .get("status")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_ascii_uppercase();
        match state.as_str() {
            "COMPLETED" => {
                let result_url = format!("{}/{}/requests/{}", origin, endpoint, request_id);
                let result_response = client
                    .get(&result_url)
                    .header(auth_header, &auth_value)
                    .send()
                    .await
                    .map_err(|e| format!("FAL result fetch failed: {}", e))?;
                let result: Value = result_response
                    .json()
                    .await
                    .map_err(|e| format!("FAL result was unreadable: {}", e))?;
                return Ok(result);
            }
            "FAILED" | "ERROR" | "CANCELLED" => {
                let detail = status_body
                    .get("error")
                    .and_then(|e| e.as_str())
                    .unwrap_or("unknown error");
                return Err(format!("FAL job failed: {}", detail));
            }
            _ => continue, // IN_PROGRESS / IN_QUEUE / ...
        }
    }
}

pub struct FalVideoGenProvider;

#[async_trait]
impl VideoGenProvider for FalVideoGenProvider {
    fn name(&self) -> &str {
        "fal"
    }

    fn display_name(&self) -> String {
        "FAL".to_string()
    }

    fn is_available(&self) -> bool {
        fal_route().is_some()
    }

    fn default_model(&self) -> String {
        FAL_DEFAULT_MODEL.to_string()
    }

    async fn generate(&self, prompt: &str, params: VideoGenParams) -> Value {
        // The family id rides in `params.model` (tool resolution already
        // applied config/arg precedence above the provider default).
        let config = crate::config::UlncLawConfig::default();
        let explicit = if params.model_override_explicit {
            Some(params.model.as_str())
        } else {
            None
        };
        let (family_id, family) = resolve_fal_family(explicit, &config);

        let prompt = prompt.trim();
        let image_url = params
            .image_url
            .as_deref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty());
        let (endpoint, modality_used) = match image_url {
            Some(_) => (family.image_endpoint, "image"),
            None => (family.text_endpoint, "text"),
        };

        if prompt.is_empty() {
            return error_response(
                "prompt is required.",
                "missing_prompt",
                "fal",
                &family_id,
                prompt,
            );
        }

        let payload = fal_build_payload(
            &family,
            prompt,
            image_url,
            params.duration,
            &params.aspect_ratio,
            &params.resolution,
            params.negative_prompt.as_deref(),
            params.audio,
            params.seed,
        );

        let result = match fal_submit_and_wait(endpoint, &payload).await {
            Ok(result) => result,
            Err(e) => {
                tracing::warn!(
                    "FAL video gen failed (family={}, endpoint={}): {}",
                    family_id,
                    endpoint,
                    e
                );
                return error_response(
                    &format!("FAL video generation failed: {}", e),
                    "api_error",
                    "fal",
                    &family_id,
                    prompt,
                );
            }
        };

        let video = result.get("video");
        let url = match video {
            Some(Value::Object(_)) => video
                .and_then(|v| v.get("url"))
                .and_then(|u| u.as_str())
                .map(|s| s.to_string()),
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        };
        let Some(url) = url else {
            return error_response(
                "FAL returned no video URL in response",
                "empty_response",
                "fal",
                &family_id,
                prompt,
            );
        };

        let mut extra = json!({"endpoint": endpoint});
        if let Some(Value::Object(video)) = video {
            if let Some(file_size) = video.get("file_size") {
                extra["file_size"] = file_size.clone();
            }
            if let Some(content_type) = video.get("content_type") {
                extra["content_type"] = content_type.clone();
            }
        }
        let aspect_in_payload = payload.get("aspect_ratio").is_some();
        let duration_sent = payload
            .get("duration")
            .and_then(|d| d.as_str())
            .and_then(|s| {
                s.chars()
                    .filter(|c| c.is_ascii_digit())
                    .collect::<String>()
                    .parse::<i64>()
                    .ok()
            })
            .unwrap_or(0);
        success_response(
            &url,
            &family_id,
            prompt,
            modality_used,
            if aspect_in_payload {
                &params.aspect_ratio
            } else {
                ""
            },
            duration_sent,
            "fal",
            Some(extra),
        )
    }
}

pub fn fal_provider() -> Arc<dyn VideoGenProvider> {
    Arc::new(FalVideoGenProvider)
}

// ===========================================================================
// DeepInfra — OpenAI-compatible /videos (hermes
// OpenAICompatibleVideoGenProvider + plugins/video_gen/deepinfra)
// ===========================================================================

const DEEPINFRA_ENV_KEY: &str = "DEEPINFRA_API_KEY";
const DEEPINFRA_DEFAULT_BASE_URL: &str = "https://api.deepinfra.com/v1/openai";
const OPENAI_COMPAT_POLL_INTERVAL_SECONDS: u64 = 5;
const OPENAI_COMPAT_POLL_DEADLINE_SECONDS: u64 = 900;

fn deepinfra_api_key() -> String {
    crate::config::get_env_value(DEEPINFRA_ENV_KEY)
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn deepinfra_base_url() -> String {
    crate::config::get_env_value("DEEPINFRA_BASE_URL")
        .map(|v| v.trim().trim_end_matches('/').to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEEPINFRA_DEFAULT_BASE_URL.to_string())
}

/// Live catalog probe (hermes `_fetch_deepinfra_models_by_tag("video-gen")`
/// — best-effort, empty when unreachable so the picker shows no options).
fn deepinfra_video_models() -> Vec<String> {
    let client = match reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(client) => client,
        Err(_) => return Vec::new(),
    };
    let url = format!("{}/models", deepinfra_base_url());
    let Ok(response) = client.get(&url).bearer_auth(deepinfra_api_key()).send() else {
        return Vec::new();
    };
    let Ok(body) = response.json::<Value>() else {
        return Vec::new();
    };
    let items = body
        .get("data")
        .and_then(|d| d.as_array())
        .cloned()
        .unwrap_or_default();
    items
        .iter()
        .filter_map(|item| item.get("id").and_then(|id| id.as_str()))
        .filter(|id| id.to_ascii_lowercase().contains("video"))
        .map(|id| id.to_string())
        .collect()
}

/// Save a video under `<home>/videos` (hermes `save_url_video` /
/// `save_bytes_video`).
fn save_video_bytes(bytes: &[u8], prefix: &str) -> Result<std::path::PathBuf, String> {
    let dir = crate::config::ulnclaw_home().join("videos");
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {}", dir.display(), e))?;
    let path = dir.join(format!("{}-{}.mp4", prefix, uuid::Uuid::new_v4()));
    std::fs::write(&path, bytes).map_err(|e| format!("write {}: {}", path.display(), e))?;
    Ok(path)
}

pub struct DeepInfraVideoGenProvider;

#[async_trait]
impl VideoGenProvider for DeepInfraVideoGenProvider {
    fn name(&self) -> &str {
        "deepinfra"
    }

    fn display_name(&self) -> String {
        "DeepInfra".to_string()
    }

    fn is_available(&self) -> bool {
        !deepinfra_api_key().is_empty()
    }

    fn default_model(&self) -> String {
        // No hardcoded model ids (hermes parity): the live catalog decides;
        // an empty string signals "no model available" to the caller.
        deepinfra_video_models()
            .first()
            .cloned()
            .unwrap_or_default()
    }

    async fn generate(&self, prompt: &str, params: VideoGenParams) -> Value {
        let name = self.name();
        if prompt.trim().is_empty() {
            return error_response("prompt is required", "invalid_request", name, "", prompt);
        }
        if deepinfra_api_key().is_empty() {
            return error_response(
                &format!("{} is not set", DEEPINFRA_ENV_KEY),
                "missing_credentials",
                name,
                "",
                prompt,
            );
        }
        if params.model.is_empty() {
            return error_response(
                &format!("no {} video model available (live catalog empty?)", name),
                "no_model",
                name,
                "",
                prompt,
            );
        }
        let model_id = params.model.clone();

        // OpenAI videos.create kwargs; provider-specific fields ride along.
        let mut body = json!({"model": model_id, "prompt": prompt});
        if let Some(duration) = params.duration {
            body["seconds"] = json!(duration.to_string());
        }
        if !params.resolution.is_empty() {
            body["size"] = json!(params.resolution);
        }
        if let Some(negative) = &params.negative_prompt {
            body["negative_prompt"] = json!(negative);
        }
        if !params.aspect_ratio.is_empty() {
            body["aspect_ratio"] = json!(params.aspect_ratio);
        }
        if let Some(image_url) = &params.image_url {
            body["image_url"] = json!(image_url);
        }
        if let Some(seed) = params.seed {
            body["seed"] = json!(seed);
        }

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .expect("reqwest client builds");
        let base_url = deepinfra_base_url();
        let auth = deepinfra_api_key();

        // create
        let create_response = match client
            .post(format!("{}/videos", base_url))
            .bearer_auth(&auth)
            .json(&body)
            .send()
            .await
        {
            Ok(response) => response,
            Err(e) => {
                return error_response(
                    &format!("{} video generation failed: {}", name, e),
                    "api_error",
                    name,
                    &model_id,
                    prompt,
                )
            }
        };
        if !create_response.status().is_success() {
            let status = create_response.status();
            let detail = create_response.text().await.unwrap_or_default();
            let truncated: String = detail.chars().take(300).collect();
            return error_response(
                &format!(
                    "{} video generation failed (HTTP {}): {}",
                    name, status, truncated
                ),
                "api_error",
                name,
                &model_id,
                prompt,
            );
        }
        let mut job: Value = match create_response.json().await {
            Ok(job) => job,
            Err(e) => {
                return error_response(
                    &format!("{} video generation failed: {}", name, e),
                    "api_error",
                    name,
                    &model_id,
                    prompt,
                )
            }
        };

        // poll with a hard deadline (hermes `_create_and_poll`).
        let terminal = [
            "completed",
            "succeeded",
            "failed",
            "error",
            "cancelled",
            "canceled",
        ];
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_secs(OPENAI_COMPAT_POLL_DEADLINE_SECONDS);
        let job_id = job
            .get("id")
            .and_then(|id| id.as_str())
            .unwrap_or("?")
            .to_string();
        loop {
            let status = job
                .get("status")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string();
            if terminal.contains(&status.as_str()) {
                break;
            }
            if std::time::Instant::now() >= deadline {
                return error_response(
                    &format!(
                        "video job {} did not reach a terminal status within {}s (last status={})",
                        job_id, OPENAI_COMPAT_POLL_DEADLINE_SECONDS, status
                    ),
                    "timeout",
                    name,
                    &model_id,
                    prompt,
                );
            }
            tokio::time::sleep(std::time::Duration::from_secs(
                OPENAI_COMPAT_POLL_INTERVAL_SECONDS,
            ))
            .await;
            match client
                .get(format!("{}/videos/{}", base_url, job_id))
                .bearer_auth(&auth)
                .send()
                .await
            {
                Ok(response) if response.status().is_success() => {
                    if let Ok(updated) = response.json::<Value>().await {
                        job = updated;
                    }
                }
                _ => continue,
            }
        }

        let status = job
            .get("status")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();
        if status != "completed" && status != "succeeded" {
            let job_error = job.get("error").map(|e| e.to_string());
            return error_response(
                &job_error.unwrap_or_else(|| format!("video job ended with status={:?}", status)),
                "job_failed",
                name,
                &model_id,
                prompt,
            );
        }

        // Resolve the output: delivery URL in `data[]`, else the download
        // endpoint (hermes parity).
        let mut url: Option<String> = None;
        if let Some(items) = job.get("data").and_then(|d| d.as_array()) {
            for item in items {
                if let Some(candidate) = item.get("url").and_then(|u| u.as_str()) {
                    if !candidate.is_empty() {
                        url = Some(candidate.to_string());
                        break;
                    }
                }
            }
        }

        let video_ref = match &url {
            Some(url) => {
                // Materialise the (often short-lived) delivery URL locally.
                match reqwest::get(url).await.and_then(|r| r.error_for_status()) {
                    Ok(response) => match response.bytes().await {
                        Ok(bytes) => match save_video_bytes(&bytes, name) {
                            Ok(path) => path.display().to_string(),
                            Err(e) => {
                                tracing::debug!(
                                    "{}: saving video locally failed ({}); returning URL",
                                    name,
                                    e
                                );
                                url.clone()
                            }
                        },
                        Err(e) => {
                            tracing::debug!(
                                "{}: saving video locally failed ({}); returning URL",
                                name,
                                e
                            );
                            url.clone()
                        }
                    },
                    Err(e) => {
                        tracing::debug!(
                            "{}: saving video locally failed ({}); returning URL",
                            name,
                            e
                        );
                        url.clone()
                    }
                }
            }
            None => {
                let download = client
                    .get(format!("{}/videos/{}/content", base_url, job_id))
                    .bearer_auth(&auth)
                    .send()
                    .await
                    .and_then(|r| r.error_for_status());
                match download {
                    Ok(response) => match response.bytes().await {
                        Ok(bytes) => match save_video_bytes(&bytes, name) {
                            Ok(path) => path.display().to_string(),
                            Err(e) => {
                                return error_response(
                                    &format!(
                                    "{} video job succeeded but no output could be retrieved: {}",
                                    name, e
                                ),
                                    "empty_response",
                                    name,
                                    &model_id,
                                    prompt,
                                )
                            }
                        },
                        Err(e) => {
                            return error_response(
                                &format!(
                                    "{} video job succeeded but no output could be retrieved: {}",
                                    name, e
                                ),
                                "empty_response",
                                name,
                                &model_id,
                                prompt,
                            )
                        }
                    },
                    Err(e) => {
                        return error_response(
                            &format!(
                                "{} video job succeeded but no output could be retrieved: {}",
                                name, e
                            ),
                            "empty_response",
                            name,
                            &model_id,
                            prompt,
                        )
                    }
                }
            }
        };

        let modality = if params.image_url.is_some() {
            "image"
        } else {
            "text"
        };
        success_response(
            &video_ref,
            &model_id,
            prompt,
            modality,
            &params.aspect_ratio,
            params.duration.unwrap_or(0),
            name,
            None,
        )
    }
}

pub fn deepinfra_provider() -> Arc<dyn VideoGenProvider> {
    Arc::new(DeepInfraVideoGenProvider)
}

/// Register every compiled-in video backend (hermes plugin auto-discovery).
pub fn register_all_providers() {
    crate::video_gen::register_provider(crate::video_gen_xai::provider());
    crate::video_gen::register_provider(fal_provider());
    crate::video_gen::register_provider(deepinfra_provider());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::UlncLawConfig;

    #[test]
    fn family_resolution_precedence() {
        let config = UlncLawConfig::default();
        // Explicit wins.
        let (id, _) = resolve_fal_family(Some("veo3.1"), &config);
        assert_eq!(id, "veo3.1");
        // Unknown falls back to default.
        let (id, _) = resolve_fal_family(Some("nope"), &config);
        assert_eq!(id, FAL_DEFAULT_MODEL);
        // Config-level family selection.
        let mut config = UlncLawConfig::default();
        config.video_gen.fal.model = Some("kling-v3-4k".to_string());
        let (id, _) = resolve_fal_family(None, &config);
        assert_eq!(id, "kling-v3-4k");
    }

    #[test]
    fn duration_clamping_modes() {
        let families = fal_families();
        let pixverse = families
            .iter()
            .find(|(id, _)| *id == "pixverse-v6")
            .map(|(_, f)| f)
            .unwrap();
        // Range family: unset omits, set clamps.
        assert_eq!(fal_clamp_duration(pixverse, None), None);
        assert_eq!(fal_clamp_duration(pixverse, Some(30)), Some(15));
        assert_eq!(fal_clamp_duration(pixverse, Some(7)), Some(7));

        let veo = families
            .iter()
            .find(|(id, _)| *id == "veo3.1")
            .map(|(_, f)| f)
            .unwrap();
        // Enum family: unset takes the first entry; nearest wins.
        assert_eq!(fal_clamp_duration(veo, None), Some(4));
        assert_eq!(fal_clamp_duration(veo, Some(6)), Some(6));
        assert_eq!(
            fal_clamp_duration(veo, Some(7)),
            Some(6) /* or 8: nearest */
        );

        let ltx = families
            .iter()
            .find(|(id, _)| *id == "ltx-2.3")
            .map(|(_, f)| f)
            .unwrap();
        assert_eq!(fal_clamp_duration(ltx, Some(9)), Some(9));
    }

    #[test]
    fn payload_drops_undeclared_keys() {
        let families = fal_families();
        let happy = families
            .iter()
            .find(|(id, _)| *id == "happy-horse")
            .map(|(_, f)| f)
            .unwrap();
        let payload = fal_build_payload(
            happy,
            "a horse runs",
            None,
            Some(8),
            "16:9",
            "720p",
            Some("avoid blur"),
            Some(true),
            Some(42),
        );
        // happy-horse declares no aspect/resolution/duration/audio/negative.
        assert!(payload.get("aspect_ratio").is_none());
        assert!(payload.get("resolution").is_none());
        assert!(payload.get("duration").is_none());
        assert!(payload.get("generate_audio").is_none());
        assert!(payload.get("negative_prompt").is_none());
        assert_eq!(payload["prompt"], json!("a horse runs"));
        assert_eq!(payload["seed"], json!(42));
    }

    #[test]
    fn payload_duration_suffix_and_image_key() {
        let families = fal_families();
        let veo = families
            .iter()
            .find(|(id, _)| *id == "veo3.1")
            .map(|(_, f)| f)
            .unwrap();
        let payload = fal_build_payload(
            &veo,
            "cinematic shot",
            Some("https://x/i.png"),
            Some(6),
            "16:9",
            "720p",
            None,
            None,
            None,
        );
        assert_eq!(payload["duration"], json!("6s"));
        assert_eq!(payload["image_url"], json!("https://x/i.png"));

        let kling = families
            .iter()
            .find(|(id, _)| *id == "kling-v3-4k")
            .map(|(_, f)| f)
            .unwrap();
        let payload = fal_build_payload(
            &kling,
            "pan",
            Some("https://x/i.png"),
            None,
            "1:1",
            "720p",
            None,
            None,
            None,
        );
        assert_eq!(payload["start_image_url"], json!("https://x/i.png"));
        assert!(payload.get("image_url").is_none());
    }

    #[test]
    fn providers_identity() {
        assert_eq!(crate::video_gen_xai::provider().name(), "xai");
        assert_eq!(fal_provider().name(), "fal");
        assert_eq!(deepinfra_provider().name(), "deepinfra");
    }
}

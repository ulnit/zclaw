//! xAI Grok-Imagine video generation backend — port of hermes
//! `plugins/video_gen/xai/__init__.py` (v2026.8.3).
//!
//! Surface: text-to-video, image-to-video, and reference-to-video through
//! the unified `video_generate` provider, plus `xai_video_edit` /
//! `xai_video_extend` exposed as separate tools (edit/extend are
//! provider-specific, so they stay off the unified surface).
//!
//! Authentication: an xAI Grok OAuth token from `auth.json`
//! (`providers.xai-oauth.tokens.access_token`, written by a hermes-side
//! sign-in) or `XAI_API_KEY` (process env or the ulnclaw `.env`).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::video_gen::{error_response, success_response, VideoGenParams, VideoGenProvider};

pub const DEFAULT_XAI_BASE_URL: &str = "https://api.x.ai/v1";
pub const DEFAULT_TEXT_TO_VIDEO_MODEL: &str = "grok-imagine-video";
pub const DEFAULT_IMAGE_TO_VIDEO_MODEL: &str = "grok-imagine-video-1.5";
pub const DEFAULT_MODEL: &str = DEFAULT_TEXT_TO_VIDEO_MODEL;
const DEFAULT_DURATION: i64 = 8;
const DEFAULT_ASPECT_RATIO: &str = "16:9";
const DEFAULT_RESOLUTION: &str = "720p";
const DEFAULT_TIMEOUT_SECONDS: i64 = 240;
const DEFAULT_POLL_INTERVAL_SECONDS: i64 = 5;
const DEFAULT_EXTEND_DURATION: i64 = 6;

const VALID_ASPECT_RATIOS: &[&str] = &["1:1", "16:9", "9:16", "4:3", "3:4", "3:2", "2:3"];
const VALID_RESOLUTIONS: &[&str] = &["480p", "720p"];
const MAX_REFERENCE_IMAGES: usize = 7;

/// Model ids that behave like the 1.5 image-to-video model (hermes
/// `_IMAGE_TO_VIDEO_COMPAT_MODEL_IDS`).
const IMAGE_TO_VIDEO_COMPAT_MODEL_IDS: &[&str] = &[
    "grok-imagine-video-1.5-preview",
    "grok-imagine-video-1.5-2026-05-30",
];

// ---------------------------------------------------------------------------
// Credentials (hermes `_resolve_xai_credentials` via xai_http)
// ---------------------------------------------------------------------------

/// `(api_key, base_url)` — auth.json OAuth token first, then `XAI_API_KEY`;
/// `XAI_BASE_URL` overrides the endpoint. No OAuth refresh (ulnclaw parity
/// note: tokens are used as-is).
pub fn resolve_xai_credentials() -> (String, String) {
    let mut api_key = String::new();

    // auth.json `providers.xai-oauth.tokens.access_token` (peek only).
    if let Ok(data) = std::fs::read_to_string(crate::managed_gateway::auth_json_path()) {
        if let Ok(parsed) = serde_json::from_str::<Value>(&data) {
            let token = parsed
                .get("providers")
                .and_then(|p| p.get("xai-oauth"))
                .and_then(|x| x.get("tokens"))
                .and_then(|t| t.get("access_token"))
                .and_then(|t| t.as_str())
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty());
            if let Some(token) = token {
                api_key = token;
            }
        }
    }

    if api_key.is_empty() {
        api_key = crate::config::get_env_value("XAI_API_KEY")
            .unwrap_or_default()
            .trim()
            .to_string();
    }

    let base_url = crate::config::get_env_value("XAI_BASE_URL")
        .map(|v| v.trim().trim_end_matches('/').to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_XAI_BASE_URL.to_string());
    (api_key, base_url)
}

pub fn has_xai_video_credentials() -> bool {
    let (api_key, _) = resolve_xai_credentials();
    !api_key.is_empty()
}

fn auth_required_response(prompt: &str) -> Value {
    error_response(
        "No xAI credentials found. Sign in to xAI Grok (auth.json providers.xai-oauth) \
         or set XAI_API_KEY from https://console.x.ai/.",
        "auth_required",
        "xai",
        "",
        prompt,
    )
}

// ---------------------------------------------------------------------------
// Input normalization (hermes `_image_ref_to_xai_input` & friends)
// ---------------------------------------------------------------------------

/// Local image file → data URI; URLs/data URIs pass through.
fn image_ref_to_xai_url(value: &str, ctx_home: Option<&std::path::Path>) -> String {
    let reference = value.trim();
    if reference.is_empty() {
        return String::new();
    }
    let lower = reference.to_ascii_lowercase();
    if lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("data:image/")
    {
        return reference.to_string();
    }
    let path = expand_media_path(reference, ctx_home);
    if !path.is_file() {
        return reference.to_string();
    }
    let mime = match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        _ => "image/jpeg",
    };
    let Ok(bytes) = std::fs::read(&path) else {
        return reference.to_string();
    };
    use base64::Engine;
    format!(
        "data:{};base64,{}",
        mime,
        base64::engine::general_purpose::STANDARD.encode(&bytes)
    )
}

fn expand_media_path(reference: &str, ctx_home: Option<&std::path::Path>) -> std::path::PathBuf {
    if let Some(rest) = reference.strip_prefix("~/") {
        if let Some(home) = ctx_home.map(|h| h.to_path_buf()).or_else(dirs::home_dir) {
            return home.join(rest);
        }
    }
    std::path::PathBuf::from(reference)
}

fn image_ref_to_xai_input(value: &str, ctx_home: Option<&std::path::Path>) -> Option<Value> {
    let reference = image_ref_to_xai_url(value, ctx_home);
    if reference.is_empty() {
        return None;
    }
    let lower = reference.to_ascii_lowercase();
    if lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("data:image/")
    {
        return Some(json!({"url": reference}));
    }
    None
}

/// `(public_video_url, temporary_url, stored_public_url)` — hermes
/// `_xai_video_output_urls`.
fn xai_video_output_urls(video: &Value) -> (String, Option<String>, Option<String>) {
    let file_output = video
        .get("file_output")
        .filter(|f| f.is_object())
        .cloned()
        .unwrap_or(json!({}));
    let stored_public = file_output
        .get("public_url")
        .and_then(|p| p.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let temporary = video
        .get("url")
        .and_then(|u| u.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let public_video_url = stored_public
        .clone()
        .or_else(|| temporary.clone())
        .unwrap_or_default();
    let temporary_out = match (&temporary, &stored_public) {
        (Some(t), Some(s)) if t != s => Some(t.clone()),
        _ => None,
    };
    (public_video_url, temporary_out, stored_public)
}

fn clamp_duration(
    duration: Option<i64>,
    has_reference_images: bool,
    max_seconds: i64,
    default: i64,
) -> i64 {
    let mut value = duration.unwrap_or(default);
    if value < 1 {
        value = 1;
    }
    if value > max_seconds {
        value = max_seconds;
    }
    if has_reference_images && value > 10 {
        value = 10;
    }
    value
}

/// Select xAI's text/video model without treating config as a prompt
/// override (hermes `_resolve_model_for_modality`).
fn resolve_model_for_modality(model: Option<&str>, modality: &str, explicit_model: bool) -> String {
    let requested = model.map(|m| m.trim().to_string()).unwrap_or_default();
    if explicit_model && !requested.is_empty() {
        return requested;
    }
    if modality == "image" {
        return DEFAULT_IMAGE_TO_VIDEO_MODEL.to_string();
    }
    if requested == DEFAULT_IMAGE_TO_VIDEO_MODEL
        || IMAGE_TO_VIDEO_COMPAT_MODEL_IDS.contains(&requested.as_str())
    {
        return DEFAULT_TEXT_TO_VIDEO_MODEL.to_string();
    }
    if requested.is_empty() {
        DEFAULT_TEXT_TO_VIDEO_MODEL.to_string()
    } else {
        requested
    }
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

fn xai_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .expect("reqwest client builds")
}

/// POST to one of xAI's async video endpoints and return the request_id
/// (hermes `_submit`).
async fn xai_submit(
    client: &reqwest::Client,
    payload: &Value,
    api_key: &str,
    base_url: &str,
    endpoint: &str,
) -> Result<String, String> {
    let response = client
        .post(format!("{}/videos/{}", base_url, endpoint))
        .bearer_auth(api_key)
        .header("x-idempotency-key", uuid::Uuid::new_v4().to_string())
        .json(payload)
        .send()
        .await
        .map_err(|e| format!("xAI submit failed: {}", e))?;
    let status = response.status();
    if !status.is_success() {
        let detail = response.text().await.unwrap_or_default();
        let truncated: String = detail.chars().take(500).collect();
        return Err(format!("xAI submit failed ({}): {}", status, truncated));
    }
    let body: Value = response
        .json()
        .await
        .map_err(|e| format!("xAI submit returned an unreadable body: {}", e))?;
    body.get("request_id")
        .and_then(|r| r.as_str())
        .map(|r| r.to_string())
        .ok_or_else(|| "xAI video response did not include request_id".to_string())
}

/// Poll until done/failed/timeout (hermes `_poll`).
async fn xai_poll(
    client: &reqwest::Client,
    request_id: &str,
    api_key: &str,
    base_url: &str,
    timeout_seconds: i64,
    poll_interval: i64,
) -> (String, Value) {
    let mut elapsed = 0i64;
    let mut last_status = "queued".to_string();
    while elapsed < timeout_seconds {
        let Ok(response) = client
            .get(format!("{}/videos/{}", base_url, request_id))
            .bearer_auth(api_key)
            .send()
            .await
        else {
            tokio::time::sleep(std::time::Duration::from_secs(poll_interval as u64)).await;
            elapsed += poll_interval;
            continue;
        };
        if response.status().is_success() {
            if let Ok(body) = response.json::<Value>().await {
                last_status = body
                    .get("status")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if last_status == "done" {
                    return ("done".to_string(), body);
                }
                if matches!(
                    last_status.as_str(),
                    "failed" | "error" | "expired" | "cancelled"
                ) {
                    return (last_status.clone(), body);
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(poll_interval as u64)).await;
        elapsed += poll_interval;
    }
    ("timeout".to_string(), json!({"status": last_status}))
}

/// Submit + poll + shape the response (hermes `_submit_xai_video_payload`;
/// xAI storage options are not ported — no Portal storage config).
async fn submit_xai_video_payload(
    api_key: &str,
    base_url: &str,
    endpoint: &str,
    payload: &Value,
    prompt: &str,
    resolved_model: &str,
    modality: &str,
    aspect_ratio: &str,
    duration: i64,
    operation: &str,
    resolution: Option<&str>,
) -> Value {
    let client = xai_client();
    let request_id = match xai_submit(&client, payload, api_key, base_url, endpoint).await {
        Ok(request_id) => request_id,
        Err(e) => {
            return error_response(&e, "api_error", "xai", resolved_model, prompt);
        }
    };

    let (status, body) = xai_poll(
        &client,
        &request_id,
        api_key,
        base_url,
        DEFAULT_TIMEOUT_SECONDS,
        DEFAULT_POLL_INTERVAL_SECONDS,
    )
    .await;

    if status == "done" {
        let video = body
            .get("video")
            .filter(|v| v.is_object())
            .cloned()
            .unwrap_or(json!({}));
        let file_output = video
            .get("file_output")
            .filter(|f| f.is_object())
            .cloned()
            .unwrap_or(json!({}));
        let (public_video_url, temporary_url, stored_public_url) = xai_video_output_urls(&video);
        if public_video_url.is_empty() {
            return error_response(
                "xAI video request completed without a video URL",
                "empty_response",
                "xai",
                body.get("model")
                    .and_then(|m| m.as_str())
                    .unwrap_or(resolved_model),
                prompt,
            );
        }
        let mut extra = json!({
            "request_id": request_id,
            "operation": operation,
            "storage_enabled": false,
        });
        if let Some(resolution) = resolution {
            extra["resolution"] = json!(resolution);
        }
        if let Some(stored) = &stored_public_url {
            extra["public_url"] = json!(stored);
        }
        if let Some(temporary) = &temporary_url {
            extra["temporary_url"] = json!(temporary);
        }
        for key in [
            "filename",
            "expires_at",
            "public_url_expires_at",
            "public_url_error",
            "storage_error",
        ] {
            if let Some(value) = file_output.get(key) {
                extra[key] = value.clone();
            }
        }
        if let Some(usage) = body.get("usage") {
            extra["usage"] = usage.clone();
        }
        let video_duration = video
            .get("duration")
            .and_then(|d| d.as_i64())
            .unwrap_or(duration);
        let model = body
            .get("model")
            .and_then(|m| m.as_str())
            .unwrap_or(resolved_model);
        return success_response(
            &public_video_url,
            model,
            prompt,
            modality,
            aspect_ratio,
            video_duration,
            "xai",
            Some(extra),
        );
    }

    if status == "timeout" {
        return error_response(
            &format!(
                "Timed out waiting for xAI video request after {}s",
                DEFAULT_TIMEOUT_SECONDS
            ),
            "timeout",
            "xai",
            resolved_model,
            prompt,
        );
    }

    let message = body
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .or_else(|| body.get("message").and_then(|m| m.as_str()))
        .map(|m| m.to_string())
        .unwrap_or_else(|| format!("xAI video request ended with status '{}'", status));
    error_response(
        &message,
        &format!("xai_{}", status),
        "xai",
        resolved_model,
        prompt,
    )
}

// ---------------------------------------------------------------------------
// Generation (hermes `_generate_xai_video_async`)
// ---------------------------------------------------------------------------

pub struct XaiVideoGenProvider;

#[async_trait]
impl VideoGenProvider for XaiVideoGenProvider {
    fn name(&self) -> &str {
        "xai"
    }

    fn display_name(&self) -> String {
        "xAI".to_string()
    }

    fn is_available(&self) -> bool {
        has_xai_video_credentials()
    }

    fn default_model(&self) -> String {
        DEFAULT_MODEL.to_string()
    }

    async fn generate(&self, prompt: &str, params: VideoGenParams) -> Value {
        generate_xai_video(
            prompt,
            Some(params.model.as_str()),
            params.model_override_explicit,
            params.image_url.as_deref(),
            params.reference_image_urls.as_deref(),
            params.duration,
            &params.aspect_ratio,
            &params.resolution,
            None,
        )
        .await
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn generate_xai_video(
    prompt: &str,
    model: Option<&str>,
    explicit_model: bool,
    image_url: Option<&str>,
    reference_image_urls: Option<&[String]>,
    duration: Option<i64>,
    aspect_ratio: &str,
    resolution: &str,
    ctx_home: Option<&std::path::Path>,
) -> Value {
    let (api_key, base_url) = resolve_xai_credentials();
    if api_key.is_empty() {
        return auth_required_response(prompt);
    }

    let prompt = prompt.trim();
    let image_input = if image_url
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .is_some()
    {
        match image_ref_to_xai_input(image_url.unwrap().trim(), ctx_home) {
            Some(input) => Some(input),
            None => {
                return error_response(
                    "image_url must be a public HTTPS URL or data URI (e.g. the \
                     `image`/`public_url` from a prior Imagine result)",
                    "invalid_image_url",
                    "xai",
                    "",
                    prompt,
                )
            }
        }
    } else {
        None
    };

    let mut normalized_aspect_ratio = aspect_ratio.trim().to_string();
    if normalized_aspect_ratio.is_empty() {
        normalized_aspect_ratio = DEFAULT_ASPECT_RATIO.to_string();
    }
    let mut normalized_resolution = resolution.trim().to_ascii_lowercase();
    if normalized_resolution.is_empty() {
        normalized_resolution = DEFAULT_RESOLUTION.to_string();
    }

    // Normalize reference images (hermes `_normalize_reference_images`).
    let mut refs: Vec<Value> = Vec::new();
    if let Some(urls) = reference_image_urls {
        for url in urls {
            let cleaned = url.trim();
            if cleaned.is_empty() {
                continue;
            }
            match image_ref_to_xai_input(cleaned, ctx_home) {
                Some(normalized) => refs.push(normalized),
                None => {
                    return error_response(
                        "reference_image_urls must be public HTTPS URLs or data URIs (e.g. the \
                         `image`/`public_url` from a prior Imagine result)",
                        "invalid_reference_image_urls",
                        "xai",
                        "",
                        prompt,
                    )
                }
            }
        }
    }
    let refs = if refs.is_empty() { None } else { Some(refs) };

    if prompt.is_empty() {
        return error_response(
            "prompt is required for xAI video generation",
            "missing_prompt",
            "xai",
            "",
            prompt,
        );
    }
    if refs.as_ref().map(|r| r.len()).unwrap_or(0) > MAX_REFERENCE_IMAGES {
        return error_response(
            &format!(
                "reference_image_urls supports at most {} images on xAI",
                MAX_REFERENCE_IMAGES
            ),
            "too_many_references",
            "xai",
            "",
            prompt,
        );
    }
    if image_input.is_some() && refs.is_some() {
        return error_response(
            "image_url and reference_image_urls cannot be combined on xAI",
            "conflicting_inputs",
            "xai",
            "",
            prompt,
        );
    }

    if !VALID_ASPECT_RATIOS.contains(&normalized_aspect_ratio.as_str()) {
        normalized_aspect_ratio = DEFAULT_ASPECT_RATIO.to_string();
    }
    if !VALID_RESOLUTIONS.contains(&normalized_resolution.as_str()) {
        normalized_resolution = DEFAULT_RESOLUTION.to_string();
    }

    let modality_used = if refs.is_some() {
        "reference"
    } else if image_input.is_some() {
        "image"
    } else {
        "text"
    };
    let mut resolved_model = resolve_model_for_modality(model, modality_used, explicit_model);
    if refs.is_some() && resolved_model != DEFAULT_TEXT_TO_VIDEO_MODEL {
        if explicit_model {
            return error_response(
                &format!(
                    "xAI reference-to-video requires {}; got {}",
                    DEFAULT_TEXT_TO_VIDEO_MODEL, resolved_model
                ),
                "unsupported_model",
                "xai",
                &resolved_model,
                prompt,
            );
        }
        resolved_model = DEFAULT_TEXT_TO_VIDEO_MODEL.to_string();
    }

    let clamped_duration = clamp_duration(duration, refs.is_some(), 15, DEFAULT_DURATION);
    let mut payload = json!({
        "model": resolved_model,
        "prompt": prompt,
        "duration": clamped_duration,
        "aspect_ratio": normalized_aspect_ratio,
        "resolution": normalized_resolution,
    });
    if let Some(image_input) = &image_input {
        payload["image"] = image_input.clone();
    }
    if let Some(refs) = &refs {
        payload["reference_images"] = json!(refs);
    }

    submit_xai_video_payload(
        &api_key,
        &base_url,
        "generations",
        &payload,
        prompt,
        &resolved_model,
        modality_used,
        &normalized_aspect_ratio,
        clamped_duration,
        "generate",
        Some(&normalized_resolution),
    )
    .await
}

// ---------------------------------------------------------------------------
// Edit / extend (hermes `_run_xai_video_mutation`)
// ---------------------------------------------------------------------------

async fn run_xai_video_mutation(
    prompt: &str,
    video_url: &str,
    model: Option<&str>,
    endpoint: &str,
    operation: &str,
    duration: i64,
) -> Value {
    let (api_key, base_url) = resolve_xai_credentials();
    if api_key.is_empty() {
        return auth_required_response(prompt);
    }
    let prompt = prompt.trim();
    if prompt.is_empty() {
        return error_response(
            "prompt is required for xAI video edit/extend",
            "missing_prompt",
            "xai",
            "",
            prompt,
        );
    }

    // video_url must be a public HTTPS MP4 URL (hermes
    // `_video_input_from_public_url` — local files become data URIs).
    let reference = video_url.trim();
    let video_input = if reference.is_empty() {
        None
    } else {
        let lower = reference.to_ascii_lowercase();
        if lower.starts_with("http://") || lower.starts_with("https://") {
            Some(json!({"url": reference}))
        } else {
            let path = expand_media_path(reference, None);
            if path.is_file() {
                if let Ok(bytes) = std::fs::read(&path) {
                    use base64::Engine;
                    let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
                    Some(json!({"url": format!("data:video/mp4;base64,{}", encoded)}))
                } else {
                    None
                }
            } else if lower.starts_with("data:video/") {
                Some(json!({"url": reference}))
            } else {
                None
            }
        }
    };
    let Some(video_input) = video_input else {
        return error_response(
            "video_url must be a public HTTPS MP4 URL (the `video`/`public_url` from a prior \
             Imagine result)",
            "missing_video",
            "xai",
            "",
            prompt,
        );
    };

    let resolved_model = resolve_model_for_modality(model, "text", model.is_some());
    let mut payload = json!({
        "model": resolved_model,
        "prompt": prompt,
        "video": video_input,
    });
    if endpoint == "extensions" {
        payload["duration"] = json!(duration);
    }

    submit_xai_video_payload(
        &api_key,
        &base_url,
        endpoint,
        &payload,
        prompt,
        &resolved_model,
        operation,
        DEFAULT_ASPECT_RATIO,
        duration,
        operation,
        None,
    )
    .await
}

pub async fn run_xai_video_edit(prompt: &str, video_url: &str, model: Option<&str>) -> Value {
    run_xai_video_mutation(prompt, video_url, model, "edits", "edit", DEFAULT_DURATION).await
}

pub async fn run_xai_video_extend(
    prompt: &str,
    video_url: &str,
    duration: Option<i64>,
    model: Option<&str>,
) -> Value {
    let clamped = clamp_duration(duration, false, 10, DEFAULT_EXTEND_DURATION);
    run_xai_video_mutation(prompt, video_url, model, "extensions", "extend", clamped).await
}

/// Provider registration entry (hermes plugin `register`).
pub fn provider() -> Arc<dyn VideoGenProvider> {
    Arc::new(XaiVideoGenProvider)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_modality_routing() {
        // Explicit model always wins.
        assert_eq!(
            resolve_model_for_modality(Some("custom"), "image", true),
            "custom"
        );
        // Image modality defaults to the 1.5 model.
        assert_eq!(
            resolve_model_for_modality(None, "image", false),
            DEFAULT_IMAGE_TO_VIDEO_MODEL
        );
        // The 1.5 model cannot do text-only — falls back.
        assert_eq!(
            resolve_model_for_modality(Some(DEFAULT_IMAGE_TO_VIDEO_MODEL), "text", false),
            DEFAULT_TEXT_TO_VIDEO_MODEL
        );
        assert_eq!(
            resolve_model_for_modality(Some("grok-imagine-video-1.5-preview"), "text", false),
            DEFAULT_TEXT_TO_VIDEO_MODEL
        );
        // Plain text keeps the requested model.
        assert_eq!(
            resolve_model_for_modality(Some("grok-imagine-video"), "text", false),
            DEFAULT_TEXT_TO_VIDEO_MODEL
        );
    }

    #[test]
    fn duration_clamping() {
        assert_eq!(clamp_duration(None, false, 15, DEFAULT_DURATION), 8);
        assert_eq!(clamp_duration(Some(30), false, 15, DEFAULT_DURATION), 15);
        assert_eq!(clamp_duration(Some(0), false, 15, DEFAULT_DURATION), 1);
        assert_eq!(clamp_duration(Some(14), true, 15, DEFAULT_DURATION), 10);
        assert_eq!(clamp_duration(None, false, 10, DEFAULT_EXTEND_DURATION), 6);
    }

    #[test]
    fn output_url_preference() {
        let video = json!({
            "url": "https://tmp.example/short.mp4",
            "file_output": {"public_url": "https://files-cdn.example/stored.mp4"}
        });
        let (public_url, temporary, stored) = xai_video_output_urls(&video);
        assert_eq!(public_url, "https://files-cdn.example/stored.mp4");
        assert_eq!(temporary.as_deref(), Some("https://tmp.example/short.mp4"));
        assert_eq!(
            stored.as_deref(),
            Some("https://files-cdn.example/stored.mp4")
        );

        let video = json!({"url": "https://tmp.example/only.mp4"});
        let (public_url, temporary, stored) = xai_video_output_urls(&video);
        assert_eq!(public_url, "https://tmp.example/only.mp4");
        assert!(temporary.is_none());
        assert!(stored.is_none());
    }

    #[test]
    fn image_input_normalization() {
        assert_eq!(
            image_ref_to_xai_input("https://example.com/x.png", None),
            Some(json!({"url": "https://example.com/x.png"}))
        );
        assert_eq!(
            image_ref_to_xai_input("data:image/png;base64,AAA", None),
            Some(json!({"url": "data:image/png;base64,AAA"}))
        );
        // Opaque non-file ids are not usable inputs.
        assert_eq!(image_ref_to_xai_input("opaque-id", None), None);
    }

    #[test]
    fn credential_resolution_env_fallback() {
        std::env::set_var("XAI_API_KEY", "test-key-123");
        let (key, base) = resolve_xai_credentials();
        // auth.json may or may not exist in the test environment; the env
        // fallback applies when it carries no xai-oauth token.
        assert!(!key.is_empty());
        assert_eq!(base, "https://api.x.ai/v1");
        std::env::remove_var("XAI_API_KEY");
    }
}

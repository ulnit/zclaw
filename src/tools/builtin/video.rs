//! Video generation tools — port of hermes `tools/video_generation_tool.py`
//! + `tools/flux3_video_tool.py` (v2026.8.3).
//!
//! Two surfaces:
//!   - `video_generate` — the unified tool dispatched through the
//!     `video_gen` provider registry (hermes plugin design; ulnclaw
//!     compiles backends in-tree and gates them on credentials).
//!   - `bfl_flux3_*` — six FLUX 3 tools that run through the Nous managed
//!     tool gateway (Bearer-auth passthrough, presigned media uploads,
//!     poll-until-done retrieval that saves the clip locally).

use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::tools::{tool, ToolAvailability, ToolContext, ToolRegistry};
use serde_json::{json, Value};

pub fn register(registry: &mut ToolRegistry) {
    // Compiled-in backends (hermes plugin auto-discovery).
    crate::video_gen_backends::register_all_providers();
    registry.register(video_generate_tool());
    registry.register(flux3_text_to_video_tool());
    registry.register(flux3_image_to_video_tool());
    registry.register(flux3_keyframes_to_video_tool());
    registry.register(flux3_video_continuation_tool());
    registry.register(flux3_get_result_tool());
    registry.register(flux3_prompting_guide_tool());
    registry.register(xai_video_edit_tool());
    registry.register(xai_video_extend_tool());
}

// ===========================================================================
// video_generate (hermes tools/video_generation_tool.py)
// ===========================================================================

fn coerce_int(value: &Value) -> Option<i64> {
    match value {
        Value::Null => None,
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

fn coerce_bool(value: &Value) -> Option<bool> {
    match value {
        Value::Null => None,
        Value::Bool(b) => Some(*b),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Some(true),
            "false" | "0" | "no" | "off" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn normalize_reference_images(value: &Value) -> Option<Vec<String>> {
    let items: Vec<Value> = match value {
        Value::Null => return None,
        Value::String(s) => vec![Value::String(s.clone())],
        Value::Array(list) => list.clone(),
        _ => return None,
    };
    let out: Vec<String> = items
        .iter()
        .filter_map(|item| item.as_str().map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty())
        .collect();
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn video_generate_check() -> ToolAvailability {
    // hermes check_video_generation_requirements: at least one registered
    // provider reports available.
    let any = crate::video_gen::list_providers().iter().any(|p| {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| p.is_available())).unwrap_or(false)
    });
    if any {
        ToolAvailability::available()
    } else {
        ToolAvailability::unavailable("no video generation backend is configured")
    }
}

fn video_generate_tool() -> crate::tools::Tool {
    let ratios: Vec<&str> = crate::video_gen::COMMON_ASPECT_RATIOS.to_vec();
    let resolutions: Vec<&str> = crate::video_gen::COMMON_RESOLUTIONS.to_vec();
    tool("video_generate")
        .description(
            "Generate a video from a text prompt (text-to-video), animate a still image \
             (image-to-video), or guide generation with reference images. Pass `image_url` to \
             animate an image or `reference_image_urls` for reference-to-video. Video \
             edit/extend workflows are not part of this unified surface; use a dedicated \
             provider-specific tool when one is available. The backend and model family are \
             user-configured (`[video_gen]` in config.toml); the agent does not pick them. \
             Long-running generations may take 30 seconds to several minutes — the call blocks \
             until the video is ready. Returns the result in the `video` field — either an HTTP \
             URL or an absolute file path.",
        )
        .parameters(json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "Text instruction describing the desired video, motion, subject, style, camera movement, etc."
                },
                "image_url": {
                    "type": "string",
                    "description": "Optional public HTTPS URL of a still image. When provided, the active backend routes to its image-to-video endpoint (animate the image); when omitted, it routes to text-to-video."
                },
                "reference_image_urls": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Optional list of public HTTPS reference image URLs (style or character refs)."
                },
                "duration": {
                    "type": "integer",
                    "description": "Desired video duration in seconds. Providers clamp to their supported range (commonly 4-15s). Omit to use the provider's default."
                },
                "aspect_ratio": {
                    "type": "string",
                    "enum": ratios,
                    "description": "Output aspect ratio. Providers clamp to their supported set.",
                    "default": crate::video_gen::DEFAULT_ASPECT_RATIO
                },
                "resolution": {
                    "type": "string",
                    "enum": resolutions,
                    "description": "Output resolution. Providers clamp to their supported set.",
                    "default": crate::video_gen::DEFAULT_RESOLUTION
                },
                "negative_prompt": {
                    "type": "string",
                    "description": "Optional negative prompt — content to avoid in the output. Supported by Pixverse, Kling, and similar; ignored by providers that do not support it."
                },
                "audio": {
                    "type": "boolean",
                    "description": "Optional audio generation toggle. Supported by Veo3 and Pixverse (affects pricing tier); ignored elsewhere."
                },
                "seed": {
                    "type": "integer",
                    "description": "Optional seed for reproducible outputs (provider-dependent)."
                },
                "model": {
                    "type": "string",
                    "description": "Optional model override. If omitted, the configured `[video_gen] model` is used. Models that the active provider does not know are rejected."
                }
            },
            "required": ["prompt"]
        }))
        .handler(|args, ctx| async move {
            Ok(video_generate_impl(&args, &ctx).await)
        })
        .toolset("video_gen")
        .emoji("🎬")
        .check_fn(video_generate_check)
        .build()
        .expect("video_generate builds")
}

async fn video_generate_impl(args: &Value, ctx: &ToolContext) -> Value {
    use crate::video_gen::{error_response, DEFAULT_ASPECT_RATIO, DEFAULT_RESOLUTION};

    let prompt = args
        .get("prompt")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let image_url = args
        .get("image_url")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let reference_image_urls = args
        .get("reference_image_urls")
        .and_then(normalize_reference_images);
    let duration = args.get("duration").map(coerce_int).unwrap_or(None);
    let aspect_ratio = args
        .get("aspect_ratio")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_ASPECT_RATIO.to_string());
    let resolution = args
        .get("resolution")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_RESOLUTION.to_string());
    let negative_prompt = args
        .get("negative_prompt")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let audio = args.get("audio").map(coerce_bool).unwrap_or(None);
    let seed = args.get("seed").map(coerce_int).unwrap_or(None);
    let model_override = args
        .get("model")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    // Soft validation — providers do their own. Prompt is required by the
    // schema; the surface always needs one.
    if prompt.is_empty() {
        return json!({"success": false, "error": "prompt is required for video generation"});
    }
    if args.get("operation").is_some() || args.get("video_url").is_some() {
        return json!({
            "success": false,
            "error": "video_generate only supports text-to-video, image-to-video, and reference-to-video; use a provider-specific tool for video edit/extend"
        });
    }

    // Resolve the active provider.
    let configured = ctx
        .config
        .video_gen
        .provider
        .as_deref()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let Some(provider) = crate::video_gen::get_active_provider(&ctx.config) else {
        return match configured {
            Some(name) => error_response(
                &format!(
                    "video_gen.provider='{}' is set but no backend registered that name.",
                    name
                ),
                "provider_not_registered",
                &name,
                "",
                "",
            ),
            None => error_response(
                "No video generation backend is configured. Set `[video_gen] provider` in config.toml to enable one.",
                "no_provider_configured",
                "",
                "",
                "",
            ),
        };
    };

    // Resolve model: explicit arg wins, then config, then provider default.
    let model = model_override
        .clone()
        .or_else(|| crate::video_gen::configured_model(&ctx.config))
        .unwrap_or_else(|| provider.default_model());

    let params = crate::video_gen::VideoGenParams {
        model: model.clone(),
        model_override_explicit: model_override.is_some(),
        image_url,
        reference_image_urls,
        duration,
        aspect_ratio,
        resolution,
        negative_prompt,
        audio,
        seed,
    };

    let result = provider.generate(&prompt, params).await;
    if result.is_object() {
        result
    } else {
        error_response(
            "Provider returned a non-object result",
            "provider_contract",
            provider.name(),
            &model,
            &prompt,
        )
    }
}

// ===========================================================================
// BFL FLUX 3 (hermes tools/flux3_video_tool.py)
// ===========================================================================

const VENDOR: &str = "bfl";

/// Submit sits behind the gateway's upstream call plus upload-reference
/// resolution, so it gets a generous read timeout; polls pass a much
/// shorter one.
const TRANSPORT_READ_TIMEOUT_SECONDS: u64 = 180;
const TRANSPORT_CONNECT_TIMEOUT_SECONDS: u64 = 10;
/// Wall-clock guarantee over the whole get_result handler (hermes
/// `_CALL_BACKSTOP_SECONDS`).
const CALL_BACKSTOP_SECONDS: f64 = 240.0;
/// Stop starting new looks earlier than the backstop (hermes
/// `_POLL_BUDGET_SECONDS`).
const POLL_BUDGET_SECONDS: f64 = 180.0;
/// Gap between looks (hermes `_POLL_GAP_SECONDS`).
const POLL_GAP_SECONDS: f64 = 5.0;
/// Waits are taken in slices so they stay answerable (hermes
/// `_POLL_WAIT_SLICE_SECONDS`).
const POLL_WAIT_SLICE_SECONDS: f64 = 1.0;
/// A poll's own read timeout (hermes `_POLL_READ_TIMEOUT_SECONDS`).
const POLL_READ_TIMEOUT_SECONDS: u64 = 60;
/// How many looks in a row may fail to reach the gateway before the loop
/// gives up on the call (hermes `_MAX_CONSECUTIVE_TRANSPORT_ERRORS`).
const MAX_CONSECUTIVE_TRANSPORT_ERRORS: u32 = 3;
/// The vendor takes at most ten keyframes (hermes `_MAX_IMAGES`).
const MAX_IMAGES: usize = 10;
/// A rejection page is a few hundred bytes of XML; a clip is megabytes
/// (hermes `_MIN_PLAUSIBLE_VIDEO_BYTES`).
const MIN_PLAUSIBLE_VIDEO_BYTES: u64 = 64 * 1024;
/// Enough collision suffixes to be useful, few enough to fail fast (hermes
/// `_MAX_FILENAME_ATTEMPTS`).
const MAX_FILENAME_ATTEMPTS: u32 = 50;
const DOWNLOAD_READ_TIMEOUT_SECONDS: u64 = 300;
const DOWNLOAD_GRACE_SECONDS: f64 = 5.0;

const SIGN_IN_MESSAGE: &str =
    "BFL video generation needs a Nous Portal sign-in. Ask the user to sign in to Nous \
     (auth.json `providers.nous` or TOOL_GATEWAY_USER_TOKEN), then retry.";

fn bfl_endpoints() -> Option<crate::managed_gateway::ManagedVendorEndpoints> {
    crate::managed_gateway::managed_vendor_endpoints(VENDOR)
}

/// True when a Nous bearer is on hand, without spending a refresh to learn
/// it (hermes `_has_nous_credential`).
fn has_nous_credential() -> bool {
    crate::managed_gateway::peek_nous_access_token().is_some()
}

/// Visible to anyone signed in to Nous; the gateway rules on the rest
/// (hermes `check_bfl_requirements`).
fn check_bfl_requirements() -> ToolAvailability {
    if bfl_endpoints().is_none() {
        return ToolAvailability::unavailable("BFL video gateway is not available in this build");
    }
    if has_nous_credential() {
        ToolAvailability::available()
    } else {
        ToolAvailability::unavailable("no Nous credential (sign in to Nous to enable BFL video)")
    }
}

fn error_payload(message: &str) -> Value {
    json!({"error": message})
}

// ---------------------------------------------------------------------------
// Local-path detection (hermes `_looks_like_local_path`)
// ---------------------------------------------------------------------------

fn looks_like_base64_payload(value: &str) -> bool {
    // Real filesystem paths are short; base64 of even a thumbnail runs to
    // thousands of characters.
    if value.len() < 256 {
        return false;
    }
    value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '\r' | '\n'))
}

fn looks_like_local_path(value: &str) -> bool {
    // Base64's alphabet includes "/", so an inline JPEG payload always
    // starts with "/9j/" — which reads as an absolute path unless caught
    // first.
    if looks_like_base64_payload(value) {
        return false;
    }
    if value.starts_with("file://") {
        return true;
    }
    if value == "~" || value.starts_with("~/") || value.starts_with("~\\") {
        return true;
    }
    if value.starts_with('/') || value.starts_with("./") || value.starts_with("../") {
        return true;
    }
    let bytes = value.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
    {
        return true;
    }
    value.starts_with("\\\\")
}

fn display_path(path: &str) -> String {
    if path.len() <= 200 {
        path.to_string()
    } else {
        let truncated: String = path.chars().take(200).collect();
        format!("{}… ({} characters)", truncated, path.len())
    }
}

// ---------------------------------------------------------------------------
// Gateway transport (hermes `_call_gateway`)
// ---------------------------------------------------------------------------

/// One REST round trip, rendered as this tool's result. The gateway's
/// `guidance` (on success) and `error.message` (on a refusal) are both
/// written for a model to act on, so they are surfaced verbatim.
async fn call_gateway(
    method: reqwest::Method,
    url: &str,
    body: Option<&Value>,
    read_timeout_seconds: Option<u64>,
) -> String {
    let Some(bearer) = crate::managed_gateway::managed_gateway_auth_bearer(url) else {
        return serde_json::to_string(&error_payload(SIGN_IN_MESSAGE)).unwrap_or_default();
    };

    let client = match reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(
            TRANSPORT_CONNECT_TIMEOUT_SECONDS,
        ))
        .timeout(std::time::Duration::from_secs(
            read_timeout_seconds.unwrap_or(TRANSPORT_READ_TIMEOUT_SECONDS),
        ))
        .build()
    {
        Ok(client) => client,
        Err(e) => {
            return serde_json::to_string(&json!({
                "error": format!("Could not build an HTTP client: {}", e),
                "transport_error": true,
            }))
            .unwrap_or_default()
        }
    };

    let mut request = client.request(method, url).bearer_auth(bearer);
    if let Some(body) = body {
        request = request.json(body);
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(e) => {
            return serde_json::to_string(&json!({
                "error": format!("Could not reach the video-generation gateway: {}", e),
                "transport_error": true,
            }))
            .unwrap_or_default()
        }
    };

    let status = response.status();
    if status == 401 {
        return serde_json::to_string(&json!({"error": SIGN_IN_MESSAGE, "needs_reauth": true}))
            .unwrap_or_default();
    }

    let payload: Option<Value> = response
        .json::<Value>()
        .await
        .ok()
        .filter(|v| v.is_object());
    let Some(mut payload) = payload else {
        // An edge or a proxy answering in HTML rather than the gateway
        // itself — what a 502/504 in front of it looks like from here.
        return serde_json::to_string(&json!({
            "error": format!(
                "The video-generation gateway answered HTTP {} with an unreadable body.",
                status
            ),
            "transport_error": true,
        }))
        .unwrap_or_default();
    };

    if status.as_u16() >= 400 {
        let error = payload
            .get("error")
            .filter(|e| e.is_object())
            .cloned()
            .unwrap_or(json!({}));
        let message = error
            .get("message")
            .and_then(|m| m.as_str())
            .map(|m| m.to_string())
            .unwrap_or_else(|| format!("the gateway refused the request (HTTP {})", status));
        let mut out = json!({"error": message});
        if let Some(details) = error.get("details").filter(|d| d.is_object()) {
            out["details"] = details.clone();
        }
        return serde_json::to_string(&out).unwrap_or_default();
    }

    let guidance = payload
        .as_object_mut()
        .and_then(|obj| obj.remove("guidance"));
    let result = guidance
        .filter(|g| !g.is_null())
        .unwrap_or(json!("Request accepted."));
    serde_json::to_string(&json!({"result": result, "details": payload})).unwrap_or_default()
}

fn is_transport_error(raw: &str) -> bool {
    serde_json::from_str::<Value>(raw)
        .ok()
        .and_then(|v| v.get("transport_error").and_then(|t| t.as_bool()))
        .unwrap_or(false)
}

/// Mirrors the gateway's BFL statuses (hermes `_TERMINAL_POLL_STATUSES`).
fn poll_is_finished(raw: &str) -> bool {
    let Ok(payload) = serde_json::from_str::<Value>(raw) else {
        return true;
    };
    if !payload.is_object() || payload.get("error").is_some() {
        return true;
    }
    let status = payload
        .get("details")
        .and_then(|d| d.get("status"))
        .and_then(|s| s.as_str());
    match status {
        None => true,
        Some(status) => matches!(
            status,
            "Ready" | "Error" | "Request Moderated" | "Content Moderated" | "Task not found"
        ),
    }
}

/// How long the gateway asked us to wait, when a refusal is a throttle
/// (hermes `_retry_after_seconds`).
fn retry_after_seconds(raw: &str) -> Option<f64> {
    let payload = serde_json::from_str::<Value>(raw).ok()?;
    if !payload.is_object() || payload.get("error").is_none() {
        return None;
    }
    let value = payload
        .get("details")
        .and_then(|d| d.get("retryAfterSeconds"))
        .filter(|v| !v.is_boolean())
        .and_then(|v| v.as_f64())?;
    if value <= 0.0 {
        None
    } else {
        Some(value)
    }
}

// ---------------------------------------------------------------------------
// Media delivery (hermes `_prepare_media` / `_deliver_media`)
// ---------------------------------------------------------------------------

fn mime_for_media(path: &Path, kind: &str) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    match kind {
        "video" => "video/mp4",
        _ => match ext.as_str() {
            "png" => "image/png",
            "webp" => "image/webp",
            "gif" => "image/gif",
            _ => "image/jpeg",
        },
    }
}

fn expand_tilde(value: &str) -> PathBuf {
    if let Some(rest) = value.strip_prefix("~/").map(|r| r.to_string()) {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    if value == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }
    PathBuf::from(value.strip_prefix("file://").unwrap_or(value))
}

/// Replace a local path with a `nous-upload:` reference; pass URLs through
/// (hermes `_deliver_media`).
async fn deliver_media(value: &str, kind: &str) -> std::result::Result<String, String> {
    if !looks_like_local_path(value) {
        return Ok(value.to_string());
    }
    let path = expand_tilde(value);
    let data = std::fs::read(&path)
        .map_err(|e| format!("Could not read {}: {}", display_path(value), e))?;
    let mime = mime_for_media(&path, kind);
    let Some(endpoints) = bfl_endpoints() else {
        return Err("BFL video generation is not available in this build.".to_string());
    };
    crate::managed_gateway::upload_managed_media(
        &endpoints.base_url,
        &endpoints.upload_path,
        &data,
        mime,
    )
    .await
    .map_err(|e| format!("Could not upload {}: {}", display_path(value), e))
}

/// Replace local paths with upload references in every media field (hermes
/// `_prepare_media`). Covers all fields rather than the one the mode
/// expects — `input_image` and `input_images` are interchangeable
/// server-side, so a value left unsanitized in the "wrong" field still
/// reaches the vendor.
async fn prepare_media(args: &Value) -> std::result::Result<Value, String> {
    let media_fields: [(&str, &str); 3] = [
        ("input_image", "image"),
        ("input_images", "image"),
        ("input_video", "video"),
    ];
    let mut prepared = args.clone();
    let obj = prepared
        .as_object_mut()
        .ok_or_else(|| "arguments must be an object".to_string())?;
    for (field, kind) in media_fields {
        let Some(value) = obj.get(field).cloned() else {
            continue;
        };
        match value {
            Value::Array(items) => {
                if items.len() > MAX_IMAGES {
                    return Err(format!(
                        "{} takes at most {} images; you passed {}.",
                        field,
                        MAX_IMAGES,
                        items.len()
                    ));
                }
                let mut uploaded = Vec::with_capacity(items.len());
                for item in &items {
                    let Some(text) = item.as_str() else {
                        return Err(format!("{} entries must be strings", field));
                    };
                    uploaded.push(Value::String(deliver_media(text, kind).await?));
                }
                obj.insert(field.to_string(), Value::Array(uploaded));
            }
            Value::String(text) => {
                obj.insert(
                    field.to_string(),
                    Value::String(deliver_media(&text, kind).await?),
                );
            }
            _ => {}
        }
    }
    Ok(prepared)
}

/// Drop media fields entirely — for the mode that takes none (hermes
/// `_without_media`).
fn without_media(args: &Value) -> Value {
    let mut out = json!({});
    if let Some(obj) = args.as_object() {
        for (key, value) in obj {
            if !matches!(key.as_str(), "input_image" | "input_images" | "input_video") {
                out[key] = value.clone();
            }
        }
    }
    out
}

fn submit_args(mode: &str, args: &Value) -> Value {
    let mut body = args.clone();
    if let Some(obj) = body.as_object_mut() {
        obj.retain(|_, v| !v.is_null());
        obj.insert("mode".to_string(), json!(mode));
    }
    body
}

async fn submit(mode: &str, args: &Value) -> Value {
    let Some(endpoints) = bfl_endpoints() else {
        return error_payload("BFL video generation is not available in this build.");
    };
    let body = submit_args(mode, args);
    let raw = call_gateway(
        reqwest::Method::POST,
        &format!("{}/generations", endpoints.base_url),
        Some(&body),
        None,
    )
    .await;
    serde_json::from_str(&raw).unwrap_or_else(|_| error_payload(&raw))
}

// ---------------------------------------------------------------------------
// Saving the finished clip (hermes `_save_if_ready` & friends)
// ---------------------------------------------------------------------------

fn filename_from_url(url: &str) -> String {
    let path = url::Url::parse(url)
        .map(|u| u.path().to_string())
        .unwrap_or_default();
    let last = path.rsplit('/').next().unwrap_or("");
    let decoded = percent_encoding_decode(last);
    let cleaned: String = decoded
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = cleaned
        .trim_start_matches('.')
        .chars()
        .take(120)
        .collect::<String>();
    if cleaned.is_empty() {
        "flux3-video.mp4".to_string()
    } else {
        cleaned
    }
}

fn percent_encoding_decode(text: &str) -> String {
    percent_encoding::percent_decode_str(text)
        .decode_utf8_lossy()
        .to_string()
}

fn default_directory() -> PathBuf {
    // No messaging surfaces in ulnclaw today — clips land in ~/Downloads
    // (hermes `_default_directory` non-attachment branch).
    if let Some(home) = dirs::home_dir() {
        let downloads = home.join("Downloads");
        if downloads.is_dir() {
            return downloads;
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// `name.mp4` -> `name-2.mp4` -> ... so nothing is clobbered (hermes
/// `_free_path`).
fn free_path(candidate: &Path) -> std::result::Result<PathBuf, String> {
    if !candidate.exists() {
        return Ok(candidate.to_path_buf());
    }
    let stem = candidate
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("video");
    let suffix = candidate
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    for n in 2..MAX_FILENAME_ATTEMPTS + 2 {
        let sibling = candidate.with_file_name(format!("{}-{}{}", stem, n, suffix));
        if !sibling.exists() {
            return Ok(sibling);
        }
    }
    Err(format!(
        "could not find a free filename next to {}",
        candidate.display()
    ))
}

/// Where to write, honouring an explicit request and never overwriting
/// (hermes `_resolve_destination`).
fn resolve_destination(
    save_to: Option<&str>,
    filename: &str,
) -> std::result::Result<PathBuf, String> {
    let (directory, name) = match save_to.map(|s| s.trim()).filter(|s| !s.is_empty()) {
        Some(requested) => {
            let requested = expand_tilde(requested);
            if requested.is_dir()
                || requested.to_string_lossy().ends_with('/')
                || requested.to_string_lossy().ends_with('\\')
            {
                (requested, filename.to_string())
            } else {
                let name = requested
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| filename.to_string());
                let parent = requested
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| PathBuf::from("."));
                (parent, name)
            }
        }
        None => (default_directory(), filename.to_string()),
    };
    std::fs::create_dir_all(&directory)
        .map_err(|e| format!("could not create {}: {}", directory.display(), e))?;
    free_path(&directory.join(name))
}

fn download_read_timeout(started: Instant) -> f64 {
    let left = CALL_BACKSTOP_SECONDS - started.elapsed().as_secs_f64() - DOWNLOAD_GRACE_SECONDS;
    left.max(0.0).min(DOWNLOAD_READ_TIMEOUT_SECONDS as f64)
}

/// Stream the clip to disk, returning (path, bytes) (hermes
/// `_download_video`). SSRF-guarded: this URL comes from the vendor by way
/// of the gateway and is fetched from the user's own machine.
async fn download_video(
    url: &str,
    save_to: Option<&str>,
    started: Instant,
) -> std::result::Result<(PathBuf, u64), String> {
    if !crate::url_safety::is_safe_url(url).await {
        return Err("the clip URL failed the SSRF safety check".to_string());
    }
    let target = resolve_destination(save_to, &filename_from_url(url))?;
    // Written under a .part name and renamed only once complete and
    // plausible, so a failed download never leaves something that looks
    // like a playable file behind.
    let partial = target.with_file_name(format!(
        "{}.part",
        target
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("video")
    ));

    let timeout_secs = download_read_timeout(started).max(1.0) as u64;
    let client =
        crate::url_safety::ssrf_guarded_client(std::time::Duration::from_secs(timeout_secs));
    let download = async {
        let response = client
            .get(url)
            .send()
            .await
            .map_err(|e| format!("download failed: {}", e))?
            .error_for_status()
            .map_err(|e| format!("download failed: {}", e))?;
        let bytes = response
            .bytes()
            .await
            .map_err(|e| format!("download failed: {}", e))?;
        std::fs::write(&partial, &bytes)
            .map_err(|e| format!("could not write {}: {}", partial.display(), e))?;
        Ok::<u64, String>(bytes.len() as u64)
    };
    let size = match download.await {
        Ok(size) => size,
        Err(e) => {
            let _ = std::fs::remove_file(&partial);
            return Err(e);
        }
    };
    if size < MIN_PLAUSIBLE_VIDEO_BYTES {
        let _ = std::fs::remove_file(&partial);
        return Err(format!(
            "the download returned only {} bytes, which is not a video",
            size
        ));
    }
    std::fs::rename(&partial, &target).map_err(|e| {
        let _ = std::fs::remove_file(&partial);
        format!("could not finalize {}: {}", target.display(), e)
    })?;
    Ok((target, size))
}

fn delivery_lead_in(target: &Path) -> String {
    // ulnclaw has no messaging surfaces today — the non-attachment branch
    // of hermes `_delivery_lead_in`.
    format!("Saved to {}. ", target.display())
}

/// Download a finished clip and swap the signed URL for a local path
/// (hermes `_save_if_ready`).
async fn save_if_ready(raw: &str, save_to: Option<&str>, started: Instant) -> String {
    let Ok(mut payload) = serde_json::from_str::<Value>(raw) else {
        return raw.to_string();
    };
    if !payload.is_object() {
        return raw.to_string();
    }
    let is_ready = payload
        .get("details")
        .and_then(|d| d.get("status"))
        .and_then(|s| s.as_str())
        == Some("Ready");
    if !is_ready {
        return raw.to_string();
    }
    let url = payload
        .get("details")
        .and_then(|d| d.get("result"))
        .and_then(|r| r.get("sample"))
        .and_then(|s| s.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let Some(url) = url else {
        return raw.to_string();
    };

    // Dropped whether or not the save succeeds: a retry re-polls, which
    // mints a fresh URL, so nothing is lost — and the signed URL never
    // enters the model's context.
    if let Some(result) = payload.get_mut("details").and_then(|d| d.get_mut("result")) {
        if let Some(obj) = result.as_object_mut() {
            obj.remove("sample");
        }
    }

    match download_video(&url, save_to, started).await {
        Ok((target, size)) => {
            if let Some(details) = payload.get_mut("details") {
                details["saved_path"] = json!(target.display().to_string());
                details["saved_bytes"] = json!(size);
            }
            let existing = payload
                .get("result")
                .and_then(|r| r.as_str())
                .unwrap_or("")
                .to_string();
            payload["result"] = json!(format!("{}{}", delivery_lead_in(&target), existing));
            serde_json::to_string(&payload).unwrap_or_else(|_| raw.to_string())
        }
        Err(e) => {
            payload["result"] = json!(format!(
                "The clip finished but saving it failed: {}. Poll this job again to retry the download; the job itself is unaffected.",
                e
            ));
            serde_json::to_string(&payload).unwrap_or_else(|_| raw.to_string())
        }
    }
}

fn still_generating(job_id: &str) -> Value {
    json!({
        "result": format!(
            "Still generating. This call reached its own time limit, which the job is \
             unaffected by — call bfl_flux3_get_result again with id={} to keep waiting.",
            job_id
        ),
        "details": {"id": job_id, "status": "Generating"}
    })
}

/// Look until the job settles, the budget runs out, or the loop is
/// interrupted (hermes `_poll_until_done`).
async fn poll_until_done(url: &str, save_to: Option<&str>, started: Instant) -> String {
    let mut spent = 0.0f64;
    let mut unanswered = 0u32;
    loop {
        let look_started = Instant::now();
        let raw = call_gateway(
            reqwest::Method::GET,
            url,
            None,
            Some(POLL_READ_TIMEOUT_SECONDS),
        )
        .await;
        spent += look_started.elapsed().as_secs_f64();

        let gap = if is_transport_error(&raw) {
            // The job is upstream and unaffected by our failure to ask
            // about it, so a blip costs this look and the loop tries again.
            unanswered += 1;
            if unanswered >= MAX_CONSECUTIVE_TRANSPORT_ERRORS {
                return raw;
            }
            POLL_GAP_SECONDS
        } else {
            unanswered = 0;
            let throttled_for = retry_after_seconds(&raw);
            if throttled_for.is_none() && poll_is_finished(&raw) {
                return save_if_ready(&raw, save_to, started).await;
            }
            // Never faster than our own cadence, however short a wait the
            // gateway names.
            throttled_for
                .map(|t| t.max(POLL_GAP_SECONDS))
                .unwrap_or(POLL_GAP_SECONDS)
        };

        if gap <= 0.0 || spent + gap >= POLL_BUDGET_SECONDS {
            return raw;
        }
        // Hold the call open until the next look, in slices.
        let mut remaining = gap;
        while remaining > 0.0 {
            let slice = POLL_WAIT_SLICE_SECONDS.min(remaining);
            tokio::time::sleep(std::time::Duration::from_secs_f64(slice)).await;
            remaining -= slice;
        }
        spent += gap;
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn handle_submit_mode(mode: &str, args: &Value, strip_media: bool) -> Value {
    let prepared = if strip_media {
        without_media(args)
    } else {
        match prepare_media(args).await {
            Ok(prepared) => prepared,
            Err(e) => return error_payload(&e),
        }
    };
    submit(mode, &prepared).await
}

fn flux3_check_wrapper() -> ToolAvailability {
    check_bfl_requirements()
}

async fn handle_get_result(args: &Value) -> Value {
    let job_id = args
        .get("id")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let Some(job_id) = job_id else {
        return error_payload("id is required: the job id returned by the generate tool.");
    };
    let Some(endpoints) = bfl_endpoints() else {
        return error_payload("BFL video generation is not available in this build.");
    };
    let save_to = args
        .get("save_to")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let encoded: String = url::form_urlencoded::byte_serialize(job_id.as_bytes()).collect();
    let url = format!("{}/generations/{}", endpoints.base_url, encoded);
    let started = Instant::now();

    // Wall-clock guarantee over the whole handler: whatever stalls inside,
    // the model is answered from here (hermes `asyncio.wait_for`
    // backstop).
    match tokio::time::timeout(
        std::time::Duration::from_secs_f64(CALL_BACKSTOP_SECONDS),
        poll_until_done(&url, save_to.as_deref(), started),
    )
    .await
    {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|_| error_payload(&raw)),
        Err(_) => still_generating(&job_id),
    }
}

// ---------------------------------------------------------------------------
// Pinned schemas (hermes TEXT_TO_VIDEO_SCHEMA & friends)
// ---------------------------------------------------------------------------

fn shared_submit_properties() -> Value {
    json!({
        "prompt": {
            "type": "string",
            "minLength": 1,
            "description": "Generation brief in plain prose (it is interpreted and expanded by a reasoning harness). Order it subject, distinguishing visual specifics, action, camera, lighting, environment, audio, style. Audio is generated by default — say \"no music\" if unwanted."
        },
        "aspect_ratio": {
            "type": "string",
            "enum": ["auto", "21:9", "16:9", "4:3", "1:1", "3:4", "9:16", "9:21"],
            "default": "auto",
            "description": "Output aspect ratio. \"auto\" lets the model choose."
        },
        "duration": {
            "oneOf": [
                {"type": "integer", "minimum": 5, "maximum": 20},
                {"type": "string", "const": "auto"}
            ],
            "default": "auto",
            "description": "Clip duration in whole seconds (5-20), or \"auto\"."
        },
        "resolution": {
            "type": "string",
            "enum": ["720p"],
            "default": "720p",
            "description": "Output resolution bin."
        },
        "generate_audio": {
            "type": "boolean",
            "default": true,
            "description": "Generate synchronized audio."
        },
        "grounding": {
            "type": "boolean",
            "default": true,
            "description": "Allow a short research pass before generation."
        },
        "seed": {
            "type": "integer",
            "minimum": 0,
            "maximum": 4294967295_u64,
            "description": "Optional reproducibility seed."
        },
        "version": {
            "type": "string",
            "description": "Model version pin. Defaults to \"latest\"."
        }
    })
}

fn with_extra_properties(extra: Value, required: Vec<&str>) -> Value {
    let mut properties = shared_submit_properties();
    if let (Some(base), Some(additions)) = (properties.as_object_mut(), extra.as_object()) {
        for (key, value) in additions {
            base.insert(key.clone(), value.clone());
        }
    }
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

const GUIDE_POINTER: &str = "Read bfl_flux3_prompting_guide before your first generation. ";
const MEDIA_SENTENCE: &str =
    "Media fields accept a local file path (uploaded automatically to Nous-managed temporary \
     storage and deleted when the generation finishes) or a URL. ";
const OVERRIDE_SENTENCE: &str = "All guidance is defaults: explicit user instructions override it.";

fn flux3_text_to_video_tool() -> crate::tools::Tool {
    tool("bfl_flux3_text_to_video")
        .description(&format!(
            "{}FLUX 3 text-to-video: generates a clip (with audio) from the prompt alone. Nothing \
             but the prompt anchors the subject here, so research anything with a real, checkable \
             appearance before writing it — whatever you leave unspecified is filled in for you. \
             Generation takes several minutes: this returns a job id immediately; poll \
             bfl_flux3_get_result. {}",
            GUIDE_POINTER, OVERRIDE_SENTENCE
        ))
        .parameters(json!({
            "type": "object",
            "properties": shared_submit_properties(),
            "required": ["prompt"],
            "additionalProperties": false
        }))
        .handler(
            |args, _ctx| async move { Ok(handle_submit_mode("text_to_video", &args, true).await) },
        )
        .toolset("bfl")
        .emoji("🎬")
        .check_fn(flux3_check_wrapper)
        .build()
        .expect("bfl_flux3_text_to_video builds")
}

fn flux3_image_to_video_tool() -> crate::tools::Tool {
    tool("bfl_flux3_image_to_video")
        .description(&format!(
            "{}FLUX 3 image-to-video: animates one image as the literal opening frame — those \
             pixels are frame 0 and the clip moves from there. {}Returns a job id; poll \
             bfl_flux3_get_result. {}",
            GUIDE_POINTER, MEDIA_SENTENCE, OVERRIDE_SENTENCE
        ))
        .parameters(with_extra_properties(
            json!({
                "input_image": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Exactly one opening-frame image: a local file path or a URL (PNG/JPEG/WebP, up to 10MB)."
                }
            }),
            vec!["prompt", "input_image"],
        ))
        .handler(|args, _ctx| async move {
            Ok(handle_submit_mode("image_to_video", &args, false).await)
        })
        .toolset("bfl")
        .emoji("🎬")
        .check_fn(flux3_check_wrapper)
        .build()
        .expect("bfl_flux3_image_to_video builds")
}

fn flux3_keyframes_to_video_tool() -> crate::tools::Tool {
    tool("bfl_flux3_keyframes_to_video")
        .description(&format!(
            "{}FLUX 3 keyframe video: a storyboard of 1-10 images pinned at chosen frame \
             positions (24fps). Name the subject and describe the motion that carries it between \
             the pins, and keep the subject consistent across every pinned image. {}Returns a job \
             id; poll bfl_flux3_get_result. {}",
            GUIDE_POINTER, MEDIA_SENTENCE, OVERRIDE_SENTENCE
        ))
        .parameters(with_extra_properties(
            json!({
                "input_images": {
                    "type": "array",
                    "items": {"type": "string", "minLength": 1},
                    "minItems": 1,
                    "maxItems": 10,
                    "description": "1-10 keyframe images: local file paths or URLs (PNG/JPEG/WebP, up to 10MB each)."
                },
                "keyframe_indices": {
                    "type": "array",
                    "items": {"type": "integer", "minimum": 0, "maximum": 480},
                    "minItems": 1,
                    "maxItems": 10,
                    "description": "One unique non-negative frame index per image (24fps). Each must be at most duration×24, so set an explicit duration rather than \"auto\" whenever you pin indices — \"auto\" resolves to 5, 10, 15 or 20 seconds and an index past the length it picks is rejected."
                }
            }),
            vec!["prompt", "input_images", "keyframe_indices"],
        ))
        .handler(|args, _ctx| async move {
            let images = args.get("input_images");
            if !images.map(|v| v.is_array() && v.as_array().map(|a| !a.is_empty()).unwrap_or(false)).unwrap_or(false) {
                return Ok(error_payload(
                    "input_images must be a non-empty list of 1-10 images (local paths or URLs).",
                ));
            }
            Ok(handle_submit_mode("keyframes_to_video", &args, false).await)
        })
        .toolset("bfl")
        .emoji("🎬")
        .check_fn(flux3_check_wrapper)
        .build()
        .expect("bfl_flux3_keyframes_to_video builds")
}

fn flux3_video_continuation_tool() -> crate::tools::Tool {
    tool("bfl_flux3_video_continuation")
        .description(&format!(
            "{}FLUX 3 video continuation: the new generation picks up from the input clip's \
             final frames. Open the prompt with \"Continue this video from its final frames:\", \
             re-establish the subject and the moment it ended on, then describe what happens \
             next. input_video must be an mp4 of at most 50MB and 15 seconds, and the generated \
             segment tops out at 15s too — chain a second continuation for a longer sequence. \
             duration is the new segment only. {}Returns a job id; poll bfl_flux3_get_result. {}",
            GUIDE_POINTER, MEDIA_SENTENCE, OVERRIDE_SENTENCE
        ))
        .parameters(with_extra_properties(
            json!({
                "input_video": {
                    "type": "string",
                    "minLength": 1,
                    "description": "The clip to continue: a local file path or a URL. mp4 only, at most 50MB and 15 seconds."
                }
            }),
            vec!["prompt", "input_video"],
        ))
        .handler(|args, _ctx| async move {
            Ok(handle_submit_mode("video_continuation", &args, false).await)
        })
        .toolset("bfl")
        .emoji("🎬")
        .check_fn(flux3_check_wrapper)
        .build()
        .expect("bfl_flux3_video_continuation builds")
}

fn flux3_get_result_tool() -> crate::tools::Tool {
    tool("bfl_flux3_get_result")
        .description(
            "Poll a FLUX 3 video job by the job id a generate tool returned. Generation takes \
             minutes and a long Generating phase is normal. This call waits for you while the \
             job runs, so it may run for several minutes; if it returns still generating, just \
             call it again. Do not sleep between calls. On Ready the clip is downloaded for you \
             and the response gives its local path; your only remaining step is to deliver that \
             file as the response describes.",
        )
        .parameters(json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Job id from a previous bfl_flux3_* generate call."
                },
                "save_to": {
                    "type": "string",
                    "description": "Where to save the finished clip: a directory or a full file path. Set this only when the user asked for a particular location; the default is ~/Downloads. An existing file is never overwritten."
                }
            },
            "required": ["id"],
            "additionalProperties": false
        }))
        .handler(|args, _ctx| async move { Ok(handle_get_result(&args).await) })
        .toolset("bfl")
        .emoji("🎬")
        .check_fn(flux3_check_wrapper)
        .build()
        .expect("bfl_flux3_get_result builds")
}

fn flux3_prompting_guide_tool() -> crate::tools::Tool {
    tool("bfl_flux3_prompting_guide")
        .description(
            "Read this before your first FLUX 3 generation. The prompting and grounding guide: \
             how to research a subject so it renders as itself, how to assemble a prompt, which \
             generate tool fits, and how to save and deliver the finished clip. Takes no \
             arguments and spends no generation budget.",
        )
        .parameters(json!({"type": "object", "properties": {}, "additionalProperties": false}))
        .handler(|_args, _ctx| async move { Ok(json!(FLUX3_PROMPTING_GUIDE)) })
        .toolset("bfl")
        .emoji("📖")
        .check_fn(flux3_check_wrapper)
        .build()
        .expect("bfl_flux3_prompting_guide builds")
}

// ---------------------------------------------------------------------------
// Pinned prompting guide (methodology only — policy numbers arrive live in
// the gateway's tool responses, so they cannot drift; hermes
// FLUX3_PROMPTING_GUIDE)
// ---------------------------------------------------------------------------

const FLUX3_PROMPTING_GUIDE: &str = r#"# FLUX 3 video generation — how to get the best results

Everything here is guidance, not policy: the user's explicit instructions
always win. If they say skip the research, save somewhere specific, or deliver
differently — do it their way without arguing. Only the server's own validation
and limits are non-negotiable.

Start from the user's own wording. The prompt is rewritten by a reasoning
harness before anything is generated, so restyling it yourself just stacks a
second rewrite on top and gives their intent another chance to drift. There are
two reasons to add anything: grounding facts for a subject with a real,
checkable appearance, and the behaviours below, which they had no way to know
about. Absent those, send what they wrote.

## Grounding

Grounding is your job, and it is the highest-leverage step in the workflow.
What you specify is preserved; what you leave out is filled in for you. So
when a subject has a real, checkable appearance, research it first and put
what you found into the prompt — that is what makes the result yours rather
than an approximation of it.

Ground whenever someone who knows the subject could watch the clip and say
"that's not what that is": a named person or place, a landmark, a particular
vehicle or machine, anything technical, anything culturally specific, or a
period setting. Skip it for generics ("a dog on a beach") — there is no fact
to get wrong.

To ground: search for VISUAL references, where three good photographs beat any
amount of prose. If you can analyze images, analyze what you find. Then put
only what a camera could see into the prompt: silhouette and proportion,
materials and finish, specific colours, distinctive details, era-correct
context.

## How this model behaves

The prompt is read by a reasoning harness rather than a tag encoder, so keyword
tricks and word order do nothing. Write plain prose at whatever length the
brief deserves; a single line is a valid prompt.

Audio is generated by default, whether or not you mention it, so leaving it out
gets you invented sound rather than silence. Name the ambient sound, the music
and any speech separately — each lands as its own layer — and say "no music"
when you do not want it.

A quoted line becomes speech only if a speaker is visible on camera. Without
one it tends to render as burned-in text instead, so quote the line, describe
the speaker, and add "no on-screen text, no subtitles".

Multi-shot sequences work inside a single generation: "SHOT ONE ... HARD CUT.
SHOT TWO ..." produces real cuts, and "one continuous unbroken shot" gets an
uncut take. Consecutive shots have to contrast in scale, location or colour or
the cut will not read as one — near-identical coverage blends back into a
continuous take.

## Choosing a tool

- No input media -> bfl_flux3_text_to_video.
- Animate one image as the literal opening frame ->
  bfl_flux3_image_to_video (those pixels are frame 0, and the clip moves from
  there). Name the subject and its distinguishing specifics here as you would
  anywhere else: only frame 0 is pinned, every frame after it is generated,
  and what you leave out is the model's call rather than yours.
- Several images pinned at chosen moments -> bfl_flux3_keyframes_to_video
  (name the subject and describe the motion that carries it between the pins;
  keep it consistent across every pin, since a mismatch becomes a visible
  morph mid-clip). Pin exact indices and you want an explicit duration too —
  every index must fall within duration×24.
- Keep going from where a clip ends, or chain segments ->
  bfl_flux3_video_continuation. Open with "Continue this video from its final
  frames:", re-establish the subject and the moment the clip ended on, then
  describe what happens next; duration is the new segment only, so plan
  segments of 15 seconds or less to feed outputs back in.

## Media inputs

Pass local file paths directly — never hand-encode file contents into an argument.
URLs also work. Limits per file:
images 10MB (PNG/JPEG/WebP), video one mp4 of at most 50MB and 15 seconds.
Do not pre-shrink files to fit imagined caps; oversized pixel dimensions are
auto-downscaled and output tops out at 720p.

## Workflow

Submit returns a job id immediately — the video does not exist yet. Poll
bfl_flux3_get_result with that id; generation takes several minutes and a long
Generating phase is normal, not a stall. Nothing reaches disk before the job is
Ready, so checking folders mid-run tells you nothing.

The waiting is not yours to do. bfl_flux3_get_result takes the pause itself
while a job is still running, so one call can occupy several minutes and comes
back within seconds of the job finishing. If it returns still generating, just
call it again — no sleeping, no interval to judge, nothing to time.

A job survives client restarts: re-poll the same id rather than resubmitting,
which would only spend your budgets on duplicate work.

## Save and deliver

bfl_flux3_get_result saves the clip itself and returns saved_path. The download
is not yours to do and no URL is handed to you to fetch. Pass save_to only when
the user named a location; otherwise it lands in ~/Downloads, and an existing
file is never overwritten. If saving fails the response says so — poll the same
job again to retry, which is safe and spends no generation budget.

Then deliver that file so the clip plays inline. Which markup plays inline is the
host's decision, so check your system prompt or platform instructions and use
exactly the form they give; the common ones are a MEDIA: tag alone on its own
line and a markdown embed. Two things break it. Write the real absolute path,
with ~ expanded. And keep the markup plain: wrapping it in bold, backticks or a
code fence, or rewriting it as a [link](path), turns an inline player into
literal text or a click-target. That is the most common way this step fails,
and it reads as success because the filename is on screen. Where the
instructions call for no delivery markup, or the host has no such mechanism,
follow them and state the absolute path in plain text.

Report what you did rather than what the job says it did: the echoed prompt
field describes intent and often overstates what the render preserved.
"#;

// ===========================================================================
// xai_video_edit / xai_video_extend (hermes tools/xai_video_tools.py)
// ===========================================================================

fn configured_for_xai_video() -> bool {
    crate::config::UlncLawConfig::load(None)
        .map(|cfg| {
            cfg.video_gen
                .provider
                .as_deref()
                .map(|s| s.trim() == "xai")
                .unwrap_or(false)
        })
        .unwrap_or(false)
}

fn xai_video_tools_check() -> ToolAvailability {
    // hermes _check_xai_video_requirements: configured provider + creds.
    if configured_for_xai_video() && crate::video_gen_xai::has_xai_video_credentials() {
        ToolAvailability::available()
    } else {
        ToolAvailability::unavailable(
            "xAI video edit/extend require [video_gen] provider = 'xai' and xAI credentials",
        )
    }
}

/// Require a public HTTP(S) MP4 URL (hermes `_normalize_public_video_url`).
fn normalize_public_video_url(value: Option<&Value>) -> Option<String> {
    let cleaned = value
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())?;
    if cleaned.is_empty() {
        return None;
    }
    let lower = cleaned.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        Some(cleaned)
    } else {
        None
    }
}

fn xai_provider_not_configured_error() -> Value {
    json!({
        "success": false,
        "error": "xAI video edit/extend tools require [video_gen] provider = 'xai' in config.toml.",
        "error_type": "provider_not_configured",
        "provider": "xai"
    })
}

fn xai_video_edit_tool() -> crate::tools::Tool {
    tool("xai_video_edit")
        .description(
            "Edit an existing video with xAI Imagine. This is separate from `video_generate` \
             because video editing is provider-specific. `video_url` must be the public HTTPS \
             MP4 URL from a prior Imagine result (`video` or `public_url` on files-cdn).",
        )
        .parameters(json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "Instruction for how xAI should modify the source video."
                },
                "video_url": {
                    "type": "string",
                    "description": "Public HTTPS MP4 URL of the source video — the `video` or `public_url` from a prior xAI Imagine result."
                },
                "model": {
                    "type": "string",
                    "description": "Optional xAI Imagine model override."
                }
            },
            "required": ["prompt", "video_url"]
        }))
        .handler(|args, _ctx| async move {
            let prompt = args
                .get("prompt")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            let video_url = normalize_public_video_url(args.get("video_url"));
            let model = args
                .get("model")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());

            let Some(prompt) = prompt else {
                return Ok(json!({"success": false, "error": "prompt is required for xAI video edit"}));
            };
            let Some(video_url) = video_url else {
                return Ok(json!({
                    "success": false,
                    "error": "video_url must be a public HTTPS MP4 URL (the `video`/`public_url` from a prior Imagine result)"
                }));
            };
            if !configured_for_xai_video() {
                return Ok(xai_provider_not_configured_error());
            }
            Ok(crate::video_gen_xai::run_xai_video_edit(&prompt, &video_url, model.as_deref()).await)
        })
        .toolset("video_gen")
        .emoji("🎬")
        .check_fn(xai_video_tools_check)
        .build()
        .expect("xai_video_edit builds")
}

fn xai_video_extend_tool() -> crate::tools::Tool {
    tool("xai_video_extend")
        .description(
            "Extend an existing video with xAI Imagine. This is separate from `video_generate` \
             because video extension is provider-specific. `video_url` must be the public \
             HTTPS MP4 URL from a prior Imagine result (`video` or `public_url` on files-cdn).",
        )
        .parameters(json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "Instruction for how xAI should continue the source video."
                },
                "video_url": {
                    "type": "string",
                    "description": "Public HTTPS MP4 URL of the source video — the `video` or `public_url` from a prior xAI Imagine result."
                },
                "duration": {
                    "type": "integer",
                    "description": "Desired extension duration in seconds. xAI clamps this to its supported range."
                },
                "model": {
                    "type": "string",
                    "description": "Optional xAI Imagine model override."
                }
            },
            "required": ["prompt", "video_url"]
        }))
        .handler(|args, _ctx| async move {
            let prompt = args
                .get("prompt")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            let video_url = normalize_public_video_url(args.get("video_url"));
            let model = args
                .get("model")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            let duration = args.get("duration").map(coerce_int).unwrap_or(None);

            let Some(prompt) = prompt else {
                return Ok(json!({"success": false, "error": "prompt is required for xAI video extend"}));
            };
            let Some(video_url) = video_url else {
                return Ok(json!({
                    "success": false,
                    "error": "video_url must be a public HTTPS MP4 URL (the `video`/`public_url` from a prior Imagine result)"
                }));
            };
            if !configured_for_xai_video() {
                return Ok(xai_provider_not_configured_error());
            }
            Ok(crate::video_gen_xai::run_xai_video_extend(&prompt, &video_url, duration, model.as_deref()).await)
        })
        .toolset("video_gen")
        .emoji("🎬")
        .check_fn(xai_video_tools_check)
        .build()
        .expect("xai_video_extend builds")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_path_detection() {
        assert!(looks_like_local_path("/home/user/clip.mp4"));
        assert!(looks_like_local_path("./rel.png"));
        assert!(looks_like_local_path("../up.png"));
        assert!(looks_like_local_path("~/Downloads/x.mp4"));
        assert!(looks_like_local_path("file:///tmp/x.png"));
        assert!(looks_like_local_path("C:\\Users\\me\\x.png"));
        assert!(!looks_like_local_path("https://example.com/x.png"));
        assert!(!looks_like_local_path("opaque-id-123"));
        // Base64 payloads starting with "/9j/" are not paths.
        let payload = format!("/9j/{}", "A".repeat(300));
        assert!(!looks_like_local_path(&payload));
    }

    #[test]
    fn poll_finish_detection() {
        assert!(poll_is_finished("not json"));
        assert!(poll_is_finished(r#"{"error": "denied"}"#));
        assert!(poll_is_finished(r#"{"details": {"status": "Ready"}}"#));
        assert!(poll_is_finished(
            r#"{"details": {"status": "Content Moderated"}}"#
        ));
        assert!(!poll_is_finished(
            r#"{"details": {"status": "Generating"}}"#
        ));
        // No details / no status means nothing is pending — finished
        // (hermes `_poll_is_finished`).
        assert!(poll_is_finished(r#"{"result": "ok"}"#));
        assert!(poll_is_finished(
            r#"{"result": "queued", "details": {"id": "x"}}"#
        ));
    }

    #[test]
    fn retry_after_parsing() {
        assert_eq!(
            retry_after_seconds(r#"{"error": "slow down", "details": {"retryAfterSeconds": 7}}"#),
            Some(7.0)
        );
        assert_eq!(
            retry_after_seconds(r#"{"error": "x", "details": {}}"#),
            None
        );
        assert_eq!(retry_after_seconds(r#"{"result": "ok"}"#), None);
    }

    #[test]
    fn filename_sanitization() {
        let name = filename_from_url("https://cdn.example.com/a%20b/clip:v1.mp4?sig=x");
        assert_eq!(name, "clip_v1.mp4");
        let fallback = filename_from_url("https://cdn.example.com/");
        assert_eq!(fallback, "flux3-video.mp4");
    }

    #[test]
    fn submit_args_shape() {
        let args = json!({"prompt": "p", "duration": null, "seed": 3});
        let body = submit_args("text_to_video", &args);
        assert_eq!(body["mode"], json!("text_to_video"));
        assert_eq!(body["prompt"], json!("p"));
        assert_eq!(body["seed"], json!(3));
        assert!(body.get("duration").is_none());
    }

    #[test]
    fn without_media_drops_all_media_fields() {
        let args = json!({
            "prompt": "p",
            "input_image": "x.png",
            "input_images": ["a.png"],
            "input_video": "v.mp4"
        });
        let stripped = without_media(&args);
        assert_eq!(stripped, json!({"prompt": "p"}));
    }

    #[test]
    fn free_path_never_clobbers() {
        let dir = std::env::temp_dir().join(format!("ulnclaw-video-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let base = dir.join("clip.mp4");
        std::fs::write(&base, b"x").unwrap();
        let resolved = free_path(&base).unwrap();
        assert_eq!(resolved.file_name().unwrap(), "clip-2.mp4");
        std::fs::write(&resolved, b"x").unwrap();
        let resolved2 = free_path(&base).unwrap();
        assert_eq!(resolved2.file_name().unwrap(), "clip-3.mp4");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn destination_resolution() {
        let dir = std::env::temp_dir().join(format!("ulnclaw-video-dest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Directory target keeps the generated filename.
        let target = resolve_destination(Some(dir.to_str().unwrap()), "movie.mp4").unwrap();
        assert!(target.starts_with(&dir));
        assert_eq!(target.file_name().unwrap(), "movie.mp4");
        // Full-path target is honoured.
        let full = dir.join("named.mp4");
        let target = resolve_destination(Some(full.to_str().unwrap()), "movie.mp4").unwrap();
        assert_eq!(target, full);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn shared_schema_defaults() {
        let props = shared_submit_properties();
        assert_eq!(props["aspect_ratio"]["default"], json!("auto"));
        assert_eq!(props["resolution"]["default"], json!("720p"));
        assert_eq!(props["generate_audio"]["default"], json!(true));
    }
}

//! Media tools — vision_analyze, image_generate, text_to_speech
//!
//! Ports of hermes' vision/image/tts tools. vision_analyze routes through
//! the configured chat provider; image_generate/text_to_speech use OpenAI
//! REST endpoints and write artifacts under `<home>/images` / `<home>/audio`.

use crate::error::Result;
use crate::tools::{tool, ToolAvailability, ToolContext, ToolRegistry};
use serde_json::json;
use std::sync::Arc;

pub fn register(registry: &mut ToolRegistry) {
    registry.register(vision_analyze_tool());
    registry.register(video_analyze_tool());
    registry.register(image_generate_tool());
    registry.register(text_to_speech_tool());
    registry.register(transcribe_audio_tool());
}

fn openai_key() -> Option<String> {
    crate::config::get_env_value("OPENAI_API_KEY")
}

// ---------------------------------------------------------------------------
// vision_analyze
// ---------------------------------------------------------------------------

fn vision_analyze_tool() -> crate::tools::Tool {
    tool("vision_analyze")
        .description(
            "Analyze an image with the vision model. Provide a local file path or URL plus a \
             prompt describing what to extract (OCR, description, diagram reading, screenshot \
             analysis).",
        )
        .parameters(json!({
            "type": "object",
            "properties": {
                "image": {"type": "string", "description": "Path to a local image file or an http(s) URL"},
                "prompt": {"type": "string", "description": "What to extract or describe (default: detailed description)"}
            },
            "required": ["image"]
        }))
        .handler(|args, ctx| async move {
            let Some(image) = args.get("image").and_then(|v| v.as_str()) else {
                return Ok(json!({"success": false, "error": "vision_analyze: 'image' is required"}));
            };
            let prompt = args
                .get("prompt")
                .and_then(|v| v.as_str())
                .unwrap_or("Describe this image in detail.")
                .to_string();

            let image_url = if image.starts_with("http://") || image.starts_with("https://") {
                image.to_string()
            } else {
                let path = ctx.resolve_path(image);
                let bytes = match std::fs::read(&path) {
                    Ok(b) => b,
                    Err(e) => return Ok(json!({"success": false, "error": format!("read image: {}", e)})),
                };
                let mime = match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
                    "png" => "image/png",
                    "gif" => "image/gif",
                    "webp" => "image/webp",
                    _ => "image/jpeg",
                };
                let b64 = base64_engine::STANDARD.encode(&bytes);
                format!("data:{};base64,{}", mime, b64)
            };

            let Some(provider) = ctx.provider.clone() else {
                return Ok(json!({"success": false, "error": "vision_analyze: no provider wired into this run"}));
            };
            // Auxiliary model routing: [auxiliary.vision] override (hermes
            // resolve_vision_provider_client); falls back to the main provider.
            let provider = match crate::provider::auxiliary::resolve_aux_task(
                &ctx.config,
                crate::provider::auxiliary::TASK_VISION,
                provider.clone(),
            ) {
                Ok(aux) => aux.provider,
                Err(e) => {
                    tracing::warn!("auxiliary vision routing failed: {}; using main provider", e);
                    provider
                }
            };
            match provider.analyze_image(&prompt, &image_url).await {
                Ok(answer) => Ok(json!({"success": true, "analysis": answer})),
                Err(e) => Ok(json!({"success": false, "error": format!("vision provider: {}", e)})),
            }
        })
        .toolset("vision")
        .emoji("👁️")
        .build()
        .expect("vision_analyze builds")
}

// ---------------------------------------------------------------------------
// video_analyze (hermes vision_tools.video_analyze_tool)
// ---------------------------------------------------------------------------

/// Hermes `_VIDEO_MIME_TYPES` — extension → mime for inline video payloads.
fn video_mime_for(path: &std::path::Path) -> Option<&'static str> {
    match path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .as_deref()?
    {
        "mp4" => Some("video/mp4"),
        "webm" => Some("video/webm"),
        "mov" => Some("video/mov"),
        "avi" => Some("video/mp4"),
        "mkv" => Some("video/mp4"),
        "mpeg" => Some("video/mpeg"),
        "mpg" => Some("video/mpeg"),
        _ => None,
    }
}

const MAX_VIDEO_BASE64_BYTES: usize = 50 * 1024 * 1024; // hermes hard cap
const VIDEO_SIZE_WARN_BYTES: u64 = 20 * 1024 * 1024;

fn video_analyze_tool() -> crate::tools::Tool {
    tool("video_analyze")
        .description(
            "Analyze a video with the multimodal model. Provide a local file path or an              HTTP(S) URL plus a prompt describing what to extract (action description,              scene understanding, transcription, event detection). Videos are sent inline              as base64 (50 MB payload cap); large files may need trimming first.",
        )
        .parameters(json!({
            "type": "object",
            "properties": {
                "video_url": {
                    "type": "string",
                    "description": "Path to a local video file (file:// or bare path) or an http(s) URL"
                },
                "prompt": {
                    "type": "string",
                    "description": "What to extract or describe (default: detailed description of the video)"
                }
            },
            "required": ["video_url"]
        }))
        .handler(|args, ctx| async move {
            let Some(video_url) = args.get("video_url").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty()) else {
                return Ok(json!({"success": false, "error": "video_analyze: 'video_url' is required"}));
            };
            let prompt = args
                .get("prompt")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .unwrap_or("Describe this video in detail.")
                .to_string();

            // ── Resolve local path vs remote URL (hermes parity) ────────
            let mut temp_path: Option<std::path::PathBuf> = None;
            let source_path: std::path::PathBuf = if video_url.starts_with("http://")
                || video_url.starts_with("https://")
            {
                if !crate::url_safety::is_safe_url(video_url).await {
                    return Ok(json!({
                        "success": false,
                        "error": "Blocked: URL targets a private or internal network address"
                    }));
                }
                let cache_dir = ctx
                    .home
                    .join("cache")
                    .join("video")
                    .join("temp_video_files");
                if let Err(e) = std::fs::create_dir_all(&cache_dir) {
                    return Ok(json!({"success": false, "error": format!("create video cache dir: {e}")}));
                }
                let dest = cache_dir.join(format!("temp_video_{}.mp4", uuid::Uuid::new_v4()));
                let client = crate::url_safety::ssrf_guarded_client(std::time::Duration::from_secs(300));
                match client.get(video_url).send().await {
                    Ok(response) => {
                        if !response.status().is_success() {
                            return Ok(json!({
                                "success": false,
                                "error": format!("download video: HTTP {}", response.status())
                            }));
                        }
                        match response.bytes().await {
                            Ok(bytes) => {
                                if let Err(e) = std::fs::write(&dest, &bytes) {
                                    return Ok(json!({"success": false, "error": format!("save video: {e}")}));
                                }
                            }
                            Err(e) => {
                                return Ok(json!({"success": false, "error": format!("download video body: {e}")}))
                            }
                        }
                    }
                    Err(e) => {
                        return Ok(json!({"success": false, "error": format!("download video: {e}")}))
                    }
                }
                temp_path = Some(dest.clone());
                dest
            } else {
                let raw = video_url.strip_prefix("file://").unwrap_or(video_url);
                ctx.resolve_path(raw)
            };

            if !source_path.is_file() {
                return Ok(json!({
                    "success": false,
                    "error": "Invalid video source. Provide an HTTP/HTTPS URL or a valid local file path."
                }));
            }

            let result: serde_json::Value = loop {
                let Some(mime) = video_mime_for(&source_path) else {
                    let suffix = source_path
                        .extension()
                        .map(|e| e.to_string_lossy().to_string())
                        .unwrap_or_default();
                    break json!({
                        "success": false,
                        "error": format!(
                            "Unsupported video format: '{suffix}'. Supported: avi, mkv, mov, mp4, mpeg, mpg, webm"
                        )
                    });
                };

                let size = std::fs::metadata(&source_path).map(|m| m.len()).unwrap_or(0);
                if size > VIDEO_SIZE_WARN_BYTES {
                    tracing::warn!(
                        "Video is {:.1} MB — may be slow or rejected",
                        size as f64 / (1024.0 * 1024.0)
                    );
                }

                let bytes = match std::fs::read(&source_path) {
                    Ok(b) => b,
                    Err(e) => break json!({"success": false, "error": format!("read video: {e}")}),
                };
                let data_url = format!(
                    "data:{};base64,{}",
                    mime,
                    base64_engine::STANDARD.encode(&bytes)
                );
                if data_url.len() > MAX_VIDEO_BASE64_BYTES {
                    break json!({
                        "success": false,
                        "error": format!(
                            "Video too large for API: base64 payload is {:.1} MB (limit {} MB). Compress or trim the video and retry.",
                            data_url.len() as f64 / (1024.0 * 1024.0),
                            MAX_VIDEO_BASE64_BYTES / (1024 * 1024)
                        )
                    });
                }

                let Some(provider) = ctx.provider.clone() else {
                    break json!({"success": false, "error": "video_analyze: no provider wired into this run"});
                };
                // Auxiliary routing: [auxiliary.vision] override (hermes
                // video analysis uses the vision task), else main provider.
                let provider = match crate::provider::auxiliary::resolve_aux_task(
                    &ctx.config,
                    crate::provider::auxiliary::TASK_VISION,
                    provider.clone(),
                ) {
                    Ok(aux) => aux.provider,
                    Err(e) => {
                        tracing::warn!("auxiliary vision routing failed: {}; using main provider", e);
                        provider
                    }
                };

                let mut analysis = match provider.analyze_video(&prompt, &data_url).await {
                    Ok(answer) => answer,
                    Err(e) => break json!({"success": false, "error": format!("video provider: {e}")}),
                };
                if analysis.trim().is_empty() {
                    // Hermes retries once on an empty response.
                    tracing::warn!("Empty video response, retrying once");
                    analysis = provider
                        .analyze_video(&prompt, &data_url)
                        .await
                        .unwrap_or_default();
                }
                break json!({
                    "success": true,
                    "analysis": if analysis.trim().is_empty() {
                        "There was a problem with the request and the video could not be analyzed."
                    } else {
                        &analysis
                    },
                });
            };

            if let Some(temp) = temp_path {
                let _ = std::fs::remove_file(temp);
            }
            Ok(result)
        })
        .toolset("video")
        .emoji("🎬")
        .build()
        .expect("video_analyze builds")
}

// ---------------------------------------------------------------------------
// image_generate
// ---------------------------------------------------------------------------

fn check_image_gen() -> ToolAvailability {
    if openai_key().is_some() {
        ToolAvailability::available()
    } else {
        ToolAvailability::unavailable(
            "OPENAI_API_KEY not set (image generation needs an image API)",
        )
    }
}

fn image_generate_tool() -> crate::tools::Tool {
    tool("image_generate")
        .description(
            "Generate an image from a text prompt. Returns the local path of the saved PNG. \
             Use detailed prompts describing subject, style, lighting, and composition.",
        )
        .parameters(json!({
            "type": "object",
            "properties": {
                "prompt": {"type": "string", "description": "Text description of the image to generate"},
                "size": {"type": "string", "enum": ["1024x1024", "1024x1792", "1792x1024"], "description": "Output size (default 1024x1024)", "default": "1024x1024"},
                "filename": {"type": "string", "description": "Optional output filename (without directory)"}
            },
            "required": ["prompt"]
        }))
        .handler(|args, ctx| async move {
            let Some(prompt) = args.get("prompt").and_then(|v| v.as_str()) else {
                return Ok(json!({"success": false, "error": "image_generate: 'prompt' is required"}));
            };
            let size = args.get("size").and_then(|v| v.as_str()).unwrap_or("1024x1024");
            let Some(api_key) = openai_key() else {
                return Ok(json!({"success": false, "error": "OPENAI_API_KEY not configured"}));
            };
            let base = ctx.config.resolve_base_url();
            let client = reqwest::Client::new();
            let response = client
                .post(format!("{}/images/generations", base.trim_end_matches('/')))
                .bearer_auth(api_key)
                .json(&json!({
                    "model": crate::config::get_env_value("ULNCLAW_IMAGE_MODEL").unwrap_or_else(|| "dall-e-3".into()),
                    "prompt": prompt,
                    "n": 1,
                    "size": size,
                    "response_format": "b64_json",
                }))
                .send()
                .await;
            let response = match response {
                Ok(r) => r,
                Err(e) => return Ok(json!({"success": false, "error": format!("image API: {}", e)})),
            };
            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                return Ok(json!({"success": false, "error": format!("image API {}: {}", status, &body[..body.len().min(300)])}));
            }
            let body: serde_json::Value = match response.json().await {
                Ok(v) => v,
                Err(e) => return Ok(json!({"success": false, "error": format!("parse image response: {}", e)})),
            };
            let Some(b64) = body.pointer("/data/0/b64_json").and_then(|v| v.as_str()) else {
                // Some endpoints return a URL instead.
                if let Some(url) = body.pointer("/data/0/url").and_then(|v| v.as_str()) {
                    return Ok(json!({"success": true, "url": url, "note": "Provider returned a URL; download it if a local file is needed."}));
                }
                return Ok(json!({"success": false, "error": "image API returned no data"}));
            };
            let bytes = match base64_engine::STANDARD.decode(b64) {
                Ok(b) => b,
                Err(e) => return Ok(json!({"success": false, "error": format!("decode image: {}", e)})),
            };
            let dir = ctx.home.join("images");
            std::fs::create_dir_all(&dir).ok();
            let filename = args
                .get("filename")
                .and_then(|v| v.as_str())
                .map(|f| f.replace(['/', '\\'], "_"))
                .unwrap_or_else(|| format!("img-{}.png", &uuid::Uuid::new_v4().to_string()[..8]));
            let path = dir.join(filename);
            if let Err(e) = std::fs::write(&path, &bytes) {
                return Ok(json!({"success": false, "error": format!("save image: {}", e)}));
            }
            Ok(json!({
                "success": true,
                "path": path.display().to_string(),
                "bytes": bytes.len(),
            }))
        })
        .toolset("image_gen")
        .emoji("🎨")
        .check_fn(check_image_gen)
        .build()
        .expect("image_generate builds")
}

// ---------------------------------------------------------------------------
// text_to_speech
// ---------------------------------------------------------------------------

fn check_tts() -> ToolAvailability {
    if openai_key().is_some() || crate::config::get_env_value("ULNCLAW_TTS_ENDPOINT").is_some() {
        ToolAvailability::available()
    } else {
        ToolAvailability::unavailable(
            "no TTS backend configured (set OPENAI_API_KEY or ULNCLAW_TTS_ENDPOINT)",
        )
    }
}

fn text_to_speech_tool() -> crate::tools::Tool {
    tool("text_to_speech")
        .description(
            "Convert text to speech audio. Returns the local path of the saved audio file. \
             Use for voice replies, narration, or audio artifacts.",
        )
        .parameters(json!({
            "type": "object",
            "properties": {
                "text": {"type": "string", "description": "The text to synthesize"},
                "voice": {"type": "string", "description": "Voice name (default: alloy)", "default": "alloy"},
                "filename": {"type": "string", "description": "Optional output filename"}
            },
            "required": ["text"]
        }))
        .handler(|args, ctx| async move {
            let Some(text) = args.get("text").and_then(|v| v.as_str()) else {
                return Ok(json!({"success": false, "error": "text_to_speech: 'text' is required"}));
            };
            let voice = args.get("voice").and_then(|v| v.as_str()).unwrap_or("alloy");

            // Custom endpoint takes precedence (ULNCLAW_TTS_ENDPOINT + ULNCLAW_TTS_KEY).
            if let Some(endpoint) = crate::config::get_env_value("ULNCLAW_TTS_ENDPOINT") {
                let client = reqwest::Client::new();
                let mut request = client.post(&endpoint).json(&json!({"text": text, "voice": voice}));
                if let Some(key) = crate::config::get_env_value("ULNCLAW_TTS_KEY") {
                    request = request.bearer_auth(key);
                }
                let response = match request.send().await {
                    Ok(r) => r,
                    Err(e) => return Ok(json!({"success": false, "error": format!("TTS endpoint: {}", e)})),
                };
                if !response.status().is_success() {
                    return Ok(json!({"success": false, "error": format!("TTS endpoint returned {}", response.status())}));
                }
                let bytes = response.bytes().await.map_err(|e| crate::error::AgentError::tool(e.to_string()))?;
                return save_audio(&ctx, &bytes, args.get("filename").and_then(|v| v.as_str()), "mp3");
            }

            let Some(api_key) = openai_key() else {
                return Ok(json!({"success": false, "error": "no TTS backend configured"}));
            };
            let base = ctx.config.resolve_base_url();
            let client = reqwest::Client::new();
            let response = client
                .post(format!("{}/audio/speech", base.trim_end_matches('/')))
                .bearer_auth(api_key)
                .json(&json!({
                    "model": crate::config::get_env_value("ULNCLAW_TTS_MODEL").unwrap_or_else(|| "tts-1".into()),
                    "input": text,
                    "voice": voice,
                    "response_format": "mp3",
                }))
                .send()
                .await;
            let response = match response {
                Ok(r) => r,
                Err(e) => return Ok(json!({"success": false, "error": format!("TTS API: {}", e)})),
            };
            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                return Ok(json!({"success": false, "error": format!("TTS API {}: {}", status, &body[..body.len().min(300)])}));
            }
            let bytes = response.bytes().await.map_err(|e| crate::error::AgentError::tool(e.to_string()))?;
            save_audio(&ctx, &bytes, args.get("filename").and_then(|v| v.as_str()), "mp3")
        })
        .toolset("tts")
        .emoji("🔊")
        .check_fn(check_tts)
        .build()
        .expect("text_to_speech builds")
}

fn save_audio(
    ctx: &Arc<ToolContext>,
    bytes: &[u8],
    filename: Option<&str>,
    ext: &str,
) -> Result<serde_json::Value> {
    let dir = ctx.home.join("audio");
    std::fs::create_dir_all(&dir).ok();
    let filename = filename
        .map(|f| f.replace(['/', '\\'], "_"))
        .unwrap_or_else(|| format!("tts-{}.{}", &uuid::Uuid::new_v4().to_string()[..8], ext));
    let filename = if filename.contains('.') {
        filename
    } else {
        format!("{}.{}", filename, ext)
    };
    let path = dir.join(filename);
    std::fs::write(&path, bytes)
        .map_err(|e| crate::error::AgentError::tool(format!("save audio: {}", e)))?;
    Ok(json!({
        "success": true,
        "path": path.display().to_string(),
        "bytes": bytes.len(),
    }))
}

// ---------------------------------------------------------------------------
// transcribe_audio (hermes tools/transcription_tools.transcribe_audio)
// ---------------------------------------------------------------------------

fn check_stt() -> ToolAvailability {
    let config = crate::config::UlncLawConfig::load(None).unwrap_or_default();
    match crate::stt::provider_readiness(&config.stt) {
        Ok(()) => ToolAvailability::available(),
        Err(reason) => ToolAvailability::unavailable(&format!(
            "stt provider '{}' not ready: {}",
            config.stt.provider, reason
        )),
    }
}

fn transcribe_audio_tool() -> crate::tools::Tool {
    tool("transcribe_audio")
        .description(
            "Transcribe an audio file (voice note, recording) to text using the configured \
             STT provider ([stt] config: local_command/groq/openai/mistral/xai/elevenlabs/\
             deepinfra or a [stt.providers.<name>] command).",
        )
        .parameters(json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to the audio file"},
                "model": {"type": "string", "description": "Optional model override for the active provider"},
                "language": {"type": "string", "description": "Optional language hint override (e.g. en, es, zh)"}
            },
            "required": ["path"]
        }))
        .handler(|args, ctx| async move {
            let Some(path_arg) = args.get("path").and_then(|v| v.as_str()) else {
                return Ok(json!({"success": false, "error": "transcribe_audio: 'path' is required"}));
            };
            let path = ctx.resolve_path(path_arg);
            if !path.exists() {
                return Ok(json!({"success": false, "error": format!("audio file not found: {}", path.display())}));
            }
            let mut stt = ctx.config.stt.clone();
            crate::stt::apply_overrides(
                &mut stt,
                args.get("model").and_then(|v| v.as_str()),
                args.get("language").and_then(|v| v.as_str()),
            );
            let outcome = crate::stt::transcribe_audio(&stt, &path).await;
            let mut value = json!({
                "success": outcome.success,
                "transcript": outcome.transcript,
                "provider": outcome.provider,
            });
            if let Some(error) = outcome.error {
                value["error"] = json!(error);
            }
            Ok(value)
        })
        .toolset("stt")
        .emoji("🎙️")
        .check_fn(check_stt)
        .build()
        .expect("transcribe_audio builds")
}

/// base64 engine alias (uses reqwest's re-exported base64 when available;
/// falls back to a tiny local implementation).
mod base64_engine {
    pub struct Standard;
    pub const STANDARD: Standard = Standard;

    impl Standard {
        pub fn encode(&self, input: &[u8]) -> String {
            const ALPHABET: &[u8; 64] =
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let mut out = String::with_capacity((input.len() + 2) / 3 * 4);
            for chunk in input.chunks(3) {
                let b0 = chunk[0] as u32;
                let b1 = *chunk.get(1).unwrap_or(&0) as u32;
                let b2 = *chunk.get(2).unwrap_or(&0) as u32;
                let triple = (b0 << 16) | (b1 << 8) | b2;
                out.push(ALPHABET[((triple >> 18) & 0x3F) as usize] as char);
                out.push(ALPHABET[((triple >> 12) & 0x3F) as usize] as char);
                out.push(if chunk.len() > 1 {
                    ALPHABET[((triple >> 6) & 0x3F) as usize] as char
                } else {
                    '='
                });
                out.push(if chunk.len() > 2 {
                    ALPHABET[(triple & 0x3F) as usize] as char
                } else {
                    '='
                });
            }
            out
        }

        pub fn decode(&self, input: &str) -> std::result::Result<Vec<u8>, String> {
            let mut output = Vec::with_capacity(input.len() / 4 * 3);
            let mut buffer: u32 = 0;
            let mut bits = 0;
            for ch in input.chars() {
                if ch == '=' || ch.is_whitespace() {
                    continue;
                }
                let value = match ch {
                    'A'..='Z' => ch as u32 - 'A' as u32,
                    'a'..='z' => ch as u32 - 'a' as u32 + 26,
                    '0'..='9' => ch as u32 - '0' as u32 + 52,
                    '+' => 62,
                    '/' => 63,
                    other => return Err(format!("invalid base64 char: {}", other)),
                };
                buffer = (buffer << 6) | value;
                bits += 6;
                if bits >= 8 {
                    bits -= 8;
                    output.push(((buffer >> bits) & 0xFF) as u8);
                }
            }
            Ok(output)
        }
    }

    #[allow(dead_code)]
    pub trait Engine {
        fn encode(&self, input: &[u8]) -> String;
        fn decode(&self, input: &str) -> std::result::Result<Vec<u8>, String>;
    }

    impl Engine for Standard {
        fn encode(&self, input: &[u8]) -> String {
            Standard::encode(self, input)
        }
        fn decode(&self, input: &str) -> std::result::Result<Vec<u8>, String> {
            Standard::decode(self, input)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::base64_engine::STANDARD;
    use super::video_mime_for;

    #[test]
    fn test_base64_roundtrip() {
        let data = b"hello ulnclaw \xF0\x9F\xA6\x80";
        let encoded = STANDARD.encode(data);
        let decoded = STANDARD.decode(&encoded).unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn video_mime_mapping_matches_hermes() {
        use std::path::Path;
        assert_eq!(video_mime_for(Path::new("a.mp4")), Some("video/mp4"));
        assert_eq!(video_mime_for(Path::new("a.MP4")), Some("video/mp4"));
        assert_eq!(video_mime_for(Path::new("a.webm")), Some("video/webm"));
        assert_eq!(video_mime_for(Path::new("a.mov")), Some("video/mov"));
        assert_eq!(video_mime_for(Path::new("a.avi")), Some("video/mp4"));
        assert_eq!(video_mime_for(Path::new("a.mkv")), Some("video/mp4"));
        assert_eq!(video_mime_for(Path::new("a.mpeg")), Some("video/mpeg"));
        assert_eq!(video_mime_for(Path::new("a.mpg")), Some("video/mpeg"));
        assert_eq!(video_mime_for(Path::new("a.txt")), None);
        assert_eq!(video_mime_for(Path::new("noext")), None);
    }
}

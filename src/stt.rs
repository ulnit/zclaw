//! Audio speech-to-text pipeline — port of hermes `tools/transcription_tools.py`
//! (provider dispatch) + `gateway/run.py::_enrich_message_with_transcription`
//! (voice-note enrichment semantics) @ v2026.8.3.
//!
//! Built-in providers (hermes `BUILTIN_STT_PROVIDERS`): `local`,
//! `local_command`, `groq`, `openai`, `mistral`, `xai`, `elevenlabs`,
//! `deepinfra`. Built-ins always win — command providers can never shadow
//! them (hermes built-ins-always-win invariant). Custom command providers
//! live in `[stt.providers.<name>]` (canonical) with a legacy top-level
//! `[stt.<name>]` fallback; a block is command-typed when `type` is absent
//! or `"command"` and `command` is a non-empty string (hermes
//! `_is_command_stt_provider_config`).
//!
//! Known difference: hermes' `local` provider runs faster-whisper in-process
//! (Python). The static Rust binary cannot embed it, so `local` honours an
//! optional `stt.local.command` escape hatch and otherwise reports an error
//! that keeps the gateway's neutral "[voice message could not be transcribed
//! automatically…]" marker semantics.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Built-in provider names (hermes `BUILTIN_STT_PROVIDERS`). Command
/// providers registering these names are rejected.
pub const BUILTIN_STT_PROVIDERS: &[&str] = &[
    "local",
    "local_command",
    "groq",
    "openai",
    "mistral",
    "xai",
    "elevenlabs",
    "deepinfra",
];

fn default_provider() -> String {
    "local".into()
}
fn default_language() -> String {
    // hermes: defaults to "en" — Whisper auto-detection frequently
    // misidentifies short/accented clips.
    "en".into()
}
fn default_local_model() -> String {
    "base".into()
}
fn default_groq_model() -> String {
    "whisper-large-v3-turbo".into()
}
fn default_openai_model() -> String {
    "whisper-1".into()
}
fn default_mistral_model() -> String {
    "voxtral-mini-latest".into()
}
fn default_elevenlabs_model() -> String {
    "scribe_v2".into()
}

/// `[stt.local]` — hermes stt.local (faster-whisper knobs; kept for config
/// parity) plus the ulnclaw `command` escape hatch.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SttLocalConfig {
    pub model: String,
    pub language: String,
    pub initial_prompt: String,
    pub vad: bool,
    pub vad_min_silence_ms: u32,
    pub no_speech_prob_threshold: f32,
    pub logprob_threshold: f32,
    /// ulnclaw extension: run this command (stdout = transcript) instead of
    /// the unavailable faster-whisper runtime.
    pub command: String,
}

impl Default for SttLocalConfig {
    fn default() -> Self {
        Self {
            model: default_local_model(),
            language: String::new(),
            initial_prompt: String::new(),
            vad: true,
            vad_min_silence_ms: 500,
            no_speech_prob_threshold: 0.6,
            logprob_threshold: -1.0,
            command: String::new(),
        }
    }
}

/// A command-type provider block: `{ type?: "command", command: "..." }`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SttCommandBlock {
    #[serde(rename = "type")]
    pub provider_type: String,
    pub command: String,
}

/// Shared `{ model, language }` block (groq/openai/mistral/xai).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SttModelBlock {
    pub model: String,
    pub language: String,
}

impl Default for SttModelBlock {
    fn default() -> Self {
        Self {
            model: String::new(),
            language: String::new(),
        }
    }
}

/// `[stt.elevenlabs]` (hermes stt.elevenlabs).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SttElevenlabsConfig {
    pub model_id: String,
    pub language_code: String,
    pub tag_audio_events: bool,
    pub diarize: bool,
}

impl Default for SttElevenlabsConfig {
    fn default() -> Self {
        Self {
            model_id: default_elevenlabs_model(),
            language_code: String::new(),
            tag_audio_events: false,
            diarize: false,
        }
    }
}

/// `[stt.deepinfra]` (hermes stt.deepinfra; empty model = first stt-tagged
/// model from the live catalog).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SttDeepinfraConfig {
    pub model: String,
    pub base_url: String,
}

/// `[stt]` — speech-to-text pipeline settings (hermes config.yaml `stt:`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SttConfig {
    /// Master switch (hermes `stt.enabled`).
    pub enabled: bool,
    /// Echo transcripts back to the user as 🎙️ messages (hermes
    /// `stt.echo_transcripts`).
    pub echo_transcripts: bool,
    /// Active provider name (hermes `stt.provider`).
    pub provider: String,
    /// Global language hint applied to every provider unless a per-provider
    /// language overrides it (hermes `stt.language`, default "en").
    pub language: String,
    pub local: SttLocalConfig,
    pub local_command: SttCommandBlock,
    pub groq: SttModelBlock,
    pub openai: SttModelBlock,
    pub mistral: SttModelBlock,
    pub xai: SttModelBlock,
    pub elevenlabs: SttElevenlabsConfig,
    pub deepinfra: SttDeepinfraConfig,
    /// Canonical command-provider location: `[stt.providers.<name>]`.
    pub providers: HashMap<String, SttCommandBlock>,
    /// Legacy top-level command blocks (`[stt.<name>]`) collected via
    /// flatten; entries whose name collides with a typed field or a
    /// built-in are ignored (built-ins always win).
    #[serde(flatten)]
    pub legacy_providers: HashMap<String, toml::Value>,
}

impl Default for SttConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            echo_transcripts: true,
            provider: default_provider(),
            language: default_language(),
            local: SttLocalConfig::default(),
            local_command: SttCommandBlock::default(),
            groq: SttModelBlock {
                model: default_groq_model(),
                language: String::new(),
            },
            openai: SttModelBlock {
                model: default_openai_model(),
                language: String::new(),
            },
            mistral: SttModelBlock {
                model: default_mistral_model(),
                language: String::new(),
            },
            xai: SttModelBlock::default(),
            elevenlabs: SttElevenlabsConfig::default(),
            deepinfra: SttDeepinfraConfig::default(),
            providers: HashMap::new(),
            legacy_providers: HashMap::new(),
        }
    }
}

/// Transcription result envelope (hermes provider response contract).
#[derive(Debug, Clone)]
pub struct SttOutcome {
    pub success: bool,
    pub transcript: String,
    pub provider: String,
    pub error: Option<String>,
}

impl SttOutcome {
    fn ok(provider: &str, transcript: String) -> Self {
        Self {
            success: true,
            transcript,
            provider: provider.to_string(),
            error: None,
        }
    }

    fn fail(provider: &str, error: impl Into<String>) -> Self {
        Self {
            success: false,
            transcript: String::new(),
            provider: provider.to_string(),
            error: Some(error.into()),
        }
    }
}

/// Language resolution (hermes semantics): per-provider language wins;
/// otherwise the global hint; blank means auto-detect (omit the field).
pub fn resolve_language(global: &str, per_provider: &str) -> Option<String> {
    let per = per_provider.trim();
    if !per.is_empty() {
        return Some(per.to_string());
    }
    let g = global.trim();
    if g.is_empty() {
        None
    } else {
        Some(g.to_string())
    }
}

fn normalize_name(name: &str) -> String {
    name.trim().to_lowercase()
}

/// True when the block declares a command-type provider (hermes
/// `_is_command_stt_provider_config`): `type` optional and
/// case/space-insensitive (absent or normalizing to "command"), `command`
/// a non-empty string.
fn is_command_block(block: &SttCommandBlock) -> bool {
    let ptype = block.provider_type.trim().to_lowercase();
    if !ptype.is_empty() && ptype != "command" {
        return false;
    }
    !block.command.trim().is_empty()
}

fn legacy_command_block(value: &toml::Value) -> Option<String> {
    let table = value.as_table()?;
    let ptype = table
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_lowercase();
    if !ptype.is_empty() && ptype != "command" {
        return None;
    }
    let command = table.get("command")?.as_str()?.trim().to_string();
    if command.is_empty() {
        None
    } else {
        Some(command)
    }
}

/// Resolve a configured provider name to a command string (canonical
/// `[stt.providers.<name>]` first, then the legacy top-level fallback —
/// hermes `_get_named_stt_provider_config` order). Built-in names are
/// never resolved as command providers.
pub fn resolve_command_provider(stt: &SttConfig, name: &str) -> Option<String> {
    let key = normalize_name(name);
    if BUILTIN_STT_PROVIDERS.contains(&key.as_str()) {
        return None;
    }
    if let Some(block) = stt.providers.get(&key) {
        if is_command_block(block) {
            return Some(block.command.trim().to_string());
        }
    }
    // Legacy top-level `[stt.<name>]` fallback.
    if let Some(value) = stt.legacy_providers.get(&key) {
        return legacy_command_block(value);
    }
    None
}

/// The local-command escape hatch (hermes built-in `local_command`):
/// `stt.local_command.command` or the single-env-var
/// `ULNCLAW_LOCAL_STT_COMMAND` (legacy `HERMES_LOCAL_STT_COMMAND`).
pub fn local_command(stt: &SttConfig) -> Option<String> {
    if is_command_block(&stt.local_command) {
        return Some(stt.local_command.command.trim().to_string());
    }
    crate::config::get_env_value("ULNCLAW_LOCAL_STT_COMMAND")
        .or_else(|| crate::config::get_env_value("HERMES_LOCAL_STT_COMMAND"))
}

/// Run a command-type STT provider: `{file}` placeholders are replaced
/// with the quoted audio path; without a placeholder the path is appended.
/// Stdout (trimmed) is the transcript.
async fn run_command_stt(name: &str, command: &str, path: &Path) -> SttOutcome {
    let quoted = shell_quote(&path.display().to_string());
    let cmdline = if command.contains("{file}") {
        command.replace("{file}", &quoted)
    } else {
        format!("{} {}", command.trim(), quoted)
    };
    let child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(&cmdline)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn();
    let child = match child {
        Ok(c) => c,
        Err(e) => return SttOutcome::fail(name, format!("spawn stt command: {e}")),
    };
    let output = match tokio::time::timeout(
        std::time::Duration::from_secs(300),
        child.wait_with_output(),
    )
    .await
    {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return SttOutcome::fail(name, format!("stt command: {e}")),
        Err(_) => return SttOutcome::fail(name, "stt command timed out (300s)"),
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        return SttOutcome::fail(
            name,
            format!(
                "stt command exited with {}{}",
                output.status.code().unwrap_or(-1),
                if stderr.is_empty() {
                    String::new()
                } else {
                    format!(": {}", &stderr[..stderr.len().min(300)])
                }
            ),
        );
    }
    let transcript = String::from_utf8_lossy(&output.stdout).trim().to_string();
    SttOutcome::ok(name, transcript)
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Multipart upload transcription against an OpenAI-compatible
/// `/audio/transcriptions` endpoint.
async fn openai_compat_transcribe(
    name: &str,
    url: &str,
    api_key: &str,
    path: &Path,
    model: Option<&str>,
    language: Option<&str>,
) -> SttOutcome {
    let file_name = path
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_else(|| "audio.ogg".into());
    let bytes = match tokio::fs::read(path).await {
        Ok(b) => b,
        Err(e) => return SttOutcome::fail(name, format!("read audio: {e}")),
    };
    let mime = crate::media_cache::mime_for_ext(path);
    let part = match reqwest::multipart::Part::bytes(bytes)
        .file_name(file_name)
        .mime_str(&mime)
    {
        Ok(p) => p,
        Err(e) => return SttOutcome::fail(name, format!("multipart: {e}")),
    };
    let mut form = reqwest::multipart::Form::new().part("file", part);
    if let Some(model) = model.filter(|m| !m.trim().is_empty()) {
        form = form.text("model", model.to_string());
    }
    if let Some(language) = language {
        form = form.text("language", language.to_string());
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .unwrap_or_default();
    let response = client
        .post(url)
        .bearer_auth(api_key)
        .multipart(form)
        .send()
        .await;
    let response = match response {
        Ok(r) => r,
        Err(e) => return SttOutcome::fail(name, format!("{name} API: {e}")),
    };
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return SttOutcome::fail(
            name,
            format!("{name} API {status}: {}", &body[..body.len().min(300)]),
        );
    }
    let value: serde_json::Value = match response.json().await {
        Ok(v) => v,
        Err(e) => return SttOutcome::fail(name, format!("{name} response: {e}")),
    };
    let transcript = value
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    SttOutcome::ok(name, transcript)
}

/// DeepInfra model discovery: empty configured model → first stt-tagged
/// model from the live catalog (hermes "first stt-tagged model" rule,
/// approximated over the OpenAI-compatible model list).
async fn deepinfra_default_model(base: &str) -> Option<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .ok()?;
    let value: serde_json::Value = client
        .get(format!("{}/models", base.trim_end_matches('/')))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    let items = value.get("data")?.as_array()?;
    for item in items {
        let id = item.get("id")?.as_str()?.to_lowercase();
        if id.contains("whisper") || id.contains("scribe") || id.contains("stt") {
            return item
                .get("id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
        }
    }
    None
}

/// Dispatch one transcription through the configured provider (hermes
/// `transcribe_audio`). Never raises — every failure becomes an error
/// envelope (hermes provider contract).
pub async fn transcribe_audio(stt: &SttConfig, path: &Path) -> SttOutcome {
    let provider = normalize_name(&stt.provider);
    if provider.is_empty() {
        return SttOutcome::fail("(unset)", "stt.provider is empty");
    }
    if !path.exists() {
        return SttOutcome::fail(
            &provider,
            format!("audio file not found: {}", path.display()),
        );
    }
    match provider.as_str() {
        "local" => {
            // hermes runs faster-whisper in-process; the static binary has
            // no embedded whisper runtime. Honour the command escape hatch,
            // otherwise fail with an actionable message (the gateway turns
            // this into the neutral marker note).
            if !stt.local.command.trim().is_empty() {
                return run_command_stt("local", stt.local.command.trim(), path).await;
            }
            SttOutcome::fail(
                "local",
                "built-in 'local' STT needs faster-whisper (Python runtime), which the \
                 static ulnclaw binary does not embed — set stt.local.command, \
                 stt.local_command.command / ULNCLAW_LOCAL_STT_COMMAND, or a cloud \
                 provider (groq/openai/mistral/xai/elevenlabs/deepinfra)",
            )
        }
        "local_command" => match local_command(stt) {
            Some(command) => run_command_stt("local_command", &command, path).await,
            None => SttOutcome::fail(
                "local_command",
                "no local STT command configured (set stt.local_command.command or \
                 ULNCLAW_LOCAL_STT_COMMAND)",
            ),
        },
        "groq" => {
            let Some(key) = crate::config::get_env_value("GROQ_API_KEY") else {
                return SttOutcome::fail("groq", "GROQ_API_KEY not set");
            };
            let language = resolve_language(&stt.language, &stt.groq.language);
            openai_compat_transcribe(
                "groq",
                "https://api.groq.com/openai/v1/audio/transcriptions",
                &key,
                path,
                Some(&stt.groq.model),
                language.as_deref(),
            )
            .await
        }
        "openai" => {
            let Some(key) = crate::config::get_env_value("OPENAI_API_KEY") else {
                return SttOutcome::fail("openai", "OPENAI_API_KEY not set");
            };
            let base = crate::config::get_env_value("OPENAI_BASE_URL")
                .unwrap_or_else(|| "https://api.openai.com/v1".into());
            let url = format!("{}/audio/transcriptions", base.trim_end_matches('/'));
            let language = resolve_language(&stt.language, &stt.openai.language);
            openai_compat_transcribe(
                "openai",
                &url,
                &key,
                path,
                Some(&stt.openai.model),
                language.as_deref(),
            )
            .await
        }
        "mistral" => {
            let Some(key) = crate::config::get_env_value("MISTRAL_API_KEY") else {
                return SttOutcome::fail("mistral", "MISTRAL_API_KEY not set");
            };
            let language = resolve_language(&stt.language, &stt.mistral.language);
            openai_compat_transcribe(
                "mistral",
                "https://api.mistral.ai/v1/audio/transcriptions",
                &key,
                path,
                Some(&stt.mistral.model),
                language.as_deref(),
            )
            .await
        }
        "xai" => {
            let Some(key) = crate::config::get_env_value("XAI_API_KEY") else {
                return SttOutcome::fail("xai", "XAI_API_KEY not set");
            };
            let language = resolve_language(&stt.language, &stt.xai.language);
            openai_compat_transcribe(
                "xai",
                "https://api.x.ai/v1/audio/transcriptions",
                &key,
                path,
                Some(stt.xai.model.as_str()),
                language.as_deref(),
            )
            .await
        }
        "elevenlabs" => {
            let Some(key) = crate::config::get_env_value("ELEVENLABS_API_KEY") else {
                return SttOutcome::fail("elevenlabs", "ELEVENLABS_API_KEY not set");
            };
            let file_name = path
                .file_name()
                .map(|f| f.to_string_lossy().to_string())
                .unwrap_or_else(|| "audio.ogg".into());
            let bytes = match tokio::fs::read(path).await {
                Ok(b) => b,
                Err(e) => return SttOutcome::fail("elevenlabs", format!("read audio: {e}")),
            };
            let mime = crate::media_cache::mime_for_ext(path);
            let part = match reqwest::multipart::Part::bytes(bytes)
                .file_name(file_name)
                .mime_str(&mime)
            {
                Ok(p) => p,
                Err(e) => return SttOutcome::fail("elevenlabs", format!("multipart: {e}")),
            };
            let mut form = reqwest::multipart::Form::new()
                .part("file", part)
                .text("model_id", stt.elevenlabs.model_id.clone())
                .text(
                    "tag_audio_events",
                    stt.elevenlabs.tag_audio_events.to_string(),
                )
                .text("diarize", stt.elevenlabs.diarize.to_string());
            // hermes: language_code uses ISO-639-3 ("eng", "spa", …) — the
            // global BCP-47 hint only applies when the per-provider code is
            // set explicitly.
            if !stt.elevenlabs.language_code.trim().is_empty() {
                form = form.text(
                    "language_code",
                    stt.elevenlabs.language_code.trim().to_string(),
                );
            }
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(300))
                .build()
                .unwrap_or_default();
            let response = client
                .post("https://api.elevenlabs.io/v1/speech-to-text")
                .header("xi-api-key", key)
                .multipart(form)
                .send()
                .await;
            let response = match response {
                Ok(r) => r,
                Err(e) => return SttOutcome::fail("elevenlabs", format!("elevenlabs API: {e}")),
            };
            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                return SttOutcome::fail(
                    "elevenlabs",
                    format!("elevenlabs API {status}: {}", &body[..body.len().min(300)]),
                );
            }
            let value: serde_json::Value = match response.json().await {
                Ok(v) => v,
                Err(e) => {
                    return SttOutcome::fail("elevenlabs", format!("elevenlabs response: {e}"))
                }
            };
            let transcript = value
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            SttOutcome::ok("elevenlabs", transcript)
        }
        "deepinfra" => {
            let Some(key) = crate::config::get_env_value("DEEPINFRA_API_KEY") else {
                return SttOutcome::fail("deepinfra", "DEEPINFRA_API_KEY not set");
            };
            let base = if stt.deepinfra.base_url.trim().is_empty() {
                crate::config::get_env_value("DEEPINFRA_BASE_URL")
                    .unwrap_or_else(|| "https://api.deepinfra.com/v1/openai".into())
            } else {
                stt.deepinfra.base_url.trim().to_string()
            };
            let mut model = stt.deepinfra.model.trim().to_string();
            if model.is_empty() {
                match deepinfra_default_model(&base).await {
                    Some(m) => model = m,
                    None => {
                        return SttOutcome::fail(
                            "deepinfra",
                            "stt.deepinfra.model is empty and no stt-tagged model was \
                             discovered from the DeepInfra catalog",
                        )
                    }
                }
            }
            let url = format!("{}/audio/transcriptions", base.trim_end_matches('/'));
            openai_compat_transcribe("deepinfra", &url, &key, path, Some(&model), None).await
        }
        other => match resolve_command_provider(stt, other) {
            Some(command) => run_command_stt(other, &command, path).await,
            None => SttOutcome::fail(
                other,
                format!(
                    "unknown stt provider '{}' — built-ins: {}; or define [stt.providers.{}] \
                     with a command",
                    other,
                    BUILTIN_STT_PROVIDERS.join(", "),
                    other
                ),
            ),
        },
    }
}

/// Local recovery path when the configured provider fails (hermes
/// `transcribe_audio_local_fallback`): the local_command provider only.
pub async fn transcribe_audio_local_fallback(stt: &SttConfig, path: &Path) -> SttOutcome {
    match local_command(stt) {
        Some(command) => run_command_stt("local_command", &command, path).await,
        None => SttOutcome::fail("local_command", "no local STT fallback command configured"),
    }
}

/// Best-effort duration probe (hermes `_probe_audio_duration`): WAV header
/// parse first, then ffprobe. Returns `MM:SS` / `HH:MM:SS`.
pub fn probe_audio_duration(path: &Path) -> Option<String> {
    if let Some(secs) = wav_duration(path) {
        return Some(format_duration(secs));
    }
    let output = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let secs: f64 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .ok()?;
    Some(format_duration(secs))
}

fn wav_duration(path: &Path) -> Option<f64> {
    let data = std::fs::read(path).ok()?;
    if data.len() < 44 || &data[0..4] != b"RIFF" || &data[8..12] != b"WAVE" {
        return None;
    }
    let mut pos = 12usize;
    let mut byte_rate: Option<u32> = None;
    let mut data_size: Option<u32> = None;
    while pos + 8 <= data.len() {
        let chunk_id = &data[pos..pos + 4];
        let chunk_size =
            u32::from_le_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]])
                as usize;
        let body = pos + 8;
        if chunk_id == b"fmt " && body + 16 <= data.len() {
            byte_rate = Some(u32::from_le_bytes([
                data[body + 8],
                data[body + 9],
                data[body + 10],
                data[body + 11],
            ]));
        } else if chunk_id == b"data" {
            data_size = Some(chunk_size as u32);
        }
        pos = body + chunk_size + (chunk_size & 1);
    }
    let (byte_rate, data_size) = (byte_rate?, data_size?);
    if byte_rate == 0 {
        return None;
    }
    Some(data_size as f64 / byte_rate as f64)
}

fn format_duration(secs: f64) -> String {
    let total = secs.max(0.0).round() as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{:02}:{:02}:{:02}", h, m, s)
    } else {
        format!("{:02}:{:02}", m, s)
    }
}

/// True when a cached attachment should enter the automatic STT pipeline
/// (hermes `_event_media_is_stt_input`, mime-first variant: VOICE-typed
/// attachments arrive as audio/* in ulnclaw adapters).
pub fn attachment_is_stt_input(mime: &str) -> bool {
    mime.trim().to_lowercase().starts_with("audio/")
}

/// Placeholder used by some adapters for media-only messages (hermes
/// Discord empty-content placeholder); stripped when a prefix is built.
const EMPTY_CONTENT_PLACEHOLDER: &str = "(The user sent a message with no text content)";

/// Enrich a voice message with transcription notes (hermes
/// `_enrich_message_with_transcription`). Returns `(enriched_text,
/// successful_transcripts, transcribed_paths)` — transcripts in input
/// order for the 🎙️ echo, transcribed paths for attachment-note
/// de-duplication.
pub async fn enrich_message_with_transcription(
    stt: &SttConfig,
    user_text: &str,
    audio_paths: &[PathBuf],
) -> (String, Vec<String>, Vec<PathBuf>) {
    let mut seen = std::collections::HashSet::new();
    let audio_paths: Vec<&PathBuf> = audio_paths
        .iter()
        .filter(|p| seen.insert(p.to_path_buf()))
        .collect();

    if !stt.enabled {
        let mut notes = Vec::new();
        for path in &audio_paths {
            let abs = std::fs::canonicalize(path).unwrap_or_else(|_| (*path).to_path_buf());
            match probe_audio_duration(&abs) {
                Some(duration) => notes.push(format!(
                    "[The user sent a voice message: {} (duration: {})]",
                    abs.display(),
                    duration
                )),
                None => notes.push(format!(
                    "[The user sent a voice message: {}]",
                    abs.display()
                )),
            }
        }
        if notes.is_empty() {
            return (user_text.to_string(), Vec::new(), Vec::new());
        }
        let prefix = notes.join("\n\n");
        return (
            join_with_user_text(&prefix, user_text),
            Vec::new(),
            Vec::new(),
        );
    }

    let mut enriched_parts: Vec<String> = Vec::new();
    let mut successful_transcripts: Vec<String> = Vec::new();
    let mut transcribed_paths: Vec<PathBuf> = Vec::new();
    for path in &audio_paths {
        let mut result = transcribe_audio(stt, path).await;
        if !result.success {
            let fallback = transcribe_audio_local_fallback(stt, path).await;
            if fallback.success {
                tracing::info!(
                    "Configured STT failed for {}; recovered with local STT",
                    path.display()
                );
                result = fallback;
            }
        }
        let abs = std::fs::canonicalize(path).unwrap_or_else(|_| (*path).to_path_buf());
        if result.success {
            let transcript = result.transcript.trim().to_string();
            if transcript.is_empty() {
                // hermes #41603: empty transcript sentinel — never emit
                // empty quotes that make the agent reply to nothing.
                enriched_parts.push(
                    "[The user sent a voice message but it came through empty or \
                     inaudible — speech-to-text returned no words. Do not guess at \
                     the content; ask the user to resend or type it out.]"
                        .to_string(),
                );
                transcribed_paths.push(abs);
                continue;
            }
            successful_transcripts.push(transcript.clone());
            transcribed_paths.push(abs);
            // Plain quoted line (hermes: meta wordings made the LLM
            // volunteer commentary about voice mode).
            enriched_parts.push(format!("\"{}\"", transcript));
        } else {
            // Single, minimal, neutral marker — the cause is logged for
            // operator diagnosis but kept out of the LLM-visible prompt
            // (hermes prompt-poisoning guard).
            tracing::info!(
                "Voice transcription failed for {}: {}",
                path.display(),
                result.error.unwrap_or_else(|| "unknown error".into())
            );
            enriched_parts.push(format!(
                "[voice message could not be transcribed automatically; the audio is \
                 available at: {}]",
                abs.display()
            ));
        }
    }

    if enriched_parts.is_empty() {
        return (
            user_text.to_string(),
            successful_transcripts,
            transcribed_paths,
        );
    }
    let prefix = enriched_parts.join("\n\n");
    (
        join_with_user_text(&prefix, user_text),
        successful_transcripts,
        transcribed_paths,
    )
}

fn join_with_user_text(prefix: &str, user_text: &str) -> String {
    if user_text.trim() == EMPTY_CONTENT_PLACEHOLDER {
        return prefix.to_string();
    }
    if user_text.trim().is_empty() {
        return prefix.to_string();
    }
    format!("{}\n\n{}", prefix, user_text)
}

/// One echo line delivered back to the chat (hermes
/// `_echo_pending_stt_transcripts_once` format).
pub fn echo_line(transcript: &str) -> String {
    format!("🎙️ \"{}\"", transcript)
}

/// Provider availability summary for the `transcribe_audio` tool check:
/// Ok(()) when the configured provider could plausibly service a call.
pub fn provider_readiness(stt: &SttConfig) -> Result<(), String> {
    let provider = normalize_name(&stt.provider);
    match provider.as_str() {
        "local" => {
            if stt.local.command.trim().is_empty() {
                Err(
                    "provider 'local' needs stt.local.command (faster-whisper is not \
                     embedded in the static binary)"
                        .into(),
                )
            } else {
                Ok(())
            }
        }
        "local_command" => local_command(stt)
            .map(|_| ())
            .ok_or_else(|| "set stt.local_command.command or ULNCLAW_LOCAL_STT_COMMAND".into()),
        "groq" => require_key("GROQ_API_KEY"),
        "openai" => require_key("OPENAI_API_KEY"),
        "mistral" => require_key("MISTRAL_API_KEY"),
        "xai" => require_key("XAI_API_KEY"),
        "elevenlabs" => require_key("ELEVENLABS_API_KEY"),
        "deepinfra" => require_key("DEEPINFRA_API_KEY"),
        other => resolve_command_provider(stt, other)
            .map(|_| ())
            .ok_or_else(|| format!("unknown stt provider '{}'", other)),
    }
}

fn require_key(name: &str) -> Result<(), String> {
    if crate::config::get_env_value(name).is_some() {
        Ok(())
    } else {
        Err(format!("{} not set", name))
    }
}

/// Apply `transcribe_audio` tool-call overrides (hermes tool `model` /
/// `language` arguments) onto a cloned config before dispatch.
pub fn apply_overrides(stt: &mut SttConfig, model: Option<&str>, language: Option<&str>) {
    let provider = normalize_name(&stt.provider);
    if let Some(model) = model.map(str::trim).filter(|m| !m.is_empty()) {
        match provider.as_str() {
            "local" => stt.local.model = model.to_string(),
            "groq" => stt.groq.model = model.to_string(),
            "openai" => stt.openai.model = model.to_string(),
            "mistral" => stt.mistral.model = model.to_string(),
            "xai" => stt.xai.model = model.to_string(),
            "elevenlabs" => stt.elevenlabs.model_id = model.to_string(),
            "deepinfra" => stt.deepinfra.model = model.to_string(),
            _ => {}
        }
    }
    if let Some(language) = language.map(str::trim).filter(|l| !l.is_empty()) {
        match provider.as_str() {
            "local" => stt.local.language = language.to_string(),
            "groq" => stt.groq.language = language.to_string(),
            "openai" => stt.openai.language = language.to_string(),
            "mistral" => stt.mistral.language = language.to_string(),
            "xai" => stt.xai.language = language.to_string(),
            "elevenlabs" => stt.elevenlabs.language_code = language.to_string(),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_hermes_config() {
        let stt = SttConfig::default();
        assert!(stt.enabled);
        assert!(stt.echo_transcripts);
        assert_eq!(stt.provider, "local");
        assert_eq!(stt.language, "en");
        assert_eq!(stt.local.model, "base");
        assert!(stt.local.vad);
        assert_eq!(stt.local.vad_min_silence_ms, 500);
        assert_eq!(stt.groq.model, "whisper-large-v3-turbo");
        assert_eq!(stt.openai.model, "whisper-1");
        assert_eq!(stt.mistral.model, "voxtral-mini-latest");
        assert_eq!(stt.elevenlabs.model_id, "scribe_v2");
        assert!(!stt.elevenlabs.tag_audio_events);
        assert!(!stt.elevenlabs.diarize);
    }

    #[test]
    fn parses_full_stt_toml() {
        let stt: SttConfig = toml::from_str(
            r#"
            enabled = false
            echo_transcripts = false
            provider = "groq"
            language = "es"
            [groq]
            model = "whisper-large-v3"
            language = "ca"
            [providers.mycmd]
            command = "whisper-cli {file}"
            [legacycmd]
            command = "legacy-tool"
            "#,
        )
        .unwrap();
        assert!(!stt.enabled);
        assert!(!stt.echo_transcripts);
        assert_eq!(stt.provider, "groq");
        assert_eq!(stt.language, "es");
        assert_eq!(stt.groq.model, "whisper-large-v3");
        assert_eq!(stt.groq.language, "ca");
        assert_eq!(
            resolve_command_provider(&stt, "mycmd").as_deref(),
            Some("whisper-cli {file}")
        );
        assert_eq!(
            resolve_command_provider(&stt, "legacycmd").as_deref(),
            Some("legacy-tool")
        );
        // Built-in names are never resolved as command providers.
        assert_eq!(resolve_command_provider(&stt, "groq"), None);
    }

    #[test]
    fn legacy_block_rejects_non_command_type() {
        let stt: SttConfig = toml::from_str(
            r#"
            [weird]
            type = "http"
            command = "something"
            "#,
        )
        .unwrap();
        assert_eq!(resolve_command_provider(&stt, "weird"), None);
    }

    #[test]
    fn language_resolution_precedence() {
        assert_eq!(resolve_language("en", "ca"), Some("ca".into()));
        assert_eq!(resolve_language("en", ""), Some("en".into()));
        assert_eq!(resolve_language("", ""), None);
        assert_eq!(resolve_language("", "fr"), Some("fr".into()));
    }

    #[test]
    fn duration_formatting() {
        assert_eq!(format_duration(5.0), "00:05");
        assert_eq!(format_duration(65.0), "01:05");
        assert_eq!(format_duration(3671.0), "01:01:11");
    }

    #[test]
    fn wav_duration_parses_header() {
        // Build a minimal WAV: RIFF/WAVE + fmt (PCM mono 8kHz, byte_rate
        // 16000) + data chunk of 32000 bytes => 2.0 seconds.
        let mut wav: Vec<u8> = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&0u32.to_le_bytes()); // filled below
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&1u16.to_le_bytes()); // mono
        wav.extend_from_slice(&8000u32.to_le_bytes()); // sample rate
        wav.extend_from_slice(&16000u32.to_le_bytes()); // byte rate
        wav.extend_from_slice(&2u16.to_le_bytes()); // block align
        wav.extend_from_slice(&16u16.to_le_bytes()); // bits/sample
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&32000u32.to_le_bytes());
        wav.resize(wav.len() + 32000, 0);
        let riff_size = (wav.len() - 8) as u32;
        wav[4..8].copy_from_slice(&riff_size.to_le_bytes());

        let dir = std::env::temp_dir().join(format!("ulnclaw-stt-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("probe.wav");
        std::fs::write(&path, &wav).unwrap();
        let secs = wav_duration(&path).unwrap();
        assert!((secs - 2.0).abs() < 0.01, "got {}", secs);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn stt_input_gate_is_audio_mime() {
        assert!(attachment_is_stt_input("audio/ogg"));
        assert!(attachment_is_stt_input("AUDIO/mpeg"));
        assert!(!attachment_is_stt_input("video/mp4"));
        assert!(!attachment_is_stt_input("application/octet-stream"));
    }

    #[test]
    fn echo_line_format() {
        assert_eq!(echo_line("hello world"), "🎙️ \"hello world\"");
    }

    #[test]
    fn join_with_user_text_semantics() {
        assert_eq!(join_with_user_text("P", ""), "P");
        assert_eq!(join_with_user_text("P", EMPTY_CONTENT_PLACEHOLDER), "P");
        assert_eq!(join_with_user_text("P", "caption"), "P\n\ncaption");
    }

    #[tokio::test]
    async fn disabled_stt_produces_voice_note() {
        let mut stt = SttConfig::default();
        stt.enabled = false;
        let dir = std::env::temp_dir().join(format!("ulnclaw-stt-test2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("note.ogg");
        std::fs::write(&path, b"fake").unwrap();
        let (text, transcripts, paths) =
            enrich_message_with_transcription(&stt, "caption", &[path.clone()]).await;
        assert!(text.contains("[The user sent a voice message:"));
        assert!(text.ends_with("caption"));
        assert!(transcripts.is_empty());
        assert!(paths.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn command_provider_transcribes_and_enriches() {
        let mut stt = SttConfig::default();
        stt.provider = "shout".into();
        stt.providers.insert(
            "shout".into(),
            SttCommandBlock {
                provider_type: String::new(),
                command: "printf 'hello from stt'".into(),
            },
        );
        let dir = std::env::temp_dir().join(format!("ulnclaw-stt-test3-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("note.ogg");
        std::fs::write(&path, b"fake").unwrap();
        let (text, transcripts, paths) =
            enrich_message_with_transcription(&stt, "", &[path.clone()]).await;
        assert_eq!(transcripts, vec!["hello from stt".to_string()]);
        assert_eq!(text, "\"hello from stt\"");
        assert_eq!(paths.len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn failed_provider_emits_neutral_marker() {
        let mut stt = SttConfig::default();
        stt.provider = "shout".into();
        stt.providers.insert(
            "shout".into(),
            SttCommandBlock {
                provider_type: String::new(),
                command: "exit 3".into(),
            },
        );
        let dir = std::env::temp_dir().join(format!("ulnclaw-stt-test4-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("note.ogg");
        std::fs::write(&path, b"fake").unwrap();
        let (text, transcripts, _) =
            enrich_message_with_transcription(&stt, "hi", &[path.clone()]).await;
        assert!(text.starts_with(
            "[voice message could not be transcribed automatically; the audio is available at:"
        ));
        assert!(text.ends_with("hi"));
        assert!(transcripts.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn empty_transcript_sentinel() {
        let mut stt = SttConfig::default();
        stt.provider = "shout".into();
        stt.providers.insert(
            "shout".into(),
            SttCommandBlock {
                provider_type: String::new(),
                command: "printf ''".into(),
            },
        );
        let dir = std::env::temp_dir().join(format!("ulnclaw-stt-test5-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("note.ogg");
        std::fs::write(&path, b"fake").unwrap();
        let (text, transcripts, _) =
            enrich_message_with_transcription(&stt, "", &[path.clone()]).await;
        assert!(text.contains("empty or inaudible"));
        assert!(transcripts.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }
}

//! Text-to-speech pipeline — lean port of hermes `tts:` config +
//! `/api/audio/speak` @ v2026.8.3.
//!
//! Hermes ships a wide provider chain (edge, openai, elevenlabs,
//! minimax, xai, mistral, gemini, local piper/neutts/kittentts). The
//! static Rust binary ports the API-backed providers — **openai**
//! (`/v1/audio/speech`, OPENAI_API_KEY) and **elevenlabs**
//! (`/v1/text-to-speech/<voice>`, ELEVENLABS_API_KEY) — plus the free
//! **edge** provider: Microsoft's read-aloud websocket protocol with
//! the Sec-MS-GEC token exchange, no API key required (P349). Local
//! model providers (piper/neutts/kittentts) stay out of scope.

use futures::StreamExt;
use serde::{Deserialize, Serialize};

fn default_provider() -> String {
    "openai".into()
}
fn default_openai_model() -> String {
    "gpt-4o-mini-tts".into()
}
fn default_openai_voice() -> String {
    "alloy".into()
}
fn default_elevenlabs_voice_id() -> String {
    // hermes config_defaults: Adam.
    "pNInz6obpgDQGcFmaJgB".into()
}
fn default_elevenlabs_model_id() -> String {
    "eleven_multilingual_v2".into()
}

/// `[tts.openai]` (hermes tts.openai).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TtsOpenaiConfig {
    pub model: String,
    pub voice: String,
}

impl Default for TtsOpenaiConfig {
    fn default() -> Self {
        Self {
            model: default_openai_model(),
            voice: default_openai_voice(),
        }
    }
}

/// `[tts.elevenlabs]` (hermes tts.elevenlabs).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TtsElevenlabsConfig {
    pub voice_id: String,
    pub model_id: String,
}

impl Default for TtsElevenlabsConfig {
    fn default() -> Self {
        Self {
            voice_id: default_elevenlabs_voice_id(),
            model_id: default_elevenlabs_model_id(),
        }
    }
}

fn default_edge_voice() -> String {
    // hermes config_defaults: tts.edge.voice.
    "en-US-AriaNeural".into()
}

/// `[tts.edge]` (hermes tts.edge — the free Microsoft neural voices).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TtsEdgeConfig {
    pub voice: String,
}

impl Default for TtsEdgeConfig {
    fn default() -> Self {
        Self {
            voice: default_edge_voice(),
        }
    }
}

/// `[tts]` config block (hermes `tts:`). Provider choice: `edge`
/// (free), `openai` or `elevenlabs` (local providers out of scope —
/// see module docs).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TtsConfig {
    pub provider: String,
    pub openai: TtsOpenaiConfig,
    pub elevenlabs: TtsElevenlabsConfig,
    pub edge: TtsEdgeConfig,
}

impl Default for TtsConfig {
    fn default() -> Self {
        Self {
            provider: default_provider(),
            openai: TtsOpenaiConfig::default(),
            elevenlabs: TtsElevenlabsConfig::default(),
            edge: TtsEdgeConfig::default(),
        }
    }
}

/// Synthesized audio ready for a data URL.
#[derive(Debug)]
pub struct TtsOutput {
    pub bytes: Vec<u8>,
    pub mime: &'static str,
    pub provider: String,
}

/// Synthesize `text` with the configured provider.
pub async fn synthesize(tts: &TtsConfig, text: &str) -> Result<TtsOutput, String> {
    match tts.provider.as_str() {
        "openai" => synthesize_openai(&tts.openai, text).await,
        "elevenlabs" => synthesize_elevenlabs(&tts.elevenlabs, text).await,
        "edge" => synthesize_edge(&tts.edge, text).await,
        other => Err(format!(
            "unsupported tts provider '{other}' (expected edge, openai or elevenlabs)"
        )),
    }
}

async fn synthesize_openai(config: &TtsOpenaiConfig, text: &str) -> Result<TtsOutput, String> {
    // Custom-endpoint escape hatch shared with the agent-side
    // `text_to_speech` tool: ULNCLAW_TTS_ENDPOINT (+ optional
    // ULNCLAW_TTS_KEY) wins before OPENAI_API_KEY.
    if let Some(endpoint) = crate::config::get_env_value("ULNCLAW_TTS_ENDPOINT") {
        let mut request = reqwest::Client::new()
            .post(&endpoint)
            .timeout(std::time::Duration::from_secs(60))
            .json(&serde_json::json!({ "text": text, "voice": config.voice }));
        if let Some(key) = crate::config::get_env_value("ULNCLAW_TTS_KEY") {
            request = request.bearer_auth(key);
        }
        let response = request
            .send()
            .await
            .map_err(|e| format!("custom tts endpoint request failed: {e}"))?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(format!(
                "custom tts endpoint failed ({}): {}",
                status,
                truncate_for_error(&body)
            ));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|e| format!("custom tts endpoint body failed: {e}"))?
            .to_vec();
        return Ok(TtsOutput {
            bytes,
            mime: "audio/mpeg",
            provider: "openai".into(),
        });
    }
    let Some(key) = crate::config::get_env_value("OPENAI_API_KEY") else {
        return Err("OPENAI_API_KEY not set".into());
    };
    let base = crate::config::get_env_value("OPENAI_BASE_URL")
        .unwrap_or_else(|| "https://api.openai.com/v1".into());
    let url = format!("{}/audio/speech", base.trim_end_matches('/'));
    let response = reqwest::Client::new()
        .post(&url)
        .timeout(std::time::Duration::from_secs(60))
        .bearer_auth(&key)
        .json(&serde_json::json!({
            "model": config.model,
            "voice": config.voice,
            "input": text,
            "response_format": "mp3",
        }))
        .send()
        .await
        .map_err(|e| format!("openai tts request failed: {e}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!(
            "openai tts failed ({}): {}",
            status,
            truncate_for_error(&body)
        ));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|e| format!("openai tts body failed: {e}"))?
        .to_vec();
    Ok(TtsOutput {
        bytes,
        mime: "audio/mpeg",
        provider: "openai".into(),
    })
}

async fn synthesize_elevenlabs(
    config: &TtsElevenlabsConfig,
    text: &str,
) -> Result<TtsOutput, String> {
    let Some(key) = crate::config::get_env_value("ELEVENLABS_API_KEY") else {
        return Err("ELEVENLABS_API_KEY not set".into());
    };
    let url = format!(
        "https://api.elevenlabs.io/v1/text-to-speech/{}",
        urlencoding(&config.voice_id)
    );
    let response = reqwest::Client::new()
        .post(&url)
        .timeout(std::time::Duration::from_secs(60))
        .header("xi-api-key", &key)
        .json(&serde_json::json!({
            "text": text,
            "model_id": config.model_id,
        }))
        .send()
        .await
        .map_err(|e| format!("elevenlabs tts request failed: {e}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!(
            "elevenlabs tts failed ({}): {}",
            status,
            truncate_for_error(&body)
        ));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|e| format!("elevenlabs tts body failed: {e}"))
        .map(|b| b.to_vec())?;
    Ok(TtsOutput {
        bytes,
        mime: "audio/mpeg",
        provider: "elevenlabs".into(),
    })
}

// ── Streaming PCM (speak-stream WebSocket; hermes tts_streaming parity) ───

/// Streaming PCM provider sample rate (hermes
/// `StreamingTTSProvider.sample_rate`): 24 kHz mono int16, which both
/// OpenAI `response_format=pcm` and ElevenLabs `output_format=pcm_24000`
/// emit natively.
pub const STREAMING_PCM_SAMPLE_RATE: u32 = 24000;

/// Upper bound on the PCM bytes accepted from one provider stream for one
/// sentence (hermes `_STREAM_SENTENCE_BYTE_CAP`): a buggy or hostile
/// endpoint must not be able to feed us unbounded audio.
const STREAM_SENTENCE_BYTE_CAP: usize = 16 * 1024 * 1024;

/// Chunked PCM stream for one sentence: raw int16 little-endian mono at
/// [`STREAMING_PCM_SAMPLE_RATE`], chunks arriving as the provider
/// synthesizes them.
pub type PcmChunkStream =
    std::pin::Pin<Box<dyn futures::Stream<Item = Result<bytes::Bytes, String>> + Send>>;

/// Whether the configured provider has a chunked PCM API (hermes
/// `resolve_streaming_provider`: ElevenLabs pcm_24000 + OpenAI pcm).
/// `edge` (the free default) has no streamable API and a custom
/// `ULNCLAW_TTS_ENDPOINT` answers opaque audio, so both fall back — the
/// desktop speak-stream surface then degrades to `POST /api/audio/speak`.
pub fn has_streaming_provider(cfg: &TtsConfig) -> bool {
    match cfg.provider.as_str() {
        "openai" => {
            crate::config::get_env_value("ULNCLAW_TTS_ENDPOINT").is_none()
                && crate::config::get_env_value("OPENAI_API_KEY").is_some()
        }
        "elevenlabs" => crate::config::get_env_value("ELEVENLABS_API_KEY").is_some(),
        _ => false,
    }
}

/// Open a streaming PCM synthesis for one sentence of `text`; the returned
/// stream yields audio chunks the moment the provider produces them (hermes
/// `StreamingTTSProvider.stream` contract).
pub async fn open_pcm_stream(cfg: &TtsConfig, text: &str) -> Result<PcmChunkStream, String> {
    let response = match cfg.provider.as_str() {
        "openai" => open_openai_pcm_response(&cfg.openai, text).await?,
        "elevenlabs" => open_elevenlabs_pcm_response(&cfg.elevenlabs, text).await?,
        other => return Err(format!("provider '{other}' has no chunked pcm api")),
    };
    let mut seen = 0usize;
    let stream = response.bytes_stream().map(move |chunk| {
        let chunk = chunk.map_err(|e| format!("tts stream chunk failed: {e}"))?;
        seen += chunk.len();
        if seen > STREAM_SENTENCE_BYTE_CAP {
            return Err(format!(
                "tts stream exceeded the {} byte per-sentence cap",
                STREAM_SENTENCE_BYTE_CAP
            ));
        }
        Ok(chunk)
    });
    Ok(Box::pin(stream))
}

async fn open_openai_pcm_response(
    config: &TtsOpenaiConfig,
    text: &str,
) -> Result<reqwest::Response, String> {
    let Some(key) = crate::config::get_env_value("OPENAI_API_KEY") else {
        return Err("OPENAI_API_KEY not set".into());
    };
    let base = crate::config::get_env_value("OPENAI_BASE_URL")
        .unwrap_or_else(|| "https://api.openai.com/v1".into());
    let url = format!("{}/audio/speech", base.trim_end_matches('/'));
    let response = reqwest::Client::new()
        .post(&url)
        .timeout(std::time::Duration::from_secs(60))
        .bearer_auth(&key)
        .json(&serde_json::json!({
            "model": config.model,
            "voice": config.voice,
            "input": text,
            // 24 kHz mono int16, no header — the speak-stream wire format.
            "response_format": "pcm",
        }))
        .send()
        .await
        .map_err(|e| format!("openai tts stream request failed: {e}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!(
            "openai tts stream failed ({}): {}",
            status,
            truncate_for_error(&body)
        ));
    }
    Ok(response)
}

async fn open_elevenlabs_pcm_response(
    config: &TtsElevenlabsConfig,
    text: &str,
) -> Result<reqwest::Response, String> {
    let Some(key) = crate::config::get_env_value("ELEVENLABS_API_KEY") else {
        return Err("ELEVENLABS_API_KEY not set".into());
    };
    let url = format!(
        "https://api.elevenlabs.io/v1/text-to-speech/{}?output_format=pcm_24000",
        urlencoding(&config.voice_id)
    );
    let response = reqwest::Client::new()
        .post(&url)
        .timeout(std::time::Duration::from_secs(60))
        .header("xi-api-key", &key)
        .json(&serde_json::json!({
            "text": text,
            "model_id": config.model_id,
        }))
        .send()
        .await
        .map_err(|e| format!("elevenlabs tts stream request failed: {e}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!(
            "elevenlabs tts stream failed ({}): {}",
            status,
            truncate_for_error(&body)
        ));
    }
    Ok(response)
}

// ── edge (Microsoft read-aloud websocket protocol; P349) ──────────────────

/// Trusted client token embedded in every Microsoft Edge install — the
/// read-aloud feature's public websocket credential (edge-tts
/// convention; not a secret). Doubles as the Sec-MS-GEC hash salt.
const EDGE_TRUSTED_CLIENT_TOKEN: &str = "6A5AA1D4EAFF4E9FB37E23D68491D6F4";
const EDGE_SYNTH_URL: &str =
    "wss://speech.platform.bing.com/consumer/speech/synthesize/readaloud/edge/v1";
/// Chromium build the handshake emulates (edge-tts `SEC_MS_GEC_VERSION`;
/// Microsoft rejects stale builds).
const EDGE_CHROMIUM_VERSION: &str = "143.0.3650.75";
const EDGE_CHROMIUM_MAJOR: &str = "143";
const EDGE_ORIGIN: &str = "chrome-extension://jdiccldimpdaibmpdkjnbmckianbfold";
/// hermes config_defaults: Edge per-request input cap.
const EDGE_MAX_TEXT_CHARS: usize = 5_000;

/// `Sec-MS-GEC` token (edge-tts 7.x DRM): SHA-256 of "<windows filetime
/// rounded down to 5-minute windows><trusted client token>" in ASCII,
/// uppercase hex.
fn edge_sec_ms_gec(unix_secs: i64) -> String {
    use sha2::Digest;
    let mut seconds = unix_secs as i128 + 11_644_473_600;
    seconds -= seconds.rem_euclid(300);
    let filetime = seconds * 10_000_000;
    let payload = format!("{filetime}{EDGE_TRUSTED_CLIENT_TOKEN}");
    let digest = sha2::Sha256::digest(payload.as_bytes());
    digest.iter().map(|byte| format!("{byte:02X}")).collect()
}

/// edge-tts `X-Timestamp` format: `Fri Aug 08 2026 09:41:00 GMT+0000
/// (Coordinated Universal Time)`.
fn edge_http_date(now: chrono::DateTime<chrono::Utc>) -> String {
    use chrono::{Datelike, Timelike};
    let weekday = match now.weekday() {
        chrono::Weekday::Mon => "Mon",
        chrono::Weekday::Tue => "Tue",
        chrono::Weekday::Wed => "Wed",
        chrono::Weekday::Thu => "Thu",
        chrono::Weekday::Fri => "Fri",
        chrono::Weekday::Sat => "Sat",
        chrono::Weekday::Sun => "Sun",
    };
    let month = match now.month() {
        1 => "Jan",
        2 => "Feb",
        3 => "Mar",
        4 => "Apr",
        5 => "May",
        6 => "Jun",
        7 => "Jul",
        8 => "Aug",
        9 => "Sep",
        10 => "Oct",
        11 => "Nov",
        _ => "Dec",
    };
    format!(
        "{} {} {:02} {} {:02}:{:02}:{:02} GMT+0000 (Coordinated Universal Time)",
        weekday,
        month,
        now.day(),
        now.year(),
        now.hour(),
        now.minute(),
        now.second(),
    )
}

/// Install the process-level rustls crypto provider exactly once.
/// reqwest normally does this on first client build, but the edge
/// websocket may be the first TLS consumer (fresh process, tests).
fn ensure_rustls_provider() {
    static INSTALL: std::sync::Once = std::sync::Once::new();
    INSTALL.call_once(|| {
        let _ = rustls::crypto::CryptoProvider::install_default(
            rustls::crypto::aws_lc_rs::default_provider(),
        );
    });
}

/// Binary frames are `<2-byte big-endian header length><headers><payload>`;
/// audio lives in frames whose headers carry `Path:audio`. Returns the
/// payload offset for audio frames.
fn edge_audio_payload_offset(frame: &[u8]) -> Option<usize> {
    if frame.len() < 2 {
        return None;
    }
    let header_length = ((frame[0] as usize) << 8) | frame[1] as usize;
    let header_end = 2 + header_length;
    if frame.len() < header_end {
        return None;
    }
    let header = std::str::from_utf8(&frame[2..header_end]).unwrap_or("");
    header.contains("Path:audio").then_some(header_end)
}

/// Escape text for embedding in the SSML document.
fn edge_ssml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\'', "&apos;")
        .replace('"', "&quot;")
}

/// Free Microsoft neural voices over the Edge read-aloud websocket
/// (no API key). Sends `speech.config` + an SSML turn, collects the
/// `Path:audio` binary frames until `Path:turn.end`, returns mp3.
async fn synthesize_edge(config: &TtsEdgeConfig, text: &str) -> Result<TtsOutput, String> {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    ensure_rustls_provider();

    if text.chars().count() > EDGE_MAX_TEXT_CHARS {
        return Err(format!(
            "edge tts input exceeds {EDGE_MAX_TEXT_CHARS} characters"
        ));
    }
    use tokio_tungstenite::tungstenite::http::Request as WsRequest;
    let connection_id = uuid::Uuid::new_v4().simple().to_string();
    let now = chrono::Utc::now();
    let gec = edge_sec_ms_gec(now.timestamp());
    let muid = uuid::Uuid::new_v4().simple().to_string().to_uppercase();
    let url = format!(
        "{EDGE_SYNTH_URL}?TrustedClientToken={EDGE_TRUSTED_CLIENT_TOKEN}&ConnectionId={connection_id}&Sec-MS-GEC={gec}&Sec-MS-GEC-Version=1-{EDGE_CHROMIUM_VERSION}"
    );
    // tungstenite passes a pre-built request through unchanged, so the
    // RFC 6455 handshake headers are ours to supply.
    use base64::Engine as _;
    let ws_key = base64::engine::general_purpose::STANDARD.encode(uuid::Uuid::new_v4().as_bytes());
    let request = WsRequest::builder()
        .uri(url.as_str())
        .header("Host", "speech.platform.bing.com")
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", ws_key)
        .header("Pragma", "no-cache")
        .header("Cache-Control", "no-cache")
        .header("Origin", EDGE_ORIGIN)
        .header(
            "User-Agent",
            format!(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/{EDGE_CHROMIUM_MAJOR}.0.0.0 Safari/537.36 Edg/{EDGE_CHROMIUM_MAJOR}.0.0.0"
            ),
        )
        .header("Accept-Encoding", "gzip, deflate, br, zstd")
        .header("Accept-Language", "en-US,en;q=0.9")
        .header("Cookie", format!("muid={muid};"))
        .body(())
        .map_err(|e| format!("edge tts request build failed: {e}"))?;
    let connect = tokio_tungstenite::connect_async(request);
    let (socket, _) = tokio::time::timeout(std::time::Duration::from_secs(15), connect)
        .await
        .map_err(|_| "edge tts connect timed out".to_string())?
        .map_err(|e| format!("edge tts connect failed: {e}"))?;
    let (mut sink, mut stream) = socket.split();
    let timestamp = edge_http_date(now);
    let speech_config = format!(
        "X-Timestamp:{timestamp}\r\nContent-Type:application/json; charset=utf-8\r\nPath:speech.config\r\n\r\n\
         {{\"context\":{{\"synthesis\":{{\"audio\":{{\"metadataoptions\":\
         {{\"sentenceBoundaryEnabled\":\"false\",\"wordBoundaryEnabled\":\"true\"}},\
         \"outputFormat\":\"audio-24khz-48kbitrate-mono-mp3\"}}}}}}}}"
    );
    sink.send(WsMessage::Text(speech_config))
        .await
        .map_err(|e| format!("edge tts speech.config failed: {e}"))?;
    let request_id = uuid::Uuid::new_v4().simple().to_string();
    let ssml = format!(
        "X-RequestId:{request_id}\r\nContent-Type:application/ssml+xml\r\nX-Timestamp:{timestamp}Z\r\nPath:ssml\r\n\r\n\
         <speak version='1.0' xmlns='http://www.w3.org/2001/10/synthesis' xml:lang='en-US'>\
         <voice name='{voice}'><prosody pitch='+0Hz' rate='+0%' volume='+0%'>{text}</prosody></voice></speak>",
        voice = edge_ssml_escape(&config.voice),
        text = edge_ssml_escape(text),
    );
    sink.send(WsMessage::Text(ssml))
        .await
        .map_err(|e| format!("edge tts ssml send failed: {e}"))?;

    let mut audio: Vec<u8> = Vec::new();
    let mut turn_error: Option<String> = None;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let next = tokio::time::timeout_at(deadline, stream.next())
            .await
            .map_err(|_| "edge tts timed out waiting for audio".to_string())?;
        let Some(message) = next else {
            break;
        };
        match message.map_err(|e| format!("edge tts receive failed: {e}"))? {
            WsMessage::Binary(frame) => {
                if let Some(offset) = edge_audio_payload_offset(&frame) {
                    audio.extend_from_slice(&frame[offset..]);
                }
            }
            WsMessage::Text(frame) => {
                if frame.contains("Path:turn.end") {
                    break;
                }
                if frame.contains("Path:turn.error") || frame.contains("X-ErrorCode") {
                    turn_error = Some(truncate_for_error(&frame));
                }
            }
            WsMessage::Close(_) => break,
            _ => {}
        }
    }
    if audio.is_empty() {
        return Err(match turn_error {
            Some(detail) => format!("edge tts returned no audio: {detail}"),
            None => "edge tts returned no audio".to_string(),
        });
    }
    Ok(TtsOutput {
        bytes: audio,
        mime: "audio/mpeg",
        provider: "edge".into(),
    })
}

/// Voice-list failure modes (401/403 answer `available: false` +
/// `error: "unauthorized"` per hermes; anything else is a 502).
pub enum VoicesError {
    Unauthorized,
    Other(String),
}

/// `GET https://api.elevenlabs.io/v1/voices` — non-secret voice
/// metadata for the desktop voice picker (hermes
/// `/api/audio/elevenlabs/voices` parity).
pub async fn elevenlabs_voices(api_key: &str) -> Result<Vec<serde_json::Value>, VoicesError> {
    let response = reqwest::Client::new()
        .get("https://api.elevenlabs.io/v1/voices")
        .timeout(std::time::Duration::from_secs(10))
        .header("Accept", "application/json")
        .header("xi-api-key", api_key)
        .send()
        .await
        .map_err(|e| VoicesError::Other(format!("voices request failed: {e}")))?;
    let status = response.status();
    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err(VoicesError::Unauthorized);
    }
    if !status.is_success() {
        return Err(VoicesError::Other(format!("voices failed ({status})")));
    }
    let payload: serde_json::Value = response
        .json()
        .await
        .map_err(|e| VoicesError::Other(format!("voices body failed: {e}")))?;
    let mut voices: Vec<serde_json::Value> = Vec::new();
    for voice in payload["voices"].as_array().cloned().unwrap_or_default() {
        let voice_id = voice["voice_id"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .to_string();
        if voice_id.is_empty() {
            continue;
        }
        let name = voice["name"]
            .as_str()
            .filter(|n| !n.trim().is_empty())
            .unwrap_or(&voice_id)
            .to_string();
        let category = voice["category"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .to_string();
        let label = if category.is_empty() {
            name.clone()
        } else {
            format!("{name} ({category})")
        };
        voices.push(serde_json::json!({
            "voice_id": voice_id,
            "name": name,
            "label": label,
        }));
    }
    voices.sort_by(|a, b| {
        a["label"]
            .as_str()
            .unwrap_or_default()
            .to_lowercase()
            .cmp(&b["label"].as_str().unwrap_or_default().to_lowercase())
    });
    Ok(voices)
}

/// Percent-encode a voice id for a URL path segment (alnum + `-_.~`
/// pass through — enough for ElevenLabs ids).
fn urlencoding(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn truncate_for_error(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.len() <= 300 {
        trimmed.to_string()
    } else {
        format!("{}…", &trimmed[..297])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tts_config_defaults_match_hermes() {
        let config = TtsConfig::default();
        assert_eq!(config.provider, "openai");
        assert_eq!(config.openai.model, "gpt-4o-mini-tts");
        assert_eq!(config.openai.voice, "alloy");
        assert_eq!(config.elevenlabs.voice_id, "pNInz6obpgDQGcFmaJgB");
        assert_eq!(config.elevenlabs.model_id, "eleven_multilingual_v2");
        assert_eq!(config.edge.voice, "en-US-AriaNeural");
    }

    #[test]
    fn tts_config_parses_toml_overrides() {
        let parsed: TtsConfig = toml::from_str(
            "provider = \"elevenlabs\"\n\n[elevenlabs]\nvoice_id = \"abc\"\nmodel_id = \"eleven_turbo_v2\"\n",
        )
        .unwrap();
        assert_eq!(parsed.provider, "elevenlabs");
        assert_eq!(parsed.elevenlabs.voice_id, "abc");
        assert_eq!(parsed.elevenlabs.model_id, "eleven_turbo_v2");
        // Untouched sections keep hermes defaults.
        assert_eq!(parsed.openai.voice, "alloy");
    }

    #[tokio::test]
    async fn synthesize_rejects_unknown_provider() {
        let mut config = TtsConfig::default();
        config.provider = "piper".into();
        let error = synthesize(&config, "hello").await.unwrap_err();
        assert!(error.contains("unsupported tts provider"), "{error}");
        assert!(
            error.contains("expected edge, openai or elevenlabs"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn synthesize_openai_without_key_fails_clean() {
        let saved = std::env::var("OPENAI_API_KEY").ok();
        std::env::remove_var("OPENAI_API_KEY");
        // The home .env under a temp dir is empty too.
        let dir = tempfile::tempdir().expect("tempdir");
        let saved_home = std::env::var("ULNCLAW_HOME").ok();
        std::env::set_var("ULNCLAW_HOME", dir.path());
        let config = TtsConfig::default();
        let error = synthesize(&config, "hello").await.unwrap_err();
        assert!(error.contains("OPENAI_API_KEY"), "{error}");
        match saved {
            Some(v) => std::env::set_var("OPENAI_API_KEY", v),
            None => {}
        }
        match saved_home {
            Some(v) => std::env::set_var("ULNCLAW_HOME", v),
            None => std::env::remove_var("ULNCLAW_HOME"),
        }
    }

    #[test]
    fn urlencoding_passes_elevenlabs_ids() {
        assert_eq!(urlencoding("pNInz6obpgDQGcFmaJgB"), "pNInz6obpgDQGcFmaJgB");
        assert_eq!(urlencoding("a b/c"), "a%20b%2Fc");
    }

    #[test]
    fn edge_sec_ms_gec_matches_reference() {
        // Reference value computed with the edge-tts 7.x DRM algorithm
        // (SHA-256 over ASCII "<filetime rounded to 5 min><client token>").
        assert_eq!(
            edge_sec_ms_gec(1_754_600_000),
            "31556385FC374AAD121A4BEB4A9FAC5BF0833A3F49EE0D626D450004E12FD118"
        );
    }

    #[test]
    fn edge_audio_payload_offset_parses_frames() {
        // Audio frame: 2-byte header length + headers + payload.
        let header = b"X-RequestId:abc\r\nContent-Type:audio/mpeg\r\nPath:audio\r\n";
        let mut frame = vec![0u8, header.len() as u8];
        frame.extend_from_slice(header);
        frame.extend_from_slice(b"MP3DATA");
        let offset = edge_audio_payload_offset(&frame).expect("audio frame");
        assert_eq!(&frame[offset..], b"MP3DATA");

        // Non-audio frames yield nothing.
        let other_header = b"Path:response\r\n";
        let mut other = vec![0u8, other_header.len() as u8];
        other.extend_from_slice(other_header);
        assert_eq!(edge_audio_payload_offset(&other), None);
        assert_eq!(edge_audio_payload_offset(&[0u8]), None);
    }

    #[test]
    fn edge_ssml_escape_escapes_markup() {
        assert_eq!(
            edge_ssml_escape("a<b>&'\"c"),
            "a&lt;b&gt;&amp;&apos;&quot;c"
        );
    }

    /// Live smoke test against the Microsoft read-aloud endpoint
    /// (network required — run with `--include-ignored`).
    #[tokio::test]
    #[ignore]
    async fn synthesize_edge_live_smoke() {
        let config = TtsEdgeConfig::default();
        let output = synthesize_edge(&config, "Hello from ulnclaw.")
            .await
            .expect("edge synthesis");
        assert!(
            output.bytes.len() > 1_000,
            "audio too small: {}",
            output.bytes.len()
        );
        assert_eq!(output.mime, "audio/mpeg");
        // mp3 frames start with 0xFF sync or an ID3 tag.
        let head = &output.bytes[..3.min(output.bytes.len())];
        assert!(
            head[0] == 0xFF || head.starts_with(b"ID3"),
            "not mp3: {head:?}"
        );
    }

    #[test]
    fn tts_config_parses_edge_section() {
        let parsed: TtsConfig =
            toml::from_str("provider = \"edge\"\n\n[edge]\nvoice = \"zh-CN-XiaoxiaoNeural\"\n")
                .unwrap();
        assert_eq!(parsed.provider, "edge");
        assert_eq!(parsed.edge.voice, "zh-CN-XiaoxiaoNeural");
    }

    #[tokio::test]
    async fn open_pcm_stream_rejects_unstreamable_providers() {
        // edge's read-aloud protocol and local providers have no chunked
        // PCM API — speak-stream demotes them to the batch endpoint.
        let mut config = TtsConfig::default();
        config.provider = "edge".into();
        let Err(error) = open_pcm_stream(&config, "hello").await else {
            panic!("edge must not stream")
        };
        assert!(error.contains("no chunked pcm api"), "{error}");

        config.provider = "piper".into();
        let Err(error) = open_pcm_stream(&config, "hello").await else {
            panic!("piper must not stream")
        };
        assert!(error.contains("no chunked pcm api"), "{error}");
    }

    #[tokio::test]
    async fn open_pcm_stream_openai_without_key_fails_clean() {
        let _guard = crate::models_dev::test_env_lock();
        let saved = std::env::var("OPENAI_API_KEY").ok();
        std::env::remove_var("OPENAI_API_KEY");
        let dir = tempfile::tempdir().expect("tempdir");
        let saved_home = std::env::var("ULNCLAW_HOME").ok();
        std::env::set_var("ULNCLAW_HOME", dir.path());

        let config = TtsConfig::default(); // provider = openai
        let Err(error) = open_pcm_stream(&config, "hello").await else {
            panic!("openai without key must not stream")
        };
        assert!(error.contains("OPENAI_API_KEY"), "{error}");

        match saved {
            Some(v) => std::env::set_var("OPENAI_API_KEY", v),
            None => {}
        }
        match saved_home {
            Some(v) => std::env::set_var("ULNCLAW_HOME", v),
            None => std::env::remove_var("ULNCLAW_HOME"),
        }
    }
}

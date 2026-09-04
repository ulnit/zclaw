//! OpenAI-compatible provider implementation
//!
//! Supports: OpenAI, Azure OpenAI, OpenRouter, DashScope (Alibaba), Ollama, llama.cpp,
//! and any other provider implementing the OpenAI Chat Completions API.

use super::{
    FunctionCall, Message, Provider, ProviderRequest, ProviderResponse, ProviderStream, ToolCall,
    Usage,
};
use crate::error::{AgentError, Result};
use crate::tools::ToolDefinition;
use async_trait::async_trait;
use futures::TryStreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::debug;

/// OpenAI-compatible provider
pub struct OpenAiProvider {
    client: Client,
    endpoint: String,
    api_key: Option<String>,
    model: String,
    name: String,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
    max_retries: usize,
    /// Pinned reasoning effort sent as `reasoning_effort` on every
    /// request (hermes `--reasoning` / `agent.reasoning_effort`);
    /// None = the endpoint's own default.
    reasoning_effort: Option<String>,
    /// Pinned service tier sent as `service_tier` on every request
    /// (hermes Priority Processing / `agent.service_tier`); None = the
    /// endpoint's own default.
    service_tier: Option<String>,
    /// P625: runtime override set by `/fast` / `PUT /api/fast`;
    /// `None` outer = use `service_tier`, `Some(inner)` = force inner.
    service_tier_override: Arc<std::sync::Mutex<Option<Option<String>>>>,
}

impl OpenAiProvider {
    pub fn builder() -> OpenAiProviderBuilder {
        OpenAiProviderBuilder::default()
    }

    /// P625: the service tier in effect — runtime override (set by
    /// `/fast` / `PUT /api/fast`) wins over the build-time pin.
    fn current_service_tier(&self) -> Option<String> {
        let guard = self
            .service_tier_override
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match guard.as_ref() {
            Some(override_tier) => override_tier.clone(),
            None => self.service_tier.clone(),
        }
    }

    /// Build the full API URL
    fn api_url(&self) -> String {
        let base = self.endpoint.trim_end_matches('/');
        if base.ends_with("/chat/completions") {
            base.to_string()
        } else if base.ends_with("/v1") || base.ends_with("/v1/") {
            format!("{}/chat/completions", base.trim_end_matches('/'))
        } else {
            format!("{}/v1/chat/completions", base)
        }
    }

    /// Statuses worth retrying (rate limits + transient server errors).
    fn retriable_status(status: reqwest::StatusCode) -> bool {
        matches!(status.as_u16(), 408 | 429 | 500 | 502 | 503 | 504)
    }

    /// Exponential backoff with a small jitter: 500ms, 1s, 2s, ... capped.
    fn retry_delay(attempt: usize) -> std::time::Duration {
        let base_ms = 500u64.saturating_mul(1 << attempt.min(4));
        let jitter_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| u64::from(d.subsec_nanos() % 250))
            .unwrap_or(0);
        std::time::Duration::from_millis(base_ms.min(8000) + jitter_ms)
    }

    /// POST the request body, retrying transient failures (network errors,
    /// 429/5xx).  Returns the successful response; non-retriable HTTP errors
    /// are parsed into `AgentError::Provider`.
    async fn send_api_request(
        &self,
        url: &str,
        api_request: &ApiRequest,
    ) -> Result<reqwest::Response> {
        let mut last_error: Option<AgentError> = None;
        for attempt in 0..=self.max_retries {
            if attempt > 0 {
                let delay = Self::retry_delay(attempt - 1);
                debug!(
                    "provider retry {}/{} after {:?} ({})",
                    attempt,
                    self.max_retries,
                    delay,
                    last_error
                        .as_ref()
                        .map(|e| e.to_string())
                        .unwrap_or_default()
                );
                tokio::time::sleep(delay).await;
            }
            let mut req_builder = self
                .client
                .post(url)
                .header("Content-Type", "application/json");
            if let Some(ref key) = self.api_key {
                req_builder = req_builder.bearer_auth(key);
            }
            let response = match req_builder.json(api_request).send().await {
                Ok(response) => response,
                Err(e) => {
                    // Network-level failure are always retryable.
                    last_error = Some(AgentError::Provider(format!("HTTP request failed: {}", e)));
                    continue;
                }
            };
            let status = response.status();
            if status.is_success() {
                return Ok(response);
            }
            let error_body = response.text().await.unwrap_or_default();
            let message = serde_json::from_str::<ApiError>(&error_body)
                .map(|api_err| api_err.error.message)
                .unwrap_or_else(|_| error_body.chars().take(500).collect::<String>());
            if Self::retriable_status(status) && attempt < self.max_retries {
                last_error = Some(AgentError::Provider(format!(
                    "API error ({}): {}",
                    status, message
                )));
                continue;
            }
            return Err(AgentError::Provider(format!(
                "API error ({}): {}",
                status, message
            )));
        }
        Err(last_error
            .unwrap_or_else(|| AgentError::Provider("request failed after retries".into())))
    }
}

pub struct OpenAiProviderBuilder {
    endpoint: Option<String>,
    api_key: Option<String>,
    model: Option<String>,
    name: Option<String>,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
    max_retries: usize,
    reasoning_effort: Option<String>,
    service_tier: Option<String>,
}

impl Default for OpenAiProviderBuilder {
    fn default() -> Self {
        Self {
            endpoint: None,
            api_key: None,
            model: None,
            name: None,
            max_tokens: None,
            temperature: None,
            max_retries: 2,
            reasoning_effort: None,
            service_tier: None,
        }
    }
}

impl OpenAiProviderBuilder {
    pub fn endpoint(mut self, endpoint: &str) -> Self {
        self.endpoint = Some(endpoint.to_string());
        self
    }

    pub fn api_key(mut self, key: &str) -> Self {
        self.api_key = Some(key.to_string());
        self
    }

    pub fn model(mut self, model: &str) -> Self {
        self.model = Some(model.to_string());
        self
    }

    pub fn name(mut self, name: &str) -> Self {
        self.name = Some(name.to_string());
        self
    }

    pub fn max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    pub fn temperature(mut self, temp: f32) -> Self {
        self.temperature = Some(temp);
        self
    }

    /// Retry transient errors (429/5xx/network) up to `n` extra times with
    /// exponential backoff. Default 2.
    pub fn max_retries(mut self, n: usize) -> Self {
        self.max_retries = n;
        self
    }

    /// Pin the reasoning effort (none|minimal|low|medium|high|xhigh|
    /// max|ultra) sent on every request; None keeps the endpoint
    /// default (hermes `agent.reasoning_effort`).
    pub fn reasoning_effort(mut self, effort: &str) -> Self {
        self.reasoning_effort = Some(effort.to_string());
        self
    }

    /// Pin the service tier (e.g. "priority") sent on every request;
    /// None keeps the endpoint default (hermes `agent.service_tier`).
    pub fn service_tier(mut self, tier: &str) -> Self {
        self.service_tier = Some(tier.to_string());
        self
    }

    pub fn build(self) -> Result<OpenAiProvider> {
        let endpoint = self
            .endpoint
            .ok_or_else(|| AgentError::config("endpoint is required"))?;
        let model = self
            .model
            .ok_or_else(|| AgentError::config("model is required"))?;
        let name = self.name.unwrap_or_else(|| model.clone());

        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .map_err(|e| AgentError::Internal(format!("Failed to build HTTP client: {}", e)))?;

        Ok(OpenAiProvider {
            client,
            endpoint,
            api_key: self.api_key,
            model,
            name,
            max_tokens: self.max_tokens,
            temperature: self.temperature,
            max_retries: self.max_retries,
            reasoning_effort: self.reasoning_effort,
            service_tier: self.service_tier,
            service_tier_override: Arc::new(std::sync::Mutex::new(None)),
        })
    }
}

// --- API Request/Response types for OpenAI Chat Completions ---

#[derive(Serialize)]
struct ApiRequest {
    model: String,
    messages: Vec<ApiMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ApiTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    service_tier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop: Option<Vec<String>>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct ApiMessage {
    role: String,
    /// JSON string for plain text, array of content parts when images
    /// are attached (P226 multimodal injection).
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ApiToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
struct ApiToolCall {
    id: String,
    #[serde(rename = "type")]
    call_type: String,
    function: ApiFunctionCall,
}

#[derive(Serialize, Deserialize, Debug)]
struct ApiFunctionCall {
    name: String,
    arguments: String,
}

#[derive(Serialize)]
struct ApiTool {
    #[serde(rename = "type")]
    tool_type: String,
    function: ApiFunction,
}

#[derive(Serialize)]
struct ApiFunction {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    parameters: serde_json::Value,
}

#[derive(Deserialize, Debug)]
struct ApiResponse {
    #[serde(default)]
    choices: Vec<ApiChoice>,
    #[serde(default)]
    usage: Option<ApiUsage>,
    #[serde(default)]
    model: Option<String>,
}

#[derive(Deserialize, Debug)]
struct ApiChoice {
    message: ApiResponseMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Debug)]
struct ApiResponseMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ApiToolCall>>,
    /// Some models put reasoning/thinking here
    #[serde(default)]
    reasoning_content: Option<String>,
}

#[derive(Deserialize, Debug)]
struct ApiUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
    #[serde(default)]
    total_tokens: u32,
}

#[derive(Deserialize, Debug)]
struct ApiError {
    error: ApiErrorDetail,
}

#[derive(Deserialize, Debug)]
#[allow(dead_code)]
struct ApiErrorDetail {
    message: String,
    #[serde(default)]
    #[serde(rename = "type")]
    error_type: Option<String>,
    #[serde(default)]
    code: Option<String>,
}

// --- Streaming chunk shapes ---

#[derive(Deserialize, Debug)]
struct ApiStreamChunk {
    #[serde(default)]
    choices: Vec<ApiStreamChoice>,
    #[serde(default)]
    usage: Option<ApiUsage>,
    #[serde(default)]
    model: Option<String>,
}

#[derive(Deserialize, Debug)]
struct ApiStreamChoice {
    #[serde(default)]
    delta: ApiStreamDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Debug, Default)]
struct ApiStreamDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ApiStreamToolCall>>,
}

#[derive(Deserialize, Debug)]
struct ApiStreamToolCall {
    #[serde(default)]
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<ApiStreamFunction>,
}

#[derive(Deserialize, Debug)]
struct ApiStreamFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

/// Parse one SSE `data:` payload into a `StreamChunk`.
pub fn parse_stream_chunk(data: &str) -> crate::error::Result<crate::provider::StreamChunk> {
    let chunk: ApiStreamChunk = serde_json::from_str(data)
        .map_err(|e| crate::error::AgentError::Provider(format!("bad stream chunk: {}", e)))?;
    let mut out = crate::provider::StreamChunk {
        model: chunk.model,
        usage: chunk.usage.map(|u| crate::provider::Usage {
            prompt_tokens: u.prompt_tokens,
            completion_tokens: u.completion_tokens,
            total_tokens: u.total_tokens,
        }),
        ..Default::default()
    };
    if let Some(choice) = chunk.choices.into_iter().next() {
        out.delta_content = choice.delta.content;
        out.delta_reasoning = choice.delta.reasoning_content;
        out.finish_reason = choice.finish_reason;
        for tc in choice.delta.tool_calls.unwrap_or_default() {
            out.tool_call_deltas.push(crate::provider::ToolCallDelta {
                index: tc.index,
                id: tc.id,
                name_delta: tc.function.as_ref().and_then(|f| f.name.clone()),
                arguments_delta: tc.function.as_ref().and_then(|f| f.arguments.clone()),
            });
        }
    }
    Ok(out)
}

// --- Conversion helpers ---

/// Attach images to the last user message (P226 native multimodal
/// injection): its `content` becomes an array of parts — the text part
/// plus one `image_url` part per image (hermes media-injection parity).
fn attach_images(api_messages: &mut [ApiMessage], images: &[crate::provider::MessageImage]) {
    if images.is_empty() {
        return;
    }
    let Some(idx) = api_messages.iter().rposition(|m| m.role == "user") else {
        return;
    };
    let mut parts: Vec<serde_json::Value> = Vec::new();
    if let Some(content) = api_messages[idx].content.clone() {
        let text = match content {
            serde_json::Value::String(text) => text,
            other => other.to_string(),
        };
        if !text.trim().is_empty() {
            parts.push(serde_json::json!({"type": "text", "text": text}));
        }
    }
    for image in images {
        parts.push(serde_json::json!({
            "type": "image_url",
            "image_url": {"url": image.url},
        }));
    }
    api_messages[idx].content = Some(serde_json::Value::Array(parts));
}

fn message_to_api(msg: &Message) -> ApiMessage {
    ApiMessage {
        role: msg.role.to_string(),
        content: msg.content.clone().map(serde_json::Value::String),
        tool_calls: msg.tool_calls.as_ref().map(|calls| {
            calls
                .iter()
                .map(|c| ApiToolCall {
                    id: c.id.clone(),
                    call_type: c.call_type.clone(),
                    function: ApiFunctionCall {
                        name: c.function.name.clone(),
                        arguments: c.function.arguments.clone(),
                    },
                })
                .collect()
        }),
        tool_call_id: msg.tool_call_id.clone(),
        name: msg.name.clone(),
    }
}

fn tool_def_to_api(tool: &ToolDefinition) -> ApiTool {
    ApiTool {
        tool_type: "function".to_string(),
        function: ApiFunction {
            name: tool.name.clone(),
            description: Some(tool.description.clone()),
            parameters: tool.parameters.clone(),
        },
    }
}

#[async_trait]
impl Provider for OpenAiProvider {
    fn supports_service_tier(&self) -> bool {
        true
    }

    fn set_service_tier(&self, tier: Option<String>) {
        let mut guard = self
            .service_tier_override
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = Some(tier);
    }

    fn active_service_tier(&self) -> Option<String> {
        self.current_service_tier()
    }

    async fn chat_completion(&self, request: ProviderRequest) -> Result<ProviderResponse> {
        let url = self.api_url();
        debug!("OpenAI API call to: {}", url);

        let mut api_messages: Vec<ApiMessage> =
            request.messages.iter().map(message_to_api).collect();
        if let Some(ref images) = request.images {
            attach_images(&mut api_messages, images);
        }

        let api_tools: Option<Vec<ApiTool>> = if request.tools.is_empty() {
            None
        } else {
            Some(request.tools.iter().map(tool_def_to_api).collect())
        };

        let api_request = ApiRequest {
            model: request.model.clone(),
            messages: api_messages,
            tools: api_tools,
            max_tokens: request.max_tokens.or(self.max_tokens),
            temperature: request.temperature.or(self.temperature),
            reasoning_effort: self.reasoning_effort.clone(),
            service_tier: self.active_service_tier(),
            stop: request.stop,
            stream: false,
            stream_options: None,
        };

        let response = self.send_api_request(&url, &api_request).await?;

        let api_response: ApiResponse = response
            .json()
            .await
            .map_err(|e| AgentError::Provider(format!("Failed to parse response: {}", e)))?;

        let choice = api_response
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| AgentError::Provider("No choices in response".to_string()))?;

        let tool_calls: Vec<ToolCall> = choice
            .message
            .tool_calls
            .unwrap_or_default()
            .into_iter()
            .map(|tc| ToolCall {
                id: tc.id,
                call_type: tc.call_type,
                function: FunctionCall {
                    name: tc.function.name,
                    arguments: tc.function.arguments,
                },
            })
            .collect();

        let usage = api_response.usage.map(|u| Usage {
            prompt_tokens: u.prompt_tokens,
            completion_tokens: u.completion_tokens,
            total_tokens: u.total_tokens,
        });

        Ok(ProviderResponse {
            content: choice.message.content,
            tool_calls,
            usage,
            model: api_response.model.unwrap_or(request.model),
            reasoning: choice.message.reasoning_content,
            finish_reason: choice.finish_reason,
        })
    }

    async fn chat_completion_stream(&self, request: ProviderRequest) -> Result<ProviderStream> {
        let url = self.api_url();
        debug!("OpenAI streaming call to: {}", url);

        let mut api_messages: Vec<ApiMessage> =
            request.messages.iter().map(message_to_api).collect();
        if let Some(ref images) = request.images {
            attach_images(&mut api_messages, images);
        }
        let api_tools: Option<Vec<ApiTool>> = if request.tools.is_empty() {
            None
        } else {
            Some(request.tools.iter().map(tool_def_to_api).collect())
        };
        let api_request = ApiRequest {
            model: request.model.clone(),
            messages: api_messages,
            tools: api_tools,
            max_tokens: request.max_tokens.or(self.max_tokens),
            temperature: request.temperature.or(self.temperature),
            reasoning_effort: self.reasoning_effort.clone(),
            service_tier: self.active_service_tier(),
            stop: request.stop,
            stream: true,
            stream_options: Some(serde_json::json!({"include_usage": true})),
        };

        let response = self.send_api_request(&url, &api_request).await?;

        let byte_stream = response.bytes_stream();
        let reader = SseLineReader {
            stream: Box::pin(byte_stream),
            buf: Vec::new(),
            eof: false,
        };
        let chunk_stream = futures::stream::unfold(reader, |mut reader| async move {
            loop {
                let line = match reader.next_line().await {
                    Some(line) => line,
                    None => return None, // EOF
                };
                let line = line.trim().to_string();
                if line.is_empty() || line.starts_with(':') {
                    continue; // blank separator / SSE comment (keepalive)
                }
                let Some(data) = line.strip_prefix("data:") else {
                    continue; // event:/id:/retry: lines are not used
                };
                let data = data.trim();
                if data == "[DONE]" {
                    return None;
                }
                return Some((parse_stream_chunk(data), reader));
            }
        });
        Ok(Box::pin(chunk_stream))
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn name(&self) -> &str {
        &self.name
    }

    async fn analyze_image(&self, prompt: &str, image_url: &str) -> Result<String> {
        let body = serde_json::json!({
            "model": self.model,
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": prompt},
                    {"type": "image_url", "image_url": {"url": image_url}}
                ]
            }],
            "max_tokens": self.max_tokens.unwrap_or(1024),
        });
        let mut request = self.client.post(self.api_url()).json(&body);
        if let Some(ref key) = self.api_key {
            request = request.bearer_auth(key);
        }
        let response = request
            .send()
            .await
            .map_err(|e| AgentError::provider(format!("vision request failed: {}", e)))?;
        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(AgentError::provider(format!(
                "vision API {}: {}",
                status,
                &text[..text.len().min(300)]
            )));
        }
        let payload: serde_json::Value = response
            .json()
            .await
            .map_err(|e| AgentError::provider(format!("parse vision response: {}", e)))?;
        payload
            .pointer("/choices/0/message/content")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| AgentError::provider("vision response missing content"))
    }

    async fn analyze_video(&self, prompt: &str, video_data_url: &str) -> Result<String> {
        // Hermes call_kwargs for video: temperature 0.1, max_tokens 4000,
        // generous timeout for large inline payloads.
        let body = serde_json::json!({
            "model": self.model,
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": prompt},
                    {"type": "video_url", "video_url": {"url": video_data_url}}
                ]
            }],
            "max_tokens": 4000,
            "temperature": 0.1,
        });
        let mut request = self
            .client
            .post(self.api_url())
            .timeout(std::time::Duration::from_secs(180))
            .json(&body);
        if let Some(ref key) = self.api_key {
            request = request.bearer_auth(key);
        }
        let response = request
            .send()
            .await
            .map_err(|e| AgentError::provider(format!("video analysis request failed: {}", e)))?;
        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(AgentError::provider(format!(
                "video analysis API {}: {}",
                status,
                &text[..text.len().min(300)]
            )));
        }
        let payload: serde_json::Value = response
            .json()
            .await
            .map_err(|e| AgentError::provider(format!("parse video analysis response: {}", e)))?;
        payload
            .pointer("/choices/0/message/content")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| AgentError::provider("video analysis response missing content"))
    }
}

/// Incremental SSE line reader over a byte stream.
struct SseLineReader {
    stream: std::pin::Pin<Box<dyn futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>,
    buf: Vec<u8>,
    eof: bool,
}

impl SseLineReader {
    /// Next complete line (without trailing \n), or None at EOF.
    async fn next_line(&mut self) -> Option<String> {
        loop {
            if let Some(pos) = self.buf.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = self.buf.drain(..=pos).collect();
                let line = line
                    .strip_suffix(b"\n".as_slice())
                    .map(|l| l.to_vec())
                    .unwrap_or(line);
                let line = line
                    .strip_suffix(b"\r".as_slice())
                    .map(|l| l.to_vec())
                    .unwrap_or(line);
                return Some(String::from_utf8_lossy(&line).to_string());
            }
            if self.eof {
                if self.buf.is_empty() {
                    return None;
                }
                let rest: Vec<u8> = self.buf.drain(..).collect();
                return Some(String::from_utf8_lossy(&rest).to_string());
            }
            match self.stream.try_next().await {
                Ok(Some(bytes)) => self.buf.extend_from_slice(&bytes),
                Ok(None) => self.eof = true,
                Err(_) => self.eof = true, // treat transport errors as EOF
            }
        }
    }
}

#[cfg(test)]
mod streaming_tests {
    use super::*;
    use crate::provider::assemble_tool_calls;

    #[test]
    fn test_parse_content_chunk() {
        let chunk = parse_stream_chunk(
            r#"{"id":"x","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"Hel"},"finish_reason":null}]}"#,
        )
        .unwrap();
        assert_eq!(chunk.delta_content.as_deref(), Some("Hel"));
        assert!(chunk.finish_reason.is_none());
    }

    #[test]
    fn test_parse_tool_call_deltas() {
        let c1 = parse_stream_chunk(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"get_time","arguments":""}}]}}]}"#,
        )
        .unwrap();
        let c2 = parse_stream_chunk(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"tz\":"}}]}}]}"#,
        )
        .unwrap();
        let c3 = parse_stream_chunk(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"UTC\"}"}}]}}],"model":"m"}"#,
        )
        .unwrap();
        let mut deltas = vec![];
        deltas.extend(c1.tool_call_deltas);
        deltas.extend(c2.tool_call_deltas);
        deltas.extend(c3.tool_call_deltas);
        let calls = assemble_tool_calls(&deltas);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].function.name, "get_time");
        assert_eq!(calls[0].function.arguments, r#"{"tz":"UTC"}"#);
    }

    #[test]
    fn test_parse_final_usage_chunk() {
        let chunk = parse_stream_chunk(
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":7,"total_tokens":12}}"#,
        )
        .unwrap();
        assert_eq!(chunk.finish_reason.as_deref(), Some("stop"));
        assert_eq!(chunk.usage.as_ref().unwrap().total_tokens, 12);
    }

    #[test]
    fn test_retriable_status() {
        use reqwest::StatusCode;
        assert!(OpenAiProvider::retriable_status(
            StatusCode::TOO_MANY_REQUESTS
        ));
        assert!(OpenAiProvider::retriable_status(
            StatusCode::INTERNAL_SERVER_ERROR
        ));
        assert!(OpenAiProvider::retriable_status(StatusCode::BAD_GATEWAY));
        assert!(OpenAiProvider::retriable_status(
            StatusCode::SERVICE_UNAVAILABLE
        ));
        assert!(OpenAiProvider::retriable_status(
            StatusCode::GATEWAY_TIMEOUT
        ));
        assert!(!OpenAiProvider::retriable_status(StatusCode::BAD_REQUEST));
        assert!(!OpenAiProvider::retriable_status(StatusCode::UNAUTHORIZED));
        assert!(!OpenAiProvider::retriable_status(StatusCode::NOT_FOUND));
    }

    #[tokio::test]
    async fn test_retry_on_500_then_success() {
        use axum::extract::State;
        use axum::routing::post;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = attempts.clone();
        let app = axum::Router::new()
            .route(
                "/v1/chat/completions",
                post(|State(counter): State<Arc<AtomicUsize>>| async move {
                    let n = counter.fetch_add(1, Ordering::SeqCst);
                    if n == 0 {
                        (
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                            r#"{"error": {"message": "boom"}}"#,
                        )
                    } else {
                        (
                            axum::http::StatusCode::OK,
                            r#"{"choices":[{"message":{"content":"recovered"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}}"#,
                        )
                    }
                }),
            )
            .with_state(counter);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let provider = OpenAiProvider::builder()
            .endpoint(&format!("http://{}/v1", addr))
            .model("retry-model")
            .max_retries(2)
            .build()
            .unwrap();
        let request = crate::provider::ProviderRequest {
            messages: vec![crate::provider::Message {
                role: crate::provider::Role::User,
                content: Some("hi".into()),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            }],
            tools: vec![],
            model: "retry-model".into(),
            max_tokens: None,
            temperature: None,
            stream: false,
            stop: None,

            images: None,
        };
        let response = provider.chat_completion(request).await.unwrap();
        assert_eq!(response.content.as_deref(), Some("recovered"));
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_retry_exhausted_returns_error() {
        use axum::routing::post;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = attempts.clone();
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            post(move || {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    (
                        axum::http::StatusCode::TOO_MANY_REQUESTS,
                        r#"{"error": {"message": "rate limited"}}"#,
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let provider = OpenAiProvider::builder()
            .endpoint(&format!("http://{}/v1", addr))
            .model("retry-model")
            .max_retries(1)
            .build()
            .unwrap();
        let request = crate::provider::ProviderRequest {
            messages: vec![],
            tools: vec![],
            model: "retry-model".into(),
            max_tokens: None,
            temperature: None,
            stream: false,
            stop: None,

            images: None,
        };
        let err = provider.chat_completion(request).await.unwrap_err();
        assert!(err.to_string().contains("429"), "got: {}", err);
        assert_eq!(attempts.load(Ordering::SeqCst), 2); // initial + 1 retry
    }

    #[tokio::test]
    async fn test_sse_line_reader() {
        let data = b"data: one\n\ndata: two\n: keepalive\ndata: [DONE]\n";
        let stream = futures::stream::iter(vec![
            Ok(bytes::Bytes::from(&data[..11])),
            Ok(bytes::Bytes::from(&data[11..])),
        ]);
        let mut reader = SseLineReader {
            stream: Box::pin(stream),
            buf: Vec::new(),
            eof: false,
        };
        let mut lines = Vec::new();
        while let Some(line) = reader.next_line().await {
            lines.push(line);
        }
        assert_eq!(
            lines,
            vec!["data: one", "", "data: two", ": keepalive", "data: [DONE]"]
        );
    }

    #[test]
    fn attach_images_builds_content_parts_on_last_user_message() {
        let mut messages = vec![
            ApiMessage {
                role: "system".to_string(),
                content: Some(serde_json::Value::String("sys".to_string())),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            },
            ApiMessage {
                role: "user".to_string(),
                content: Some(serde_json::Value::String("what is this?".to_string())),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            },
            ApiMessage {
                role: "assistant".to_string(),
                content: Some(serde_json::Value::String("hmm".to_string())),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            },
            ApiMessage {
                role: "user".to_string(),
                content: Some(serde_json::Value::String("and this?".to_string())),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            },
        ];
        let images = vec![crate::provider::MessageImage {
            url: "data:image/png;base64,AAA".to_string(),
            media_type: Some("image/png".to_string()),
        }];
        attach_images(&mut messages, &images);
        // Earlier user message untouched.
        assert_eq!(
            messages[1].content,
            Some(serde_json::Value::String("what is this?".to_string()))
        );
        // Last user message became a parts array: text + image_url.
        let parts = messages[3].content.clone().unwrap();
        let arr = parts.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["type"], "text");
        assert_eq!(arr[0]["text"], "and this?");
        assert_eq!(arr[1]["type"], "image_url");
        assert_eq!(arr[1]["image_url"]["url"], "data:image/png;base64,AAA");
        // Non-user messages untouched.
        assert_eq!(
            messages[0].content,
            Some(serde_json::Value::String("sys".to_string()))
        );
    }

    #[test]
    fn attach_images_noop_when_empty_or_no_user() {
        let mut messages = vec![ApiMessage {
            role: "assistant".to_string(),
            content: Some(serde_json::Value::String("hi".to_string())),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }];
        let images = vec![crate::provider::MessageImage {
            url: "data:image/png;base64,AAA".to_string(),
            media_type: None,
        }];
        attach_images(&mut messages, &images);
        // No user message → untouched.
        assert_eq!(
            messages[0].content,
            Some(serde_json::Value::String("hi".to_string()))
        );
        let mut user = vec![ApiMessage {
            role: "user".to_string(),
            content: Some(serde_json::Value::String("hi".to_string())),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }];
        attach_images(&mut user, &[]);
        assert_eq!(
            user[0].content,
            Some(serde_json::Value::String("hi".to_string()))
        );
    }

    #[test]
    fn test_service_tier_builder_override_and_serialization() {
        use crate::provider::Provider;

        let provider = OpenAiProvider::builder()
            .endpoint("http://127.0.0.1:9")
            .model("gpt-5")
            .service_tier("priority")
            .build()
            .expect("provider builds");
        assert!(provider.supports_service_tier());
        // Build-time pin is active until overridden.
        assert_eq!(provider.active_service_tier(), Some("priority".to_string()));
        // Override to normal (None = endpoint default).
        provider.set_service_tier(None);
        assert_eq!(provider.active_service_tier(), None);
        // Override to a different tier.
        provider.set_service_tier(Some("flex".to_string()));
        assert_eq!(provider.active_service_tier(), Some("flex".to_string()));

        // The request payload carries service_tier when set…
        let request = ApiRequest {
            model: "gpt-5".to_string(),
            messages: vec![],
            tools: None,
            max_tokens: None,
            temperature: None,
            reasoning_effort: None,
            service_tier: Some("priority".to_string()),
            stop: None,
            stream: false,
            stream_options: None,
        };
        let value = serde_json::to_value(&request).expect("serializes");
        assert_eq!(value["service_tier"], "priority");

        // …and omits it when None.
        let request = ApiRequest {
            model: "gpt-5".to_string(),
            messages: vec![],
            tools: None,
            max_tokens: None,
            temperature: None,
            reasoning_effort: None,
            service_tier: None,
            stop: None,
            stream: false,
            stream_options: None,
        };
        let value = serde_json::to_value(&request).expect("serializes");
        assert!(value.get("service_tier").is_none());
    }
}

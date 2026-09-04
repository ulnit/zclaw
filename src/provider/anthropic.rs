//! Native Anthropic Messages API provider — port of hermes'
//! `anthropic_messages` transport (agent/anthropic_adapter.py).
//!
//! Speaks `POST /v1/messages` with `x-api-key` auth and
//! `anthropic-version: 2023-06-01`: system prompts move to the `system`
//! parameter, tool calls become `tool_use` blocks, tool results become
//! `tool_result` blocks merged into user turns, and responses/streaming
//! events map back onto the shared `ProviderResponse` / `StreamChunk`
//! shapes.  Third-party Anthropic-compatible endpoints (proxies under an
//! `/anthropic` suffix, MiniMax, DashScope, …) work via `endpoint`.

use super::{
    FunctionCall, Message, Provider, ProviderRequest, ProviderResponse, ProviderStream, Role,
    StreamChunk, ToolCall, ToolCallDelta, Usage,
};
use crate::error::{AgentError, Result};
use crate::tools::ToolDefinition;
use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::debug;

/// Anthropic protocol version sent on every request.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Fallback output budget for models missing from the limits table
/// (hermes `_ANTHROPIC_DEFAULT_OUTPUT_LIMIT`).
const DEFAULT_OUTPUT_LIMIT: u32 = 128_000;

/// Placeholder for empty assistant text turns (hermes
/// `_EMPTY_TEXT_PLACEHOLDER`) — Anthropic rejects empty text blocks.
const EMPTY_TEXT_PLACEHOLDER: &str = "(empty)";

/// Per-model max output token ceilings (hermes `_ANTHROPIC_OUTPUT_LIMITS`,
/// longest-substring match wins).
const OUTPUT_LIMITS: &[(&str, u32)] = &[
    ("claude-fable", 128_000),
    ("claude-sonnet-5", 128_000),
    ("claude-opus-4-8", 128_000),
    ("claude-opus-4-7", 128_000),
    ("claude-opus-4-6", 128_000),
    ("claude-sonnet-4-6", 64_000),
    ("claude-opus-4-5", 64_000),
    ("claude-sonnet-4-5", 64_000),
    ("claude-haiku-4-5", 64_000),
    ("claude-opus-4", 32_000),
    ("claude-sonnet-4", 64_000),
    ("claude-3-7-sonnet", 128_000),
    ("claude-3-5-sonnet", 8_192),
    ("claude-3-5-haiku", 8_192),
    ("claude-3-opus", 4_096),
    ("claude-3-sonnet", 4_096),
    ("claude-3-haiku", 4_096),
    ("minimax", 131_072),
    ("qwen3", 65_536),
];

/// Resolve the `max_tokens` budget for a model (requested > configured >
/// table > default).  Anthropic rejects missing/non-positive budgets.
pub fn resolve_max_tokens(requested: Option<u32>, configured: Option<u32>, model: &str) -> u32 {
    if let Some(value) = requested.filter(|v| *v > 0) {
        return value;
    }
    if let Some(value) = configured.filter(|v| *v > 0) {
        return value;
    }
    output_limit_for(model)
}

fn output_limit_for(model: &str) -> u32 {
    let normalized = model.to_lowercase().replace('.', "-");
    let mut best_key = "";
    let mut best_val = DEFAULT_OUTPUT_LIMIT;
    for (key, val) in OUTPUT_LIMITS {
        if normalized.contains(key) && key.len() > best_key.len() {
            best_key = key;
            best_val = *val;
        }
    }
    best_val
}

/// Native Anthropic Messages provider.
pub struct AnthropicProvider {
    client: Client,
    endpoint: String,
    api_key: Option<String>,
    model: String,
    name: String,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
    max_retries: usize,
}

impl AnthropicProvider {
    pub fn builder() -> AnthropicProviderBuilder {
        AnthropicProviderBuilder::default()
    }

    /// Full `/v1/messages` URL for the configured endpoint.
    fn api_url(&self) -> String {
        let base = self.endpoint.trim_end_matches('/');
        if base.ends_with("/v1/messages") || base.ends_with("/messages") {
            base.to_string()
        } else if base.ends_with("/v1") {
            format!("{}/messages", base)
        } else {
            format!("{}/v1/messages", base)
        }
    }

    /// OAuth access tokens need bearer auth; regular API keys use
    /// `x-api-key` (hermes `_is_oauth_token` / `_requires_bearer_auth`).
    fn uses_bearer_auth(&self) -> bool {
        self.api_key
            .as_deref()
            .map(|k| k.starts_with("sk-ant-oat"))
            .unwrap_or(false)
    }

    fn retriable_status(status: reqwest::StatusCode) -> bool {
        matches!(status.as_u16(), 408 | 429 | 500 | 502 | 503 | 504)
    }

    fn retry_delay(attempt: usize) -> std::time::Duration {
        let base_ms = 500u64.saturating_mul(1 << attempt.min(4));
        let jitter_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| u64::from(d.subsec_nanos() % 250))
            .unwrap_or(0);
        std::time::Duration::from_millis(base_ms.min(8000) + jitter_ms)
    }

    /// POST with the same transient-retry policy as the OpenAI provider.
    async fn send_api_request(&self, url: &str, body: &Value) -> Result<reqwest::Response> {
        let mut last_error: Option<AgentError> = None;
        for attempt in 0..=self.max_retries {
            if attempt > 0 {
                let delay = Self::retry_delay(attempt - 1);
                debug!(
                    "anthropic retry {}/{} after {:?} ({})",
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
                .header("Content-Type", "application/json")
                .header("anthropic-version", ANTHROPIC_VERSION);
            if let Some(ref key) = self.api_key {
                if self.uses_bearer_auth() {
                    req_builder = req_builder.bearer_auth(key);
                } else {
                    req_builder = req_builder.header("x-api-key", key);
                }
            }
            let response = match req_builder.json(body).send().await {
                Ok(response) => response,
                Err(e) => {
                    last_error = Some(AgentError::Provider(format!("HTTP request failed: {}", e)));
                    continue;
                }
            };
            let status = response.status();
            if status.is_success() {
                return Ok(response);
            }
            let error_body = response.text().await.unwrap_or_default();
            let message = serde_json::from_str::<AnthropicError>(&error_body)
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

    fn build_body(&self, request: &ProviderRequest, stream: bool) -> Value {
        let (system, mut messages) = messages_to_anthropic(&request.messages);
        if let Some(ref images) = request.images {
            attach_images_anthropic(&mut messages, images);
        }
        let tools = tools_to_anthropic(&request.tools);
        let mut body = json!({
            "model": request.model,
            "max_tokens": resolve_max_tokens(request.max_tokens, self.max_tokens, &request.model),
            "messages": messages,
            "stream": stream,
        });
        if let Some(system) = system {
            body["system"] = json!(system);
        }
        if !tools.is_empty() {
            body["tools"] = json!(tools);
        }
        if let Some(temp) = request.temperature.or(self.temperature) {
            body["temperature"] = json!(temp);
        }
        if let Some(ref stop) = request.stop {
            if !stop.is_empty() {
                body["stop_sequences"] = json!(stop);
            }
        }
        body
    }
}

pub struct AnthropicProviderBuilder {
    endpoint: Option<String>,
    api_key: Option<String>,
    model: Option<String>,
    name: Option<String>,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
    max_retries: usize,
}

impl Default for AnthropicProviderBuilder {
    fn default() -> Self {
        Self {
            endpoint: None,
            api_key: None,
            model: None,
            name: None,
            max_tokens: None,
            temperature: None,
            max_retries: 2,
        }
    }
}

impl AnthropicProviderBuilder {
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

    pub fn max_retries(mut self, n: usize) -> Self {
        self.max_retries = n;
        self
    }

    pub fn build(self) -> Result<AnthropicProvider> {
        let endpoint = self
            .endpoint
            .unwrap_or_else(|| "https://api.anthropic.com".to_string());
        let model = self
            .model
            .unwrap_or_else(|| "claude-sonnet-4-5".to_string());
        Ok(AnthropicProvider {
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(600))
                .build()
                .map_err(|e| AgentError::Provider(format!("HTTP client: {}", e)))?,
            endpoint,
            api_key: self.api_key,
            model,
            name: self.name.unwrap_or_else(|| "anthropic".to_string()),
            max_tokens: self.max_tokens,
            temperature: self.temperature,
            max_retries: self.max_retries,
        })
    }
}

#[derive(Deserialize)]
struct AnthropicError {
    error: AnthropicErrorDetail,
}

#[derive(Deserialize)]
struct AnthropicErrorDetail {
    #[serde(default)]
    message: String,
}

// ---------------------------------------------------------------------------
// OpenAI-style → Anthropic conversion (hermes convert_tools_to_anthropic,
// _convert_assistant_message, _convert_tool_message_to_result)
// ---------------------------------------------------------------------------

/// Convert shared messages into `(system, messages)` for the Messages API.
///
/// - System messages move into the `system` parameter (joined with blank
///   lines).
/// - Assistant tool calls become `tool_use` blocks.
/// - Tool messages become `tool_result` blocks; consecutive tool results are
///   merged into one user turn (Anthropic requires alternating roles).
/// Attach images to the last user turn as anthropic base64 image blocks
/// (P226 native multimodal injection). Only `data:` URLs are injected —
/// http(s) image URLs stay path-referenced text (messaging always builds
/// data URLs from the media cache).
fn attach_images_anthropic(messages: &mut [Value], images: &[crate::provider::MessageImage]) {
    let mut blocks: Vec<Value> = Vec::new();
    for image in images {
        let Some(data) = image.data_url_base64() else {
            continue;
        };
        blocks.push(json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": image.resolved_media_type(),
                "data": data,
            },
        }));
    }
    if blocks.is_empty() {
        return;
    }
    let Some(idx) = messages.iter().rposition(|m| m["role"] == "user") else {
        return;
    };
    if let Some(arr) = messages[idx]
        .get_mut("content")
        .and_then(|c| c.as_array_mut())
    {
        for block in blocks {
            arr.push(block);
        }
    }
}

pub fn messages_to_anthropic(messages: &[Message]) -> (Option<String>, Vec<Value>) {
    let mut system_parts: Vec<String> = Vec::new();
    let mut out: Vec<Value> = Vec::new();

    for message in messages {
        match message.role {
            Role::System => {
                if let Some(ref content) = message.content {
                    if !content.trim().is_empty() {
                        system_parts.push(content.trim().to_string());
                    }
                }
            }
            Role::User => {
                let text = message.content.clone().unwrap_or_default();
                let block = if text.trim().is_empty() {
                    json!({"type": "text", "text": "(empty message)"})
                } else {
                    json!({"type": "text", "text": text})
                };
                if out.last().map(|m| m["role"] == "user").unwrap_or(false) {
                    out.last_mut()
                        .unwrap()
                        .get_mut("content")
                        .and_then(|c| c.as_array_mut())
                        .map(|blocks| blocks.push(block));
                } else {
                    out.push(json!({"role": "user", "content": [block]}));
                }
            }
            Role::Assistant => {
                let mut blocks: Vec<Value> = Vec::new();
                if let Some(ref content) = message.content {
                    if !content.trim().is_empty() {
                        blocks.push(json!({"type": "text", "text": content}));
                    }
                }
                if let Some(ref tool_calls) = message.tool_calls {
                    for call in tool_calls {
                        let input: Value =
                            serde_json::from_str(&call.function.arguments).unwrap_or(json!({}));
                        blocks.push(json!({
                            "type": "tool_use",
                            "id": call.id,
                            "name": call.function.name,
                            "input": input,
                        }));
                    }
                }
                if blocks.is_empty() {
                    blocks.push(json!({"type": "text", "text": EMPTY_TEXT_PLACEHOLDER}));
                }
                out.push(json!({"role": "assistant", "content": blocks}));
            }
            Role::Tool => {
                let content = message.content.clone().unwrap_or_default();
                let result_content = if content.trim().is_empty() {
                    "(no output)".to_string()
                } else {
                    content
                };
                let tool_result = json!({
                    "type": "tool_result",
                    "tool_use_id": message.tool_call_id.clone().unwrap_or_default(),
                    "content": result_content,
                });
                let last_is_result = out
                    .last()
                    .map(|m| {
                        m["role"] == "user"
                            && m["content"]
                                .as_array()
                                .and_then(|blocks| blocks.first())
                                .map(|first| first["type"] == "tool_result")
                                .unwrap_or(false)
                    })
                    .unwrap_or(false);
                if last_is_result {
                    out.last_mut()
                        .unwrap()
                        .get_mut("content")
                        .and_then(|c| c.as_array_mut())
                        .map(|blocks| blocks.push(tool_result));
                } else {
                    out.push(json!({"role": "user", "content": [tool_result]}));
                }
            }
        }
    }

    let system = if system_parts.is_empty() {
        None
    } else {
        Some(system_parts.join("\n\n"))
    };
    (system, out)
}

/// Convert tool definitions to Anthropic `tools` entries, dropping duplicate
/// names (Anthropic rejects duplicates — hermes #18478 guard).
pub fn tools_to_anthropic(tools: &[ToolDefinition]) -> Vec<Value> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for tool in tools {
        if tool.name.is_empty() || !seen.insert(tool.name.clone()) {
            continue;
        }
        let input_schema = if tool.parameters.is_null() {
            json!({"type": "object", "properties": {}})
        } else {
            tool.parameters.clone()
        };
        out.push(json!({
            "name": tool.name,
            "description": tool.description,
            "input_schema": input_schema,
        }));
    }
    out
}

// ---------------------------------------------------------------------------
// Response parsing
// ---------------------------------------------------------------------------

/// Map Anthropic `stop_reason` onto the shared finish_reason vocabulary.
pub fn map_stop_reason(stop_reason: Option<&str>) -> Option<String> {
    stop_reason.map(|reason| match reason {
        "end_turn" | "stop_sequence" => "stop".to_string(),
        "tool_use" => "tool_calls".to_string(),
        "max_tokens" => "length".to_string(),
        other => other.to_string(),
    })
}

/// Parse a non-streaming `/v1/messages` response body.
pub fn parse_anthropic_response(body: &Value) -> ProviderResponse {
    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut reasoning = String::new();
    for block in body
        .get("content")
        .and_then(|c| c.as_array())
        .unwrap_or(&vec![])
    {
        match block.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                    text_parts.push(text.to_string());
                }
            }
            Some("thinking") => {
                if let Some(thinking) = block.get("thinking").and_then(|t| t.as_str()) {
                    reasoning.push_str(thinking);
                }
            }
            Some("tool_use") => {
                let arguments = block
                    .get("input")
                    .map(|input| input.to_string())
                    .unwrap_or_else(|| "{}".to_string());
                tool_calls.push(ToolCall {
                    id: block
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    call_type: "function".to_string(),
                    function: FunctionCall {
                        name: block
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        arguments,
                    },
                });
            }
            _ => {}
        }
    }
    let usage = body.get("usage").map(|u| Usage {
        prompt_tokens: u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
        completion_tokens: u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
        total_tokens: 0,
    });
    let finish_reason = map_stop_reason(body.get("stop_reason").and_then(|v| v.as_str()));
    ProviderResponse {
        content: if text_parts.is_empty() {
            None
        } else {
            Some(text_parts.join("\n"))
        },
        tool_calls,
        usage,
        model: body
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        reasoning: if reasoning.is_empty() {
            None
        } else {
            Some(reasoning)
        },
        finish_reason,
    }
}

/// Parse one Anthropic streaming event into a `StreamChunk`.
/// Returns `(chunk, is_terminal)`.
pub fn parse_anthropic_event(data: &str) -> Result<Option<StreamChunk>> {
    let event: Value = serde_json::from_str(data)
        .map_err(|e| AgentError::Provider(format!("bad stream event: {}", e)))?;
    let event_type = event.get("type").and_then(|t| t.as_str()).unwrap_or("");
    let mut chunk = StreamChunk::default();
    match event_type {
        "message_start" => {
            if let Some(message) = event.get("message") {
                chunk.model = message
                    .get("model")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                if let Some(usage) = message.get("usage") {
                    chunk.usage = Some(Usage {
                        prompt_tokens: usage
                            .get("input_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0) as u32,
                        completion_tokens: 0,
                        total_tokens: 0,
                    });
                }
            }
        }
        "content_block_start" => {
            let index = event.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            if let Some(block) = event.get("content_block") {
                if block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                    chunk.tool_call_deltas.push(ToolCallDelta {
                        index,
                        id: block.get("id").and_then(|v| v.as_str()).map(String::from),
                        name_delta: block.get("name").and_then(|v| v.as_str()).map(String::from),
                        arguments_delta: None,
                    });
                }
            }
        }
        "content_block_delta" => {
            let index = event.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            if let Some(delta) = event.get("delta") {
                match delta.get("type").and_then(|t| t.as_str()) {
                    Some("text_delta") => {
                        chunk.delta_content =
                            delta.get("text").and_then(|v| v.as_str()).map(String::from);
                    }
                    Some("thinking_delta") => {
                        chunk.delta_reasoning = delta
                            .get("thinking")
                            .and_then(|v| v.as_str())
                            .map(String::from);
                    }
                    Some("input_json_delta") => {
                        chunk.tool_call_deltas.push(ToolCallDelta {
                            index,
                            id: None,
                            name_delta: None,
                            arguments_delta: delta
                                .get("partial_json")
                                .and_then(|v| v.as_str())
                                .map(String::from),
                        });
                    }
                    _ => {}
                }
            }
        }
        "message_delta" => {
            chunk.finish_reason = map_stop_reason(
                event
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(|v| v.as_str()),
            );
            if let Some(usage) = event.get("usage") {
                chunk.usage = Some(Usage {
                    prompt_tokens: 0,
                    completion_tokens: usage
                        .get("output_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as u32,
                    total_tokens: 0,
                });
            }
        }
        "message_stop" => {}
        "ping" | "error" | _ => {
            if event_type == "error" {
                let message = event
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("stream error");
                return Err(AgentError::Provider(format!("stream error: {}", message)));
            }
        }
    }
    Ok(Some(chunk))
}

#[async_trait]
impl Provider for AnthropicProvider {
    async fn chat_completion(&self, request: ProviderRequest) -> Result<ProviderResponse> {
        let url = self.api_url();
        debug!("Anthropic call to: {}", url);
        let body = self.build_body(&request, false);
        let response = self.send_api_request(&url, &body).await?;
        let text = response
            .text()
            .await
            .map_err(|e| AgentError::Provider(format!("read response: {}", e)))?;
        let value: Value = serde_json::from_str(&text)
            .map_err(|e| AgentError::Provider(format!("parse response: {} ({})", e, text)))?;
        let mut parsed = parse_anthropic_response(&value);
        if parsed.model.is_empty() {
            parsed.model = request.model.clone();
        }
        Ok(parsed)
    }

    async fn chat_completion_stream(&self, request: ProviderRequest) -> Result<ProviderStream> {
        let url = self.api_url();
        debug!("Anthropic streaming call to: {}", url);
        let body = self.build_body(&request, true);
        let response = self.send_api_request(&url, &body).await?;

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
                    None => return None,
                };
                let line = line.trim().to_string();
                if line.is_empty() || line.starts_with(':') || !line.starts_with("data:") {
                    continue;
                }
                let data = line.trim_start_matches("data:").trim();
                if data == "[DONE]" {
                    return None;
                }
                match parse_anthropic_event(data) {
                    Ok(Some(chunk)) => return Some((Ok(chunk), reader)),
                    Ok(None) => continue,
                    Err(e) => return Some((Err(e), reader)),
                }
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
        let source = if image_url.starts_with("data:") {
            let (header, data) = image_url.split_once(',').unwrap_or(("data:", ""));
            let media_type = header
                .trim_start_matches("data:")
                .split(';')
                .next()
                .unwrap_or("image/jpeg");
            json!({"type": "base64", "media_type": media_type, "data": data})
        } else {
            json!({"type": "url", "url": image_url})
        };
        let body = json!({
            "model": self.model,
            "max_tokens": resolve_max_tokens(None, self.max_tokens, &self.model),
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "image", "source": source},
                    {"type": "text", "text": prompt},
                ],
            }],
        });
        let response = self.send_api_request(&self.api_url(), &body).await?;
        let text = response
            .text()
            .await
            .map_err(|e| AgentError::Provider(format!("read response: {}", e)))?;
        let value: Value = serde_json::from_str(&text)
            .map_err(|e| AgentError::Provider(format!("parse response: {} ({})", e, text)))?;
        Ok(parse_anthropic_response(&value).content.unwrap_or_default())
    }
}

/// Line reader over an SSE byte stream (same shape as the OpenAI
/// provider's reader).
struct SseLineReader {
    stream: std::pin::Pin<
        Box<dyn futures::Stream<Item = std::result::Result<bytes::Bytes, reqwest::Error>> + Send>,
    >,
    buf: Vec<u8>,
    eof: bool,
}

impl SseLineReader {
    async fn next_line(&mut self) -> Option<String> {
        use futures::TryStreamExt;
        loop {
            if let Some(pos) = self.buf.iter().position(|b| *b == b'\n') {
                let line_bytes: Vec<u8> = self.buf.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line_bytes).to_string();
                return Some(line.trim_end_matches(['\n', '\r']).to_string());
            }
            if self.eof {
                if self.buf.is_empty() {
                    return None;
                }
                let line = String::from_utf8_lossy(&self.buf).to_string();
                self.buf.clear();
                return Some(line);
            }
            match self.stream.try_next().await {
                Ok(Some(chunk)) => self.buf.extend_from_slice(&chunk),
                Ok(None) => self.eof = true,
                Err(_) => self.eof = true,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: Role, content: &str) -> Message {
        Message {
            role,
            content: Some(content.to_string()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }

    #[test]
    fn test_messages_to_anthropic_system_and_roles() {
        let messages = vec![
            msg(Role::System, "You are helpful."),
            msg(Role::System, "Be brief."),
            msg(Role::User, "Hi"),
            msg(Role::Assistant, "Hello!"),
            msg(Role::User, "Again"),
        ];
        let (system, out) = messages_to_anthropic(&messages);
        assert_eq!(system.as_deref(), Some("You are helpful.\n\nBe brief."));
        assert_eq!(out.len(), 3);
        assert_eq!(out[0]["role"], "user");
        assert_eq!(out[0]["content"][0]["text"], "Hi");
        assert_eq!(out[1]["role"], "assistant");
        assert_eq!(out[2]["content"][0]["text"], "Again");
    }

    #[test]
    fn test_messages_to_anthropic_tool_flow() {
        let mut assistant = msg(Role::Assistant, "");
        assistant.tool_calls = Some(vec![ToolCall {
            id: "toolu_1".into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: "terminal".into(),
                arguments: "{\"command\":\"ls\"}".into(),
            },
        }]);
        let mut tool = msg(Role::Tool, "file.txt");
        tool.tool_call_id = Some("toolu_1".into());
        let mut tool2 = msg(Role::Tool, "");
        tool2.tool_call_id = Some("toolu_2".into());
        let messages = vec![
            msg(Role::User, "list files"),
            assistant,
            tool,
            tool2,
            msg(Role::User, "thanks"),
        ];
        let (system, out) = messages_to_anthropic(&messages);
        assert!(system.is_none());
        // user, assistant(tool_use), user(2 tool_results + trailing text merged
        // into one turn — Anthropic requires alternating roles)
        assert_eq!(out.len(), 3);
        let assistant_blocks = out[1]["content"].as_array().unwrap();
        assert_eq!(assistant_blocks.len(), 1);
        assert_eq!(assistant_blocks[0]["type"], "tool_use");
        assert_eq!(assistant_blocks[0]["name"], "terminal");
        assert_eq!(assistant_blocks[0]["input"]["command"], "ls");
        let results = out[2]["content"].as_array().unwrap();
        assert_eq!(results.len(), 3);
        assert_eq!(results[0]["type"], "tool_result");
        assert_eq!(results[0]["tool_use_id"], "toolu_1");
        assert_eq!(results[1]["content"], "(no output)");
        assert_eq!(results[2]["type"], "text");
        assert_eq!(results[2]["text"], "thanks");
    }

    #[test]
    fn test_tools_to_anthropic_dedup() {
        let tools = vec![
            ToolDefinition {
                name: "terminal".into(),
                description: "run".into(),
                parameters: json!({"type": "object"}),
            },
            ToolDefinition {
                name: "terminal".into(),
                description: "dup".into(),
                parameters: Value::Null,
            },
        ];
        let out = tools_to_anthropic(&tools);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["name"], "terminal");
        assert_eq!(out[0]["input_schema"]["type"], "object");
    }

    #[test]
    fn test_parse_response_text_and_tool_use() {
        let body = json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-5",
            "content": [
                {"type": "text", "text": "Running ls"},
                {"type": "tool_use", "id": "toolu_9", "name": "terminal",
                 "input": {"command": "ls -la"}},
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 10, "output_tokens": 5},
        });
        let response = parse_anthropic_response(&body);
        assert_eq!(response.content.as_deref(), Some("Running ls"));
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].id, "toolu_9");
        assert_eq!(
            response.tool_calls[0].function.arguments,
            "{\"command\":\"ls -la\"}"
        );
        assert_eq!(response.finish_reason.as_deref(), Some("tool_calls"));
        let usage = response.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 5);
    }

    #[test]
    fn test_stop_reason_mapping() {
        assert_eq!(map_stop_reason(Some("end_turn")).as_deref(), Some("stop"));
        assert_eq!(
            map_stop_reason(Some("stop_sequence")).as_deref(),
            Some("stop")
        );
        assert_eq!(
            map_stop_reason(Some("tool_use")).as_deref(),
            Some("tool_calls")
        );
        assert_eq!(
            map_stop_reason(Some("max_tokens")).as_deref(),
            Some("length")
        );
        assert_eq!(map_stop_reason(None), None);
    }

    #[test]
    fn test_resolve_max_tokens_precedence() {
        assert_eq!(
            resolve_max_tokens(Some(100), Some(200), "claude-sonnet-4-5"),
            100
        );
        assert_eq!(
            resolve_max_tokens(None, Some(200), "claude-sonnet-4-5"),
            200
        );
        assert_eq!(resolve_max_tokens(None, None, "claude-sonnet-4-5"), 64_000);
        assert_eq!(resolve_max_tokens(None, None, "claude-3-5-haiku"), 8_192);
        assert_eq!(
            resolve_max_tokens(None, None, "unknown-model-9"),
            DEFAULT_OUTPUT_LIMIT
        );
        assert_eq!(resolve_max_tokens(Some(0), None, "claude-3-opus"), 4_096);
    }

    #[test]
    fn test_parse_stream_events() {
        let start = parse_anthropic_event(
            r#"{"type":"message_start","message":{"model":"claude-sonnet-4-5","usage":{"input_tokens":7}}}"#,
        ).unwrap().unwrap();
        assert_eq!(start.model.as_deref(), Some("claude-sonnet-4-5"));
        assert_eq!(start.usage.unwrap().prompt_tokens, 7);

        let text = parse_anthropic_event(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"He"}}"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(text.delta_content.as_deref(), Some("He"));

        let tool_start = parse_anthropic_event(
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_5","name":"terminal"}}"#,
        ).unwrap().unwrap();
        assert_eq!(
            tool_start.tool_call_deltas[0].id.as_deref(),
            Some("toolu_5")
        );
        assert_eq!(
            tool_start.tool_call_deltas[0].name_delta.as_deref(),
            Some("terminal")
        );

        let args = parse_anthropic_event(
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"co"}}"#,
        ).unwrap().unwrap();
        assert_eq!(
            args.tool_call_deltas[0].arguments_delta.as_deref(),
            Some("{\"co")
        );
        assert_eq!(args.tool_call_deltas[0].index, 1);

        let delta = parse_anthropic_event(
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":12}}"#,
        ).unwrap().unwrap();
        assert_eq!(delta.finish_reason.as_deref(), Some("stop"));
        assert_eq!(delta.usage.unwrap().completion_tokens, 12);

        assert!(parse_anthropic_event(r#"{"type":"ping"}"#)
            .unwrap()
            .is_some());
        assert!(
            parse_anthropic_event(r#"{"type":"error","error":{"message":"overloaded"}}"#,).is_err()
        );
    }

    #[test]
    fn test_api_url_and_auth_mode() {
        let provider = AnthropicProvider::builder()
            .endpoint("https://api.anthropic.com")
            .api_key("sk-ant-api03-xyz")
            .model("claude-sonnet-4-5")
            .build()
            .unwrap();
        assert_eq!(provider.api_url(), "https://api.anthropic.com/v1/messages");
        assert!(!provider.uses_bearer_auth());

        let oauth = AnthropicProvider::builder()
            .endpoint("https://proxy.example.com/anthropic/v1")
            .api_key("sk-ant-oat01-token")
            .build()
            .unwrap();
        assert_eq!(
            oauth.api_url(),
            "https://proxy.example.com/anthropic/v1/messages"
        );
        assert!(oauth.uses_bearer_auth());
    }

    #[tokio::test]
    async fn test_sse_line_reader() {
        let chunks: Vec<std::result::Result<bytes::Bytes, reqwest::Error>> = vec![
            Ok(bytes::Bytes::from("data: a\n\ndata: b\n")),
            Ok(bytes::Bytes::from("data: c\n")),
        ];
        let mut reader = SseLineReader {
            stream: Box::pin(futures::stream::iter(chunks)),
            buf: Vec::new(),
            eof: false,
        };
        let mut lines = Vec::new();
        while let Some(line) = reader.next_line().await {
            lines.push(line);
        }
        assert_eq!(lines, vec!["data: a", "", "data: b", "data: c"]);
    }

    #[test]
    fn attach_images_anthropic_appends_base64_blocks() {
        let mut messages = vec![json!({
            "role": "user",
            "content": [{"type": "text", "text": "what is this?"}],
        })];
        let images = vec![crate::provider::MessageImage {
            url: "data:image/jpeg;base64,/9j/4AAQ".to_string(),
            media_type: Some("image/jpeg".to_string()),
        }];
        attach_images_anthropic(&mut messages, &images);
        let blocks = messages[0]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[1]["type"], "image");
        assert_eq!(blocks[1]["source"]["type"], "base64");
        assert_eq!(blocks[1]["source"]["media_type"], "image/jpeg");
        assert_eq!(blocks[1]["source"]["data"], "/9j/4AAQ");
    }

    #[test]
    fn attach_images_anthropic_skips_http_urls() {
        let mut messages = vec![json!({
            "role": "user",
            "content": [{"type": "text", "text": "hi"}],
        })];
        let images = vec![crate::provider::MessageImage {
            url: "https://example.com/cat.jpg".to_string(),
            media_type: None,
        }];
        attach_images_anthropic(&mut messages, &images);
        assert_eq!(messages[0]["content"].as_array().unwrap().len(), 1);
    }
}

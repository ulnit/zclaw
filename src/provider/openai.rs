//! OpenAI-compatible provider implementation
//!
//! Supports: OpenAI, Azure OpenAI, OpenRouter, DashScope (Alibaba), Ollama, llama.cpp,
//! and any other provider implementing the OpenAI Chat Completions API.

use super::reasoning_echo::{
    body_rejects_reasoning_content, reasoning_replay, ReasoningRejectionLog, ReasoningReplay,
};
use super::{
    FunctionCall, Message, Provider, ProviderRequest, ProviderResponse, ProviderStream, Role,
    ToolCall, Usage,
};
use crate::error::{AgentError, Result};
use crate::tools::ToolDefinition;
use async_trait::async_trait;
use futures::TryStreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{debug, warn};

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
    /// P3-1d: learned "this route rejects `reasoning_content`" (hermes
    /// `record_reasoning_field_rejection`). The provider instance *is* the route
    /// key — `name`/`model`/`endpoint` are immutable after `build()` — so
    /// per-instance state is per-route state; no map needed.
    reasoning_rejected: ReasoningRejectionLog,
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
    ///
    /// `529` ("overloaded", used by Anthropic and some aggregators/CDNs) is
    /// retried with the same backoff: hermes-agent classifies 503/529 alike as
    /// `overloaded` → backoff, not a turn-ending failure. On a mobile network
    /// behind a fronting proxy this is a common transient, so failing the turn
    /// instead of retrying is the wrong default.
    fn retriable_status(status: reqwest::StatusCode) -> bool {
        matches!(status.as_u16(), 408 | 429 | 500 | 502 | 503 | 504 | 529)
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
        api_request: &mut ApiRequest,
    ) -> Result<reqwest::Response> {
        // P3-1d: at most one learn-and-retry per call, so a repeated rejection
        // surfaces the error instead of looping.
        let mut learn_retried = false;
        'outer: loop {
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
                        last_error =
                            Some(AgentError::Provider(format!("HTTP request failed: {}", e)));
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
                // P3-1d: a strict schema just told us, by name, that it does not take
                // `reasoning_content`. Learn it, strip it (done inside), and restart the
                // attempt budget with the corrected request — exactly hermes' "record it
                // per (provider, host, model) and retry once; build_api_request re-shapes
                // api_messages without it".
                //
                // 🔴 Restart via the `'outer` label rather than `continue`: an inner
                // `continue` at `attempt == self.max_retries` would fall out of the for
                // loop and return `last_error` = None, i.e. the generic "request failed
                // after retries" — swallowing the real 400 we are trying to recover from.
                if !learn_retried
                    && self.learn_reasoning_rejection(status, &error_body, &mut *api_request)
                {
                    learn_retried = true;
                    continue 'outer;
                }
                return Err(AgentError::Provider(format!(
                    "API error ({}): {}",
                    status, message
                )));
            }
            return Err(last_error
                .unwrap_or_else(|| AgentError::Provider("request failed after retries".into())));
        }
    }

    /// The replay route for this provider, downgraded to [`ReasoningReplay::Off`]
    /// once this route has been *learned* to reject `reasoning_content` (P3-1d).
    ///
    /// This is hermes' "the next `build_api_request` re-shapes `api_messages`
    /// without them for this (provider, host, model) for the rest of the session".
    /// must-echo routes are never downgraded — see `learn_reasoning_rejection`.
    fn effective_replay(&self) -> ReasoningReplay {
        if self.reasoning_rejected.is_rejected() {
            return ReasoningReplay::Off;
        }
        reasoning_replay(&self.name, &self.model, &self.endpoint)
    }

    /// P3-1d: a 4xx that names `reasoning_content` as an unknown field means this
    /// route's schema does not accept it. Remember that for the route, strip the
    /// key from the outgoing request, and report `true` so the caller re-POSTs.
    ///
    /// Returns `false` (no retry) when any of these hold, each for a reason that
    /// would otherwise break a working provider or loop forever:
    /// * not a 4xx — hermes gates on `_status_ok` (400–499); 5xx/timeouts are
    ///   transient and belong to the existing retry path;
    /// * the route is **must-echo** — there a body naming `reasoning_content` is a
    ///   *value* complaint ("must not be empty"), never a schema lacking the field,
    ///   and dropping it would guarantee a 400 on every later tool turn
    ///   (hermes: `fields -= {RC}`);
    /// * we never actually sent the key — learning a field we did not send would
    ///   silently disable reasoning for the rest of the session;
    /// * the body names no such field, or is not an unknown-field rejection;
    /// * the rejection was **already** recorded — `record()` returns false, so
    ///   recovery can never loop (hermes returns an empty frozenset for this).
    fn learn_reasoning_rejection(
        &self,
        status: reqwest::StatusCode,
        error_body: &str,
        api_request: &mut ApiRequest,
    ) -> bool {
        if !status.is_client_error() {
            return false;
        }
        if reasoning_replay(&self.name, &self.model, &self.endpoint) == ReasoningReplay::MustEcho {
            return false;
        }
        let sent = api_request
            .messages
            .iter()
            .any(|m| m.reasoning_content.is_some());
        if !body_rejects_reasoning_content(error_body, sent) {
            return false;
        }
        if !self.reasoning_rejected.record() {
            return false; // already known → surface the error, do not loop
        }
        for m in api_request.messages.iter_mut() {
            m.reasoning_content = None;
        }
        warn!(
            "{} rejected replayed reasoning_content — dropping it for this route for the rest of the process",
            self.model
        );
        true
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

        // Outbound mTLS: present this host's client certificate when the
        // endpoint (C2 / metering gateway) is in scope. Misconfiguration
        // fails closed — we must never silently send unauthenticated
        // traffic to an endpoint that expects an identity.
        let builder = Client::builder().timeout(std::time::Duration::from_secs(120));
        let builder = crate::outbound_tls::apply_env(builder, &crate::outbound_tls::host_of(&endpoint))
            .map_err(|e| AgentError::config(format!("outbound mTLS: {e}")))?;
        let client = builder
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
            reasoning_rejected: ReasoningRejectionLog::new(),
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
    /// Replayed reasoning for this assistant turn — **only ever populated per the
    /// per-provider echo policy** (`provider::reasoning_echo`). `None` for strict
    /// and unknown routes, so those providers see exactly the wire they saw before
    /// P3-1b (their schemas are `additionalProperties: false` and 400/422 on an
    /// unexpected key — upstream #45655, #70233).
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_content: Option<String>,
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
    // 🔴 mobile-ffi 必需的上游补丁（与 zclaw 0.5.x 同款修复）：
    // reasoning 字段名跨 provider 不稳定。DeepSeek 原生用 `reasoning_content`，
    // 但经 OpenRouter/部分渠道（实测 deepseek-v4.1-flash）发的是 `reasoning`
    // + `reasoning_details`（数组 [{type,text,format,index}]），**从不发**
    // reasoning_content。只读 reasoning_content 会 100% 丢弃思考流——
    // 推理模型思考期（实测 16s、1930 字）正文为空、reasoning 又被丢，
    // 客户端整轮收不到任何增量 → 空气泡 / 触发回退。
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    reasoning_details: Option<Vec<ApiReasoningDetail>>,
    #[serde(default)]
    tool_calls: Option<Vec<ApiStreamToolCall>>,
}

/// reasoning_details[] 元素（OpenRouter 形态）：只取 text。
#[derive(Deserialize, Debug, Default)]
struct ApiReasoningDetail {
    #[serde(default)]
    text: Option<String>,
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
        // 🔴 reasoning 三来源合并（优先级 reasoning_content > reasoning >
        // reasoning_details[].text）：跨 provider 字段名不稳定，实测
        // deepseek-v4.1-flash 走 reasoning+reasoning_details、从不发
        // reasoning_content。只读单一字段 = 思考流 100% 丢弃（真机空气泡根因）。
        out.delta_reasoning = choice
            .delta
            .reasoning_content
            .or(choice.delta.reasoning)
            .or_else(|| {
                choice
                    .delta
                    .reasoning_details
                    .map(|details| details.into_iter().filter_map(|d| d.text).collect::<String>())
            });
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

/// Convert a domain message to its wire shape.
///
/// `replay` is the per-provider reasoning policy resolved once per request
/// ([`crate::provider::reasoning_echo::reasoning_replay`]). It applies **only to
/// assistant turns** — a stored `reasoning` on any other role is meaningless — and
/// [`ReasoningReplay::Off`] omits the key entirely, which is what strict-schema and
/// unknown providers require (see `reasoning_echo` module docs).
fn message_to_api(msg: &Message, replay: ReasoningReplay) -> ApiMessage {
    let reasoning_content = if msg.role == Role::Assistant {
        replay.wire_value(msg.reasoning.as_deref())
    } else {
        None
    };
    ApiMessage {
        role: msg.role.to_string(),
        content: msg.content.clone().map(serde_json::Value::String),
        reasoning_content,
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

        // Resolve the reasoning-replay route once per request (hermes
        // `_reasoning_replay_route`): deterministic in (provider, model, endpoint), so the
        // wire prefix stays byte-stable across turns — unless this route has already
        // been learned to reject the carrier (P3-1d), which downgrades it to Off.
        let replay = self.effective_replay();
        let mut api_messages: Vec<ApiMessage> = request
            .messages
            .iter()
            .map(|m| message_to_api(m, replay))
            .collect();
        if let Some(ref images) = request.images {
            attach_images(&mut api_messages, images);
        }

        let api_tools: Option<Vec<ApiTool>> = if request.tools.is_empty() {
            None
        } else {
            Some(request.tools.iter().map(tool_def_to_api).collect())
        };

        let mut api_request = ApiRequest {
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

        let response = self.send_api_request(&url, &mut api_request).await?;

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

        // Same route resolution as the non-streaming path — both must agree or a
        // fallback switch mid-session would change the wire shape.
        let replay = self.effective_replay();
        let mut api_messages: Vec<ApiMessage> = request
            .messages
            .iter()
            .map(|m| message_to_api(m, replay))
            .collect();
        if let Some(ref images) = request.images {
            attach_images(&mut api_messages, images);
        }
        let api_tools: Option<Vec<ApiTool>> = if request.tools.is_empty() {
            None
        } else {
            Some(request.tools.iter().map(tool_def_to_api).collect())
        };
        let mut api_request = ApiRequest {
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

        let response = self.send_api_request(&url, &mut api_request).await?;

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

    fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// P3-1e: expose the process-lifetime learner state to the agent layer
    /// (which owns the store + session id) for persistence into
    /// `model_config` under `reasoning_rejected_carriers`.
    fn learned_reasoning_rejection(&self) -> bool {
        self.reasoning_rejected.is_rejected()
    }

    /// P3-1e: pre-arm from a persisted rejection so a resumed session never
    /// re-learns with another 400 (hermes `get_session_model_config_value` →
    /// `rejected_reasoning_carriers` seeding at request-build time).
    fn seed_reasoning_rejection(&self) {
        self.reasoning_rejected.seed();
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

    fn assistant_msg(reasoning: Option<&str>) -> Message {
        Message {
            reasoning: reasoning.map(str::to_string),
            role: Role::Assistant,
            content: Some("answer".to_string()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }

    /// 🔴 P3-1b 护栏：strict / unknown 路线上 reasoning **绝不上 wire**。
    ///
    /// 这是 P3-1「绝不上 wire」护栏的收窄版——P3-1b 引入了 per-provider echo policy，
    /// must-echo 路线（DeepSeek/Kimi/MiMo）现在**必须**发 pad，但 strict 路线
    /// （Mistral/Groq/Cerebras 的 `additionalProperties:false` schema）和**未知路线**
    /// 仍必须一个 reasoning 键都不发，否则 400/422 弄坏今天能正常工作的 provider
    /// （upstream #45655/#70233）。ulnclaw 没有 hermes 的运行时 rejection 学习，
    /// 所以未知路线刻意保持沉默（见 reasoning_echo 模块文档）。
    #[test]
    fn reasoning_never_reaches_the_wire_on_off_routes() {
        for route in [ReasoningReplay::Off] {
            let api = message_to_api(&assistant_msg(Some("SECRET-CHAIN-OF-THOUGHT")), route);
            let json = serde_json::to_string(&api).expect("ApiMessage serializes");
            assert!(
                !json.contains("reasoning"),
                "route {route}: reasoning key leaked onto the wire: {json}"
            );
            assert!(
                !json.contains("SECRET-CHAIN-OF-THOUGHT"),
                "route {route}: reasoning *content* leaked: {json}"
            );
            // content 仍正常发送（护栏不能顺手把正文也丢掉）
            assert!(json.contains("answer"), "content must still serialize: {json}");
            assert!(json.contains("assistant"), "role must still serialize: {json}");
        }
    }

    /// must-echo 路线：每个 assistant turn 都带 reasoning_content；无文本时是 `" "`
    /// 而非 `""`（DeepSeek V4 拒绝空串，upstream #17341）。
    #[test]
    fn must_echo_route_pads_reasoning_content() {
        let api = message_to_api(&assistant_msg(Some("thought")), ReasoningReplay::MustEcho);
        let v = serde_json::to_value(&api).unwrap();
        assert_eq!(v["reasoning_content"], "thought");

        // 无 reasoning 的 turn 也必须带 pad（否则该路线 tool-call turn 400）
        let api = message_to_api(&assistant_msg(None), ReasoningReplay::MustEcho);
        let v = serde_json::to_value(&api).unwrap();
        assert_eq!(v["reasoning_content"], " ", "must pad with a single space, never empty");
    }

    /// content 路线：有文本才发 reasoning_content，空文本**不发**（不 pad）。
    #[test]
    fn content_route_sends_only_when_present() {
        let api = message_to_api(&assistant_msg(Some("thought")), ReasoningReplay::Content);
        assert_eq!(serde_json::to_value(&api).unwrap()["reasoning_content"], "thought");

        let api = message_to_api(&assistant_msg(None), ReasoningReplay::Content);
        let obj = serde_json::to_value(&api).unwrap();
        assert!(
            obj.get("reasoning_content").is_none(),
            "content route must omit the key when there is no text (skip_serializing_if)"
        );
    }

    /// reasoning 只对 assistant turn 生效；user/tool/system turn 即使存了 reasoning 也不发。
    #[test]
    fn reasoning_only_applies_to_assistant_turns() {
        let user = Message {
            reasoning: Some("should-not-leak".into()),
            role: Role::User,
            content: Some("hi".into()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        };
        for route in [
            ReasoningReplay::Off,
            ReasoningReplay::Content,
            ReasoningReplay::MustEcho,
        ] {
            let json = serde_json::to_string(&message_to_api(&user, route)).unwrap();
            assert!(
                !json.contains("should-not-leak"),
                "route {route}: non-assistant reasoning leaked: {json}"
            );
        }
    }

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
        // 529 "overloaded" — hermes classifies it with 503 as backoff-retryable.
        assert!(OpenAiProvider::retriable_status(
            StatusCode::from_u16(529).unwrap()
        ));
        assert!(!OpenAiProvider::retriable_status(StatusCode::BAD_REQUEST));
        assert!(!OpenAiProvider::retriable_status(StatusCode::UNAUTHORIZED));
        assert!(!OpenAiProvider::retriable_status(StatusCode::NOT_FOUND));
        // 402 billing / 413 payload / 403 blocked are deterministic — never retried.
        assert!(!OpenAiProvider::retriable_status(
            StatusCode::PAYMENT_REQUIRED
        ));
        assert!(!OpenAiProvider::retriable_status(
            StatusCode::PAYLOAD_TOO_LARGE
        ));
        assert!(!OpenAiProvider::retriable_status(StatusCode::FORBIDDEN));
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
                reasoning: None,
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
                reasoning_content: None,
                role: "system".to_string(),
                content: Some(serde_json::Value::String("sys".to_string())),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            },
            ApiMessage {
                reasoning_content: None,
                role: "user".to_string(),
                content: Some(serde_json::Value::String("what is this?".to_string())),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            },
            ApiMessage {
                reasoning_content: None,
                role: "assistant".to_string(),
                content: Some(serde_json::Value::String("hmm".to_string())),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            },
            ApiMessage {
                reasoning_content: None,
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
            reasoning_content: None,
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
            reasoning_content: None,
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

    // -----------------------------------------------------------------
    // P3-1d 端到端：运行时 rejection 学习（axum mock server）
    // -----------------------------------------------------------------

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    #[derive(Clone)]
    struct RejectMock {
        hits: Arc<AtomicUsize>,
        bodies: Arc<Mutex<Vec<String>>>,
        /// 每次响应体；元素含 "{REJECT}" 则回 400 点名，否则回 200。
        script: Vec<String>,
    }

    impl RejectMock {
        fn new(script: Vec<String>) -> Self {
            Self {
                hits: Arc::new(AtomicUsize::new(0)),
                bodies: Arc::new(Mutex::new(Vec::new())),
                script,
            }
        }
        fn hit_count(&self) -> usize {
            self.hits.load(Ordering::SeqCst)
        }
        fn body(&self, i: usize) -> String {
            self.bodies
                .lock()
                .unwrap()
                .get(i)
                .cloned()
                .unwrap_or_default()
        }
    }

    /// Start the mock server and return its address.
    ///
    /// 🔴 Must be `async` and awaited — calling `Handle::block_on` from inside a
    /// `#[tokio::test]` panics with "Cannot block the current thread from within a
    /// runtime", which would make every test here fail for a reason unrelated to
    /// the feature under test.
    async fn reject_server(st: RejectMock) -> std::net::SocketAddr {
        use axum::extract::State;
        use axum::routing::post;
        let app = axum::Router::new()
            .route(
                "/v1/chat/completions",
                post(|State(st): State<RejectMock>, body: String| async move {
                    let n = st.hits.fetch_add(1, Ordering::SeqCst);
                    st.bodies.lock().unwrap().push(body);
                    let item = st.script.get(n).cloned().unwrap_or_else(|| "{REJECT}".into());
                    if item == "{REJECT}" {
                        (
                            axum::http::StatusCode::BAD_REQUEST,
                            r#"{"error":{"message":"messages.1.reasoning_content: Extra inputs are not permitted"}}"#.to_string(),
                        )
                    } else if item == "{REJECT_UNNAMED}" {
                        (
                            axum::http::StatusCode::BAD_REQUEST,
                            r#"{"error":{"message":"Extra inputs are not permitted"}}"#.to_string(),
                        )
                    } else {
                        (axum::http::StatusCode::OK, item)
                    }
                }),
            )
            .with_state(st);
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(l, app).await.unwrap();
        });
        addr
    }

    const OK_BODY: &str = r#"{"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#;

    /// 带 reasoning 的历史：assistant turn 存了推理文本，Content 路线会把它发出去。
    fn reasoning_history() -> Vec<Message> {
        vec![
            Message {
                reasoning: None,
                role: Role::User,
                content: Some("hi".into()),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            },
            Message {
                reasoning: Some("PRIVATE-THOUGHT".into()),
                role: Role::Assistant,
                content: Some("prev".into()),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            },
            Message {
                reasoning: None,
                role: Role::User,
                content: Some("again".into()),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            },
        ]
    }

    fn req(model: &str) -> crate::provider::ProviderRequest {
        crate::provider::ProviderRequest {
            messages: reasoning_history(),
            tools: vec![],
            model: model.into(),
            max_tokens: None,
            temperature: None,
            stream: false,
            stop: None,
            images: None,
        }
    }

    /// 🔴 P3-1d 主路径：Content 路线发出 reasoning_content → 400 点名 →
    /// 学习 + 剥离 + 重试成功；此后该路线**永久**不再带该字段。
    #[tokio::test]
    async fn learned_rejection_strips_reasoning_and_retries() {
        let st = RejectMock::new(vec!["{REJECT}".into(), OK_BODY.into(), OK_BODY.into()]);
        let addr = reject_server(st.clone()).await;
        let provider = OpenAiProvider::builder()
            .endpoint(&format!("http://{addr}/v1"))
            .model("m")
            .name("openrouter") // Content 路线：确实会发 reasoning_content
            .max_retries(2)
            .build()
            .unwrap();

        // 前置断言：该路线在没学到 rejection 前确实发送 reasoning_content
        assert_eq!(provider.effective_replay(), ReasoningReplay::Content);

        let resp = provider.chat_completion(req("m")).await.unwrap();
        assert_eq!(resp.content.as_deref(), Some("ok"));

        assert_eq!(st.hit_count(), 2, "应恰好一次学习重试，不多不少");
        assert!(
            st.body(0).contains("reasoning_content"),
            "第一次请求必须带该字段（否则这次测试什么都没证明）: {}",
            st.body(0)
        );
        assert!(
            !st.body(1).contains("reasoning_content"),
            "重试请求必须已剥离: {}",
            st.body(1)
        );
        assert!(
            !st.body(1).contains("PRIVATE-THOUGHT"),
            "推理文本不得残留在重试请求里: {}",
            st.body(1)
        );
        assert!(provider.reasoning_rejected.is_rejected());

        // 学到之后，effective_replay 整段降级为 Off —— 后续 turn 无需再吃一次 400
        assert_eq!(provider.effective_replay(), ReasoningReplay::Off);
        provider.chat_completion(req("m")).await.unwrap();
        assert_eq!(st.hit_count(), 3, "第三次调用不应再触发重试");
        assert!(
            !st.body(2).contains("reasoning_content"),
            "已学习的路线永久不再发该字段: {}",
            st.body(2)
        );
    }

    /// 🔴 不能死循环：服务端**每次**都拒绝时，恰好学习一次、重试一次，
    /// 然后把**真实错误**抛出去——绝不能退化成 "request failed after retries"。
    #[tokio::test]
    async fn repeat_rejection_surfaces_the_real_error_without_looping() {
        let st = RejectMock::new(vec![
            "{REJECT}".into(),
            "{REJECT}".into(),
            "{REJECT}".into(),
        ]);
        let addr = reject_server(st.clone()).await;
        let provider = OpenAiProvider::builder()
            .endpoint(&format!("http://{addr}/v1"))
            .model("m")
            .name("openrouter")
            .max_retries(2)
            .build()
            .unwrap();

        let err = provider
            .chat_completion(req("m"))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(st.hit_count(), 2, "只允许一次学习重试，不能循环");
        assert!(
            err.contains("Extra inputs are not permitted"),
            "必须把真实 400 抛出去，不能被吞成通用重试耗尽文案: {err}"
        );
        assert!(
            !err.contains("request failed after retries"),
            "'outer 标签重启若写错就会退化成这句，真实错误被吞掉: {err}"
        );
    }

    /// must-echo 路线**永不**被学习掉：那里点名 reasoning_content 是「值不能为空」
    /// 之类的抱怨，不是 schema 缺字段；剥掉它会让后续每个 tool turn 都 400。
    #[tokio::test]
    async fn must_echo_route_is_never_learned_away() {
        let st = RejectMock::new(vec!["{REJECT}".into()]);
        let addr = reject_server(st.clone()).await;
        let provider = OpenAiProvider::builder()
            .endpoint(&format!("http://{addr}/v1"))
            .model("m")
            .name("deepseek") // MustEcho 路线
            .max_retries(2)
            .build()
            .unwrap();
        assert_eq!(provider.effective_replay(), ReasoningReplay::MustEcho);

        let err = provider
            .chat_completion(req("m"))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(st.hit_count(), 1, "must-echo 路线不重试");
        assert!(
            !provider.reasoning_rejected.is_rejected(),
            "must-echo 路线不学习"
        );
        assert_eq!(
            provider.effective_replay(),
            ReasoningReplay::MustEcho,
            "路线不得被降级"
        );
        assert!(err.contains("Extra inputs"), "真实错误应抛出: {err}");
    }

    /// 没发过该字段就学不到：否则等于凭空把整条路线的 reasoning 永久关掉。
    /// 用 strict 路线构造「确实没发过」——上游 `_DEFAULT_CARRIERS` 让未知路线
    /// 也会发，所以这里不能再用未知路线当前提。
    #[tokio::test]
    async fn rejection_is_not_learned_when_the_field_was_never_sent() {
        let st = RejectMock::new(vec!["{REJECT}".into()]);
        let addr = reject_server(st.clone()).await;
        let provider = OpenAiProvider::builder()
            .endpoint(&format!("http://{addr}/v1"))
            .model("m")
            .name("mistral") // strict 路线 → Off，根本不发该字段
            .max_retries(2)
            .build()
            .unwrap();
        assert_eq!(provider.effective_replay(), ReasoningReplay::Off);

        provider.chat_completion(req("m")).await.unwrap_err();
        assert_eq!(st.hit_count(), 1, "没发过就不该重试");
        assert!(
            !provider.reasoning_rejected.is_rejected(),
            "学一个自己没发过的字段＝谎报，会永久关掉 reasoning"
        );
        assert!(
            !st.body(0).contains("reasoning_content"),
            "前提核对：确实没发"
        );
    }

    /// 未点名的 unknown-field 错误不学习：那个字段可能是别的，猜错就白关 reasoning。
    #[tokio::test]
    async fn unnamed_rejection_is_not_learned() {
        let st = RejectMock::new(vec!["{REJECT_UNNAMED}".into()]);
        let addr = reject_server(st.clone()).await;
        let provider = OpenAiProvider::builder()
            .endpoint(&format!("http://{addr}/v1"))
            .model("m")
            .name("openrouter")
            .max_retries(2)
            .build()
            .unwrap();

        provider.chat_completion(req("m")).await.unwrap_err();
        assert_eq!(st.hit_count(), 1, "未点名不重试");
        assert!(!provider.reasoning_rejected.is_rejected());
        assert!(
            st.body(0).contains("reasoning_content"),
            "前提核对：这次确实发了该字段，只是错误没点名"
        );
    }

    /// 🔴 上游完整闭环（此前被我用「刻意偏离」跳过的部分）：未知 provider 走
    /// `_DEFAULT_CARRIERS = {reasoning_content}` 发出该字段；若它的 schema 拒绝，
    /// 学习器当场剥离并重试，此后该路线降级为不发。没有学习器，这个默认就会
    /// 弄坏今天能用的 provider —— 两半必须同时在场。
    #[tokio::test]
    async fn unknown_provider_sends_by_default_and_self_corrects_on_rejection() {
        let st = RejectMock::new(vec!["{REJECT}".into(), OK_BODY.into(), OK_BODY.into()]);
        let addr = reject_server(st.clone()).await;
        let provider = OpenAiProvider::builder()
            .endpoint(&format!("http://{addr}/v1"))
            .model("m")
            .name("brand-new-provider") // 未知路线
            .max_retries(2)
            .build()
            .unwrap();

        // 前提：未知路线默认发 reasoning_content（上游 _DEFAULT_CARRIERS）
        assert_eq!(
            provider.effective_replay(),
            ReasoningReplay::Content,
            "unknown routes must default to the de-facto carrier"
        );

        let resp = provider.chat_completion(req("m")).await.unwrap();
        assert_eq!(resp.content.as_deref(), Some("ok"));

        assert_eq!(st.hit_count(), 2, "恰好一次学习重试");
        assert!(
            st.body(0).contains("reasoning_content"),
            "首次请求必须发出该字段（这才是上游默认）: {}",
            st.body(0)
        );
        assert!(
            !st.body(1).contains("reasoning_content"),
            "重试必须已剥离: {}",
            st.body(1)
        );
        // 学到之后该路线永久降级
        assert_eq!(provider.effective_replay(), ReasoningReplay::Off);
        provider.chat_completion(req("m")).await.unwrap();
        assert_eq!(st.hit_count(), 3, "第三次调用不该再触发重试");
        assert!(
            !st.body(2).contains("reasoning_content"),
            "已学习的路线不再发: {}",
            st.body(2)
        );
    }
}

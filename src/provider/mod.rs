//! Provider abstraction layer - supports multiple AI backends
//!
//! Inspired by Hermes Agent's runtime_provider.py which maps (provider, model)
//! to (api_mode, api_key, base_url).

pub mod anthropic;
pub mod anthropic_replay;
pub mod anthropic_thinking;
pub mod auxiliary;
pub mod error_class;
pub mod openai;
pub mod reasoning_echo;

use crate::error::{AgentError, Result};
use crate::tools::ToolDefinition;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

/// Message in a conversation
///
/// `Default` exists so construction sites can write only the fields they care
/// about (`..Default::default()`), which keeps adding an optional field from
/// touching all ~159 literals. It is internal Rust convenience — upstream
/// messages are dicts and have no equivalent, so this is not a contract surface.
/// A default `Message` is `role: User` with every payload field `None`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Name of the tool (for tool role messages)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Reasoning/thinking text for an assistant turn (extended-thinking models).
    ///
    /// Persisted so a session's train of thought survives reload/export instead
    /// of being discarded after streaming (hermes v0.22.2 replays prior
    /// reasoning across resume/branch/compaction). 🔴 This is **storage only**:
    /// it is deliberately NOT put on the provider wire — must-echo providers
    /// (DeepSeek/Kimi/MiMo) 400 *without* a reasoning pad while strict ones
    /// (Mistral/Groq/Cerebras) 400/422 *with* it, so replay needs hermes'
    /// per-provider echo policy. Sending it unconditionally would break
    /// providers that work today.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    /// Preserved Anthropic thinking/redacted_thinking blocks with their opaque
    /// `signature`/`data` fields (hermes `msg["reasoning_details"]`,
    /// `chat_completion_helpers.py:1688-1699`). Whitelisted at capture
    /// (`_sanitize_replay_block`) so output-only SDK fields never persist.
    /// Persisted as a JSON column so signatures survive resume; replay is
    /// Anthropic-only and route-gated (`_manage_thinking_signatures`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_details: Option<Vec<Value>>,
    /// Ordered sanitized block sidecar for turns interleaving SIGNED thinking
    /// with tool_use (hermes `msg["anthropic_content_blocks"]`): the parallel
    /// content/tool_calls lists reconstruct that shape in the wrong order and
    /// replay → HTTP 400. Runtime-only — upstream has no DB column for it
    /// (added then removed); cross-process replay falls back to
    /// `reasoning_details`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anthropic_content_blocks: Option<Vec<Value>>,
}

/// Role in a conversation
///
/// `Default = User`. Upstream has no `Message` type at all (messages are plain
/// dicts), so there is no struct default to copy; the nearest upstream signal is
/// its helper signatures, which all default to `role: str = "user"`
/// (`codex_responses_adapter._input_image_part`,
/// `_chat_content_to_responses_parts`, `plugins.…`, `hermes_state_messages.
/// latest_message_row_id`). It is also the most common role in this port's own
/// construction sites. Deriving on the first variant instead (`System`) would
/// make a default `Message` a system message, which is never the intent.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    #[default]
    User,
    Assistant,
    Tool,
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Role::System => write!(f, "system"),
            Role::User => write!(f, "user"),
            Role::Assistant => write!(f, "assistant"),
            Role::Tool => write!(f, "tool"),
        }
    }
}

/// A tool call requested by the model
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type", default = "default_function_type")]
    pub call_type: String,
    pub function: FunctionCall,
}

fn default_function_type() -> String {
    "function".to_string()
}

/// Function call details
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

/// Request to a provider
#[derive(Debug, Clone)]
pub struct ProviderRequest {
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub model: String,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub stream: bool,
    pub stop: Option<Vec<String>>,
    /// Images attached to the LAST user message — native multimodal
    /// user-turn injection (hermes media-injection parity, P226).
    /// Providers serialize them as content parts; vision-less providers
    /// ignore them (the text turn still carries the path references).
    pub images: Option<Vec<MessageImage>>,
    /// Resolved reasoning/thinking configuration for this request
    /// (hermes `reasoning_config`, `hermes_constants.resolve_reasoning_config`).
    ///
    /// 🔴 Lives on the COMMON request, not on the Anthropic provider: upstream
    /// threads `reasoning_config` through both the chat-completions path
    /// (`chat_completion_helpers.py:1141`) and the Anthropic path (`:1534`), and
    /// per-request plumbing is what lets the CLI `--reasoning` flag and kanban's
    /// per-task `reasoning_effort` pin override the profile default for ONE call.
    /// Putting it on the provider struct instead would lose that override.
    ///
    /// `Unresolved` (the default) means "no opinion": the Anthropic wire layer
    /// emits no `thinking` parameter, byte-identical to before this field.
    pub reasoning_config: crate::provider::anthropic_thinking::ReasoningConfig,
}

/// Image attached natively to a user turn (P226 multimodal injection).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MessageImage {
    /// `http(s)` URL or a `data:` URL (base64). Messaging turns use
    /// data URLs built from the media cache.
    pub url: String,
    /// Media type — required by block-based providers (anthropic
    /// base64 image blocks); inferred from a data-URL prefix when
    /// omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
}

impl MessageImage {
    /// Resolved media type: explicit field first, then the data-URL
    /// prefix, then a generic image fallback.
    pub fn resolved_media_type(&self) -> String {
        if let Some(ref mt) = self.media_type {
            if !mt.trim().is_empty() {
                return mt.trim().to_string();
            }
        }
        if let Some(rest) = self.url.strip_prefix("data:") {
            if let Some((mt, _)) = rest.split_once(';') {
                if !mt.is_empty() {
                    return mt.to_string();
                }
            }
        }
        "image/png".to_string()
    }

    /// Base64 payload of a `data:` URL (without the prefix), when this
    /// image is inline data.
    pub fn data_url_base64(&self) -> Option<&str> {
        let rest = self.url.strip_prefix("data:")?;
        let (_, data) = rest.split_once(",")?;
        Some(data)
    }
}

/// Response from a provider
#[derive(Debug, Clone)]
pub struct ProviderResponse {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<Usage>,
    pub model: String,
    /// Reasoning/thinking content (for models that support extended thinking)
    pub reasoning: Option<String>,
    /// Preserved thinking blocks with opaque signature/data (hermes
    /// `provider_data["reasoning_details"]`), whitelisted at capture.
    pub reasoning_details: Option<Vec<Value>>,
    /// Ordered block sidecar when signed thinking interleaves with tool_use
    /// (hermes `provider_data["anthropic_content_blocks"]`).
    pub anthropic_content_blocks: Option<Vec<Value>>,
    /// Finish reason: "stop", "tool_calls", "length", etc.
    pub finish_reason: Option<String>,
}

/// Token usage statistics
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

impl Usage {
    pub fn merge(&mut self, other: &Usage) {
        self.prompt_tokens += other.prompt_tokens;
        self.completion_tokens += other.completion_tokens;
        self.total_tokens += other.total_tokens;
    }
}

/// Incremental tool-call fragment in a streaming response (OpenAI chunk
/// `choices[0].delta.tool_calls[i]`).
#[derive(Debug, Clone, Default)]
pub struct ToolCallDelta {
    /// Position of this tool call within the message.
    pub index: usize,
    pub id: Option<String>,
    /// Function name fragment (usually arrives in one piece).
    pub name_delta: Option<String>,
    /// JSON arguments fragment (arrives incrementally).
    pub arguments_delta: Option<String>,
}

/// One chunk of a streaming chat completion.
#[derive(Debug, Clone, Default)]
pub struct StreamChunk {
    /// Content (text) delta.
    pub delta_content: Option<String>,
    /// Reasoning/thinking delta (models with extended thinking).
    pub delta_reasoning: Option<String>,
    /// Tool-call fragments.
    pub tool_call_deltas: Vec<ToolCallDelta>,
    /// Set on the final chunk: "stop", "tool_calls", "length", ...
    pub finish_reason: Option<String>,
    /// Usage (final chunk when `stream_options.include_usage`).
    pub usage: Option<Usage>,
    /// Model echoed by the server.
    pub model: Option<String>,
    /// Anthropic `content_block_start` for a thinking/redacted_thinking block:
    /// `(index, sanitized block)`. The signature arrives later via
    /// `signature_delta`; the accumulator merges them (hermes
    /// `relay_llm._ANTHROPIC_APPEND_DELTAS`).
    pub thinking_block_start: Option<(usize, Value)>,
    /// Anthropic `signature_delta`: `(block index, signature fragment)`.
    /// Fragments append to the block's `signature` field.
    pub signature_delta: Option<(usize, String)>,
    /// Anthropic `content_block_start` for a TEXT block: `(index, text prefix)`.
    /// Needed only so the ordered replay sidecar preserves text/thinking/tool_use
    /// order; `delta_content` already carries the same text for display.
    pub text_block_start: Option<(usize, String)>,
}

/// Accumulate streamed tool-call deltas into complete tool calls.
pub fn assemble_tool_calls(deltas: &[ToolCallDelta]) -> Vec<ToolCall> {
    let mut by_index: std::collections::BTreeMap<usize, (String, String, String)> =
        std::collections::BTreeMap::new();
    for delta in deltas {
        let entry = by_index
            .entry(delta.index)
            .or_insert_with(|| (String::new(), String::new(), String::new()));
        if let Some(id) = &delta.id {
            entry.0 = id.clone();
        }
        if let Some(name) = &delta.name_delta {
            entry.1.push_str(name);
        }
        if let Some(args) = &delta.arguments_delta {
            entry.2.push_str(args);
        }
    }
    by_index
        .into_values()
        .map(|(id, name, arguments)| ToolCall {
            id,
            call_type: "function".to_string(),
            function: FunctionCall { name, arguments },
        })
        .collect()
}

/// A boxed provider stream.
pub type ProviderStream =
    std::pin::Pin<Box<dyn futures::Stream<Item = Result<StreamChunk>> + Send>>;

/// Provider trait - abstraction for AI model backends
///
/// Implementations must be Send + Sync for use across async tasks.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Send a chat completion request
    async fn chat_completion(&self, request: ProviderRequest) -> Result<ProviderResponse>;

    /// Streaming chat completion (SSE chunks).  Default: unsupported —
    /// callers fall back to `chat_completion`.
    async fn chat_completion_stream(&self, _request: ProviderRequest) -> Result<ProviderStream> {
        Err(AgentError::provider(
            "this provider does not support streaming (chat_completion_stream)",
        ))
    }

    /// Whether `chat_completion_stream` is implemented.
    fn supports_streaming(&self) -> bool {
        false
    }

    /// P625: whether this provider accepts a service-tier override
    /// (hermes Priority Processing / `/fast`). Default: unsupported.
    fn supports_service_tier(&self) -> bool {
        false
    }

    /// P625: set or clear the runtime service-tier override. `Some(tier)`
    /// (e.g. "priority") applies to every subsequent request; `None`
    /// returns to the provider's configured default. Default: no-op.
    fn set_service_tier(&self, _tier: Option<String>) {}

    /// P625: the service tier in effect right now (override wins over the
    /// build-time pin). `None` = the endpoint's own default.
    fn active_service_tier(&self) -> Option<String> {
        None
    }

    /// Get the model name
    fn model(&self) -> &str;

    /// Get the provider name
    fn name(&self) -> &str;

    /// Check if provider is available
    async fn is_available(&self) -> bool {
        true
    }

    /// Analyze an image (vision). `image_url` is either an http(s) URL or a
    /// data: URL. Default: unsupported.
    async fn analyze_image(&self, _prompt: &str, _image_url: &str) -> Result<String> {
        Err(AgentError::provider(
            "this provider does not support vision (analyze_image)",
        ))
    }

    /// Analyze a video (hermes `video_analyze`). `video_data_url` is a
    /// base64 `data:` URL (providers that accept video do so inline).
    /// Default: unsupported.
    async fn analyze_video(&self, _prompt: &str, _video_data_url: &str) -> Result<String> {
        Err(AgentError::provider(
            "this provider does not support video analysis (analyze_video)",
        ))
    }

    /// The base URL this provider talks to (hermes route key component).
    /// Default: empty — providers that participate in reasoning-echo route
    /// resolution must override it, because the P3-1e persistence layer keys
    /// stored rejections by `(name, host_of(endpoint), model)` and an empty
    /// endpoint would make every default-impl provider share one bogus route.
    fn endpoint(&self) -> &str {
        ""
    }

    /// P3-1e: whether this route has *learned* (this process) that the wire
    /// rejects replayed reasoning. The agent reads it after a call to persist
    /// the rejection into the session's `model_config` (hermes
    /// `record_reasoning_field_rejection` → session KV). Default: never learns.
    fn learned_reasoning_rejection(&self) -> bool {
        false
    }

    /// P3-1e: seed a previously persisted rejection so a resumed session
    /// starts with the route already downgraded — hermes' "a resumed process
    /// never re-learns with another 400". Default: no-op (providers without
    /// the learner have nothing to seed).
    fn seed_reasoning_rejection(&self) {}
}

/// Provider configuration for dynamic instantiation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub name: String,
    pub kind: ProviderKind,
    pub endpoint: String,
    pub api_key: Option<String>,
    pub model: String,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    /// Fallback providers (by name)
    #[serde(default)]
    pub fallback_providers: Vec<String>,
}

/// Supported provider kinds
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// OpenAI-compatible endpoints (OpenAI, OpenRouter, DashScope, etc.)
    OpenAiCompatible,
    /// Ollama local models
    Ollama,
    /// llama.cpp server
    LlamaCpp,
    /// Anthropic Claude
    Anthropic,
    /// Local model (embedded)
    Local,
}

impl fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProviderKind::OpenAiCompatible => write!(f, "openai_compatible"),
            ProviderKind::Ollama => write!(f, "ollama"),
            ProviderKind::LlamaCpp => write!(f, "llama_cpp"),
            ProviderKind::Anthropic => write!(f, "anthropic"),
            ProviderKind::Local => write!(f, "local"),
        }
    }
}

impl ProviderConfig {
    /// Build a Provider from this config
    pub fn build(&self) -> Result<Box<dyn Provider>> {
        match self.kind {
            ProviderKind::OpenAiCompatible | ProviderKind::Ollama | ProviderKind::LlamaCpp => {
                let mut builder = openai::OpenAiProvider::builder()
                    .endpoint(&self.endpoint)
                    .model(&self.model)
                    .name(&self.name);

                if let Some(ref key) = self.api_key {
                    builder = builder.api_key(key);
                }
                if let Some(max_tokens) = self.max_tokens {
                    builder = builder.max_tokens(max_tokens);
                }
                if let Some(temp) = self.temperature {
                    builder = builder.temperature(temp);
                }

                Ok(Box::new(builder.build()?))
            }
            ProviderKind::Anthropic => {
                let mut builder = anthropic::AnthropicProvider::builder()
                    .endpoint(&self.endpoint)
                    .model(&self.model)
                    .name(&self.name);

                if let Some(ref key) = self.api_key {
                    builder = builder.api_key(key);
                }
                if let Some(max_tokens) = self.max_tokens {
                    builder = builder.max_tokens(max_tokens);
                }
                if let Some(temp) = self.temperature {
                    builder = builder.temperature(temp);
                }

                Ok(Box::new(builder.build()?))
            }
            ProviderKind::Local => Err(AgentError::config("Local provider not yet implemented")),
        }
    }
}

#[cfg(test)]
mod message_image_tests {
    use super::*;

    #[test]
    fn resolved_media_type_prefers_explicit_field() {
        let image = MessageImage {
            url: "data:image/jpeg;base64,AAA".to_string(),
            media_type: Some("image/webp".to_string()),
        };
        assert_eq!(image.resolved_media_type(), "image/webp");
    }

    #[test]
    fn resolved_media_type_infers_from_data_url() {
        let image = MessageImage {
            url: "data:image/png;base64,AAA".to_string(),
            media_type: None,
        };
        assert_eq!(image.resolved_media_type(), "image/png");
        // Blank field falls through to inference too.
        let blank = MessageImage {
            url: "data:image/gif;base64,AAA".to_string(),
            media_type: Some("  ".to_string()),
        };
        assert_eq!(blank.resolved_media_type(), "image/gif");
    }

    #[test]
    fn resolved_media_type_fallback_for_http_urls() {
        let image = MessageImage {
            url: "https://example.com/cat.jpg".to_string(),
            media_type: None,
        };
        assert_eq!(image.resolved_media_type(), "image/png");
    }

    #[test]
    fn data_url_base64_extracts_payload() {
        let image = MessageImage {
            url: "data:image/png;base64,aGVsbG8=".to_string(),
            media_type: None,
        };
        assert_eq!(image.data_url_base64(), Some("aGVsbG8="));
        let http = MessageImage {
            url: "https://example.com/cat.jpg".to_string(),
            media_type: None,
        };
        assert_eq!(http.data_url_base64(), None);
    }
}

#[cfg(test)]
mod message_default_tests {
    use super::*;

    /// 🔴 Pins `Role::default() == User`. A `Message::default()` whose role were
    /// `System` or `Assistant` would silently turn every `..Default::default()`
    /// construction site into a message that is replayed as a system/assistant
    /// turn. Upstream helpers default to `role: str = "user"`.
    #[test]
    fn role_default_is_user() {
        assert_eq!(Role::default(), Role::User);
    }

    /// `Message::default()` is a bare user message: role set, every payload
    /// field `None`. Pinned because ~159 construction sites rely on the
    /// remaining fields defaulting, and a future non-`Option` field would turn
    /// that into a compile error here rather than silent garbage downstream.
    #[test]
    fn message_default_is_a_bare_user_message() {
        let m = Message::default();
        assert_eq!(m.role, Role::User);
        assert!(m.content.is_none());
        assert!(m.tool_calls.is_none());
        assert!(m.tool_call_id.is_none());
        assert!(m.name.is_none());
        assert!(m.reasoning.is_none());
        // 🔴 `content` is the ONLY Option field on `Message` without
        // `skip_serializing_if` (see the struct above), so it always serializes
        // — as `null` here. That is pre-existing wire behaviour (upstream message
        // dicts always carry a `content` key) and deliberately NOT changed by the
        // P3-1c work: this test pins it, so adding `signature` must not silently
        // start emitting (or start omitting) keys.
        let json = serde_json::to_string(&m).unwrap();
        assert_eq!(json, r#"{"role":"user","content":null}"#);
    }

    /// The reason `Default` exists: a construction site may set only the fields
    /// it cares about, so adding an optional field never touches the ~159
    /// literals. This is the pattern the P3-1c signature field will rely on.
    #[test]
    fn default_spread_lets_a_site_set_only_its_fields() {
        let m = Message {
            role: Role::Assistant,
            content: Some("hi".into()),
            ..Default::default()
        };
        assert_eq!(m.role, Role::Assistant);
        assert_eq!(m.content.as_deref(), Some("hi"));
        assert!(m.reasoning.is_none(), "unset fields stay None");
    }
}

//! Provider abstraction layer - supports multiple AI backends
//!
//! Inspired by Hermes Agent's runtime_provider.py which maps (provider, model)
//! to (api_mode, api_key, base_url).

pub mod anthropic;
pub mod auxiliary;
pub mod openai;

use crate::error::{AgentError, Result};
use crate::tools::ToolDefinition;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Message in a conversation
#[derive(Debug, Clone, Serialize, Deserialize)]
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
}

/// Role in a conversation
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
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

# Provider System Guide

Complete guide to ulnclaw's provider abstraction and implementation.

## Table of Contents

- [Overview](#overview)
- [Provider Trait](#provider-trait)
- [OpenAI Provider](#openai-provider)
- [Supported Providers](#supported-providers)
- [Implementing Custom Providers](#implementing-custom-providers)
- [Provider Configuration](#provider-configuration)
- [Outbound mTLS (Client Certificates)](#outbound-mtls-client-certificates)
- [Error Handling](#error-handling)
- [Best Practices](#best-practices)
- [Auxiliary Model Routing](#auxiliary-model-routing)
- [Mixture of Agents (MoA)](#mixture-of-agents-moa)
- [Provider Fallback Chain](#provider-fallback-chain)

## Overview

ulnclaw's provider system abstracts AI model backends behind a common trait, allowing seamless switching between different providers.

**Key Features:**
- Unified `Provider` trait for all backends
- OpenAI-compatible implementation (covers most providers)
- Builder pattern for configuration
- Async/await for non-blocking I/O
- Comprehensive error handling

## Provider Trait

The core abstraction for AI model providers:

```rust
#[async_trait]
pub trait Provider: Send + Sync {
    /// Send a chat completion request
    async fn chat_completion(&self, request: ProviderRequest) -> Result<ProviderResponse>;
    
    /// Get the model name
    fn model(&self) -> &str;
    
    /// Get the provider name
    fn name(&self) -> &str;
    
    /// Check if provider is available
    async fn is_available(&self) -> bool {
        true
    }
}
```

### Request/Response Types

```rust
pub struct ProviderRequest {
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub model: String,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub stream: bool,
    pub stop: Option<Vec<String>>,
}

pub struct ProviderResponse {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<Usage>,
    pub model: String,
    pub reasoning: Option<String>,
    pub finish_reason: Option<String>,
}
```

## OpenAI Provider

The `OpenAiProvider` implements the OpenAI Chat Completions API format, which is widely adopted by other providers.

### Basic Usage

```rust
use ulnclaw::prelude::*;

let provider = OpenAiProvider::builder()
    .endpoint("https://api.openai.com/v1")
    .api_key("sk-...")
    .model("gpt-4")
    .build()?;

let agent = Agent::new(Arc::new(provider), tools);
```

### Builder Pattern

```rust
let provider = OpenAiProvider::builder()
    .endpoint("https://api.openai.com/v1")  // Required
    .api_key("sk-...")                       // Optional (some providers don't need it)
    .model("gpt-4")                          // Required
    .name("My Provider")                     // Optional (defaults to model name)
    .max_tokens(4096)                        // Optional
    .temperature(0.7)                        // Optional
    .build()?;
```

### API URL Resolution

The provider automatically resolves the API URL:

```rust
// These all resolve to the same endpoint:
"https://api.openai.com/v1"
"https://api.openai.com/v1/"
"https://api.openai.com"
// → "https://api.openai.com/v1/chat/completions"

// Direct endpoint:
"https://api.openai.com/v1/chat/completions"
// → "https://api.openai.com/v1/chat/completions"
```

### Request Flow

```
1. Convert Message/ToolCall to OpenAI API format
   ↓
2. Build HTTP request with headers
   ├─ Content-Type: application/json
   ├─ Authorization: Bearer <api_key>
   └─ Body: JSON payload
   ↓
3. Send POST to /v1/chat/completions
   ↓
4. Parse response
   ├─ Extract content
   ├─ Extract tool_calls
   ├─ Extract usage stats
   └─ Extract reasoning (if present)
   ↓
5. Convert to ProviderResponse
```

## Supported Providers

### OpenAI

```rust
let provider = OpenAiProvider::builder()
    .endpoint("https://api.openai.com/v1")
    .api_key(&std::env::var("OPENAI_API_KEY")?)
    .model("gpt-4")
    .build()?;
```

**Models:** gpt-4, gpt-4-turbo, gpt-3.5-turbo

### Anthropic (Claude)

Native Messages API (`POST /v1/messages`, `x-api-key` auth,
`anthropic-version: 2023-06-01`) — aligned with hermes' `anthropic_messages`
transport:

```rust
let provider = ulnclaw::provider::anthropic::AnthropicProvider::builder()
    .endpoint("https://api.anthropic.com")
    .api_key(&std::env::var("ANTHROPIC_API_KEY")?)
    .model("claude-sonnet-4-5")
    .build()?;
```

The CLI picks this provider automatically when `[model] provider =
"anthropic"` (API key from config, `ULNCLAW_API_KEY`, or
`ANTHROPIC_API_KEY`).  Conversion semantics mirror hermes:

- system messages move into the `system` parameter
- assistant tool calls become `tool_use` blocks; tool results become
  `tool_result` blocks merged into user turns (roles must alternate)
- `max_tokens` is always sent: requested → configured → per-model output
  ceiling table (claude-sonnet-4-5 → 64k, claude-3-5-* → 8k, …) → 128k
  default
- streaming maps Anthropic SSE events (`message_start`,
  `content_block_delta` with `text_delta` / `thinking_delta` /
  `input_json_delta`, `message_delta`, …) onto the shared chunk stream;
  vision uses `image` blocks (base64 data URLs or `url` sources)
- OAuth access tokens (`sk-ant-oat…`) switch to bearer auth
- third-party Anthropic-compatible endpoints work via `endpoint`
  (proxies under an `/anthropic` suffix, MiniMax, DashScope, …)

**Models:** claude-sonnet-4-5, claude-opus-4-x, claude-3-7-sonnet,
claude-3-5-sonnet/haiku, claude-3-opus/sonnet/haiku, …

### DashScope (Alibaba Qwen)

```rust
let provider = OpenAiProvider::builder()
    .endpoint("https://dashscope.aliyuncs.com/compatible-mode")
    .api_key(&std::env::var("DASHSCOPE_API_KEY")?)
    .model("qwen-plus")
    .build()?;
```

**Models:** qwen-turbo, qwen-plus, qwen-max

### OpenRouter

```rust
let provider = OpenAiProvider::builder()
    .endpoint("https://openrouter.ai/api/v1")
    .api_key(&std::env::var("OPENROUTER_API_KEY")?)
    .model("anthropic/claude-3-opus")
    .build()?;
```

**Models:** Any model available on OpenRouter

### Ollama (Local)

```rust
let provider = OpenAiProvider::builder()
    .endpoint("http://localhost:11434/v1")
    .model("llama2")
    .build()?;
```

**Models:** Any model pulled with `ollama pull`

### llama.cpp Server

```rust
let provider = OpenAiProvider::builder()
    .endpoint("http://localhost:8080/v1")
    .model("local-model")
    .build()?;
```

**Note:** Model name is ignored by llama.cpp server

## Implementing Custom Providers

### Step 1: Create Provider Struct

```rust
use ulnclaw::provider::{Provider, ProviderRequest, ProviderResponse};
use ulnclaw::error::Result;
use async_trait::async_trait;

pub struct CustomProvider {
    client: reqwest::Client,
    endpoint: String,
    api_key: Option<String>,
    model: String,
    name: String,
}
```

### Step 2: Implement Builder

```rust
pub struct CustomProviderBuilder {
    endpoint: Option<String>,
    api_key: Option<String>,
    model: Option<String>,
    name: Option<String>,
}

impl CustomProvider {
    pub fn builder() -> CustomProviderBuilder {
        CustomProviderBuilder {
            endpoint: None,
            api_key: None,
            model: None,
            name: None,
        }
    }
}

impl CustomProviderBuilder {
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

    pub fn build(self) -> Result<CustomProvider> {
        let endpoint = self.endpoint
            .ok_or_else(|| ulnclaw::AgentError::config("endpoint is required"))?;
        let model = self.model
            .ok_or_else(|| ulnclaw::AgentError::config("model is required"))?;
        let name = self.name.unwrap_or_else(|| model.clone());

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .map_err(|e| ulnclaw::AgentError::Internal(e.to_string()))?;

        Ok(CustomProvider {
            client,
            endpoint,
            api_key: self.api_key,
            model,
            name,
        })
    }
}
```

### Step 3: Implement Provider Trait

```rust
#[async_trait]
impl Provider for CustomProvider {
    async fn chat_completion(&self, request: ProviderRequest) -> Result<ProviderResponse> {
        // 1. Convert request to API format
        let api_request = self.convert_request(&request);
        
        // 2. Build HTTP request
        let url = format!("{}/chat/completions", self.endpoint);
        let mut req_builder = self.client.post(&url)
            .json(&api_request);
        
        if let Some(ref key) = self.api_key {
            req_builder = req_builder.bearer_auth(key);
        }
        
        // 3. Send request
        let response = req_builder.send().await
            .map_err(|e| ulnclaw::AgentError::Provider(e.to_string()))?;
        
        // 4. Check status
        if !response.status().is_success() {
            let error = response.text().await.unwrap_or_default();
            return Err(ulnclaw::AgentError::Provider(error));
        }
        
        // 5. Parse response
        let api_response: ApiResponse = response.json().await
            .map_err(|e| ulnclaw::AgentError::Provider(e.to_string()))?;
        
        // 6. Convert to ProviderResponse
        self.convert_response(api_response)
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn name(&self) -> &str {
        &self.name
    }
}
```

### Step 4: Add Conversion Methods

```rust
impl CustomProvider {
    fn convert_request(&self, request: &ProviderRequest) -> serde_json::Value {
        // Convert Message and ToolCall to API format
        serde_json::json!({
            "model": request.model,
            "messages": request.messages,
            "tools": request.tools,
            "max_tokens": request.max_tokens,
            "temperature": request.temperature,
        })
    }

    fn convert_response(&self, response: ApiResponse) -> Result<ProviderResponse> {
        // Parse API response into ProviderResponse
        Ok(ProviderResponse {
            content: response.choices[0].message.content.clone(),
            tool_calls: vec![],
            usage: None,
            model: self.model.clone(),
            reasoning: None,
            finish_reason: None,
        })
    }
}
```

### Step 5: Export and Use

```rust
// In your lib.rs or mod.rs
pub mod custom_provider;
pub use custom_provider::CustomProvider;

// Usage
let provider = CustomProvider::builder()
    .endpoint("https://api.custom.com")
    .model("custom-model")
    .build()?;

let agent = Agent::new(Arc::new(provider), tools);
```

## Provider Configuration

### Dynamic Configuration

Use `ProviderConfig` for runtime provider instantiation:

```rust
use ulnclaw::provider::{ProviderConfig, ProviderKind};

let config = ProviderConfig {
    name: "My Provider".to_string(),
    kind: ProviderKind::OpenAiCompatible,
    endpoint: "https://api.openai.com/v1".to_string(),
    api_key: Some("sk-...".to_string()),
    model: "gpt-4".to_string(),
    max_tokens: Some(4096),
    temperature: Some(0.7),
    fallback_providers: vec![],
};

let provider = config.build()?;
```

### Provider Kinds

```rust
pub enum ProviderKind {
    OpenAiCompatible,  // OpenAI, Anthropic, DashScope, etc.
    Ollama,            // Local Ollama server
    LlamaCpp,          // Local llama.cpp server
    Anthropic,         // Native Anthropic API (future)
    Local,             // Embedded model (future)
}
```

### Fallback Providers

Configure fallback providers for automatic failover:

```rust
let config = ProviderConfig {
    name: "Primary".to_string(),
    kind: ProviderKind::OpenAiCompatible,
    endpoint: "https://api.openai.com/v1".to_string(),
    api_key: Some("sk-primary".to_string()),
    model: "gpt-4".to_string(),
    fallback_providers: vec![
        "Fallback 1".to_string(),
        "Fallback 2".to_string(),
    ],
    ..Default::default()
};
```

Implement fallback logic in your application:

```rust
async fn try_providers(configs: Vec<ProviderConfig>, message: &str) -> Result<String> {
    let mut last_error = None;
    
    for config in configs {
        match config.build() {
            Ok(provider) => {
                let agent = Agent::new(Arc::from(provider), tools.clone());
                match agent.chat(message).await {
                    Ok(response) => return Ok(response),
                    Err(e) => last_error = Some(e),
                }
            }
            Err(e) => last_error = Some(e),
        }
    }
    
    Err(last_error.unwrap())
}
```

## Outbound mTLS (Client Certificates)

Deployments that route provider traffic through an mTLS-fronted gateway (a control
plane that authorises, meters and audits every call) must present a client
certificate on egress. ulnclaw resolves that policy from the environment whenever it
builds a provider client:

| Variable | Meaning |
| --- | --- |
| `ULNCLAW_MTLS_CERT` | PEM path of the client certificate chain (leaf first; may embed the key) |
| `ULNCLAW_MTLS_KEY` | PEM path of the private key (PKCS#8 / PKCS#1 / SEC1) when it is not in the cert file |
| `ULNCLAW_MTLS_CA` | Extra trust anchors — the gateway's private CA |
| `ULNCLAW_MTLS_TRUST_BUILTIN` | `1` (default) keeps the public roots; `0` trusts **only** `ULNCLAW_MTLS_CA` |
| `ULNCLAW_MTLS_HOSTS` | **Required when an identity is set**: comma-separated hosts, `*.example.com`, or `*` |
| `ULNCLAW_MTLS_MIN_TLS` | Minimum TLS version: `1.2` (default) or `1.3` |

```bash
# ~/.ulnclaw/.env works too (same variables, read on client build)
export ULNCLAW_MTLS_CERT=~/.ulnclaw/mtls/client.crt
export ULNCLAW_MTLS_KEY=~/.ulnclaw/mtls/client.key
export ULNCLAW_MTLS_CA=~/.ulnclaw/mtls/gateway-ca.crt
export ULNCLAW_MTLS_HOSTS=gw.example.com

ulnclaw mtls status   # resolved policy + leaf SHA-256 fingerprint (never key material)
ulnclaw mtls probe https://gw.example.com/health
```

The scope is enforced per client build: a host outside `ULNCLAW_MTLS_HOSTS` gets a
plain client, so a provider pointed at `api.openai.com` never presents the gateway
identity. Misconfiguration (missing key, unreadable PEM, unresolvable scope) is a
**hard error when the provider is built** — ulnclaw never silently downgrades to
unauthenticated TLS.

Wired paths: `src/provider/openai.rs` (OpenAI-compatible chat — OpenAI, DashScope,
OpenRouter, Ollama, llama.cpp, custom endpoints), `src/provider/anthropic.rs`
(Anthropic Messages API), `src/stt.rs` and `src/tts.rs` (audio in/out). Other egress
surfaces (messaging platforms, webhooks, MCP) can adopt the same one-liner,
`outbound_tls::apply_env(builder, host)?`; run `ulnclaw mtls status` to see the list
this build actually honours. Raw-`rustls` paths use
`OutboundTls::rustls_config()` for the same identity/CA/min-version.

`ulnclaw mtls probe <url>` performs **two** handshakes — with and without the
identity — and reports one of (the anonymous arm also sends a single `GET /`,
because a TLS 1.3 server can complete the handshake and only check the client
certificate afterwards; every verdict carries a `reason` field with the raw
evidence):

- `enforced` — the server refused the anonymous *handshake* **or** the anonymous
  request that follows it, and accepted ours;
- `not-required` — it served a request with no client certificate;
- `rejected` — the handshake failed while presenting our identity (trust, chain,
  expiry), printing the TLS error.

`ulnclaw mtls keygen` (self-signed, for staging) and `ulnclaw mtls csr` (key + CSR
for the gateway's CA to sign) bootstrap the certificate without hand-written openssl
commands. Full design: [Outbound mTLS design note](../design/outbound-mtls.md).

## Error Handling

### Provider Errors

```rust
pub enum AgentError {
    Provider(String),  // API failures, auth errors, rate limits
    Http(reqwest::Error),  // Network errors
    Json(serde_json::Error),  // Serialization errors
    // ...
}
```

### Common Error Scenarios

**Authentication Error:**
```rust
// Error: Provider error: API error (401): Invalid API key
// Solution: Check API key is set correctly
```

**Rate Limit Error:**
```rust
// Error: Provider error: API error (429): Rate limit exceeded
// Solution: Implement retry logic with exponential backoff
```

**Network Error:**
```rust
// Error: HTTP error: Connection timeout
// Solution: Check network connectivity, increase timeout
```

**Model Not Found:**
```rust
// Error: Provider error: API error (404): Model not found
// Solution: Check model name is correct
```

### Error Recovery

```rust
match agent.chat(&message).await {
    Ok(response) => println!("{}", response),
    Err(ulnclaw::AgentError::Provider(msg)) => {
        if msg.contains("401") {
            eprintln!("Authentication failed. Check API key.");
        } else if msg.contains("429") {
            eprintln!("Rate limited. Retrying...");
            tokio::time::sleep(Duration::from_secs(5)).await;
            // Retry
        } else {
            eprintln!("Provider error: {}", msg);
        }
    }
    Err(e) => eprintln!("Error: {}", e),
}
```

## Best Practices

### 1. Use Environment Variables for API Keys

```rust
// ❌ Don't hardcode
let provider = OpenAiProvider::builder()
    .api_key("sk-...")
    .build()?;

// ✅ Use environment variables
let provider = OpenAiProvider::builder()
    .api_key(&std::env::var("OPENAI_API_KEY")?)
    .build()?;
```

### 2. Set Reasonable Timeouts

```rust
let provider = OpenAiProvider::builder()
    .endpoint("https://api.openai.com/v1")
    .api_key(&api_key)
    .model("gpt-4")
    // Timeout is set in builder (default: 120s)
    .build()?;
```

### 3. Implement Retry Logic

```rust
async fn chat_with_retry(agent: &Agent, message: &str, max_retries: u32) -> Result<String> {
    let mut attempt = 0;
    
    loop {
        match agent.chat(message).await {
            Ok(response) => return Ok(response),
            Err(ulnclaw::AgentError::Provider(msg)) if msg.contains("429") => {
                attempt += 1;
                if attempt > max_retries {
                    return Err(ulnclaw::AgentError::Provider("Max retries exceeded".into()));
                }
                
                let delay = Duration::from_secs(2u64.pow(attempt));
                eprintln!("Rate limited. Retrying in {:?}...", delay);
                tokio::time::sleep(delay).await;
            }
            Err(e) => return Err(e),
        }
    }
}
```

### 4. Monitor Usage

```rust
let result = agent.run(&message, None).await?;

if let Some(usage) = &result.usage {
    println!("Tokens used: {}", usage.total_tokens);
    println!("Prompt tokens: {}", usage.prompt_tokens);
    println!("Completion tokens: {}", usage.completion_tokens);
    
    // Log to monitoring system
    log_usage(usage.total_tokens, &result.model);
}
```

### 5. Test with Mock Providers

```rust
#[cfg(test)]
mod tests {
    use super::*;

    struct MockProvider;

    #[async_trait]
    impl Provider for MockProvider {
        async fn chat_completion(&self, _request: ProviderRequest) -> Result<ProviderResponse> {
            Ok(ProviderResponse {
                content: Some("Mock response".to_string()),
                tool_calls: vec![],
                usage: Some(Usage {
                    prompt_tokens: 10,
                    completion_tokens: 5,
                    total_tokens: 15,
                }),
                model: "mock".to_string(),
                reasoning: None,
                finish_reason: Some("stop".to_string()),
            })
        }

        fn model(&self) -> &str {
            "mock"
        }

        fn name(&self) -> &str {
            "MockProvider"
        }
    }

    #[tokio::test]
    async fn test_agent_with_mock() {
        let provider = MockProvider;
        let tools = ToolRegistry::new();
        let agent = Agent::new(Arc::new(provider), tools);
        
        let response = agent.chat("Hello").await.unwrap();
        assert_eq!(response, "Mock response");
    }
}
```

### 6. Handle Streaming (Future)

```rust
// Streaming support is planned for future versions
// For now, use non-streaming mode

let provider = OpenAiProvider::builder()
    .endpoint("https://api.openai.com/v1")
    .api_key(&api_key)
    .model("gpt-4")
    .build()?;

// stream: false (default)
let request = ProviderRequest {
    messages: vec![],
    tools: vec![],
    model: "gpt-4".to_string(),
    stream: false,  // Non-streaming
    // ...
};
```

## Auxiliary Model Routing

Secondary LLM calls — context-compression summaries and vision analysis —
can run on a different provider/model than the main conversation. This is a
aligned with hermes' `agent/auxiliary_client.py` task layer: each task checks
`[auxiliary.<task>]` config first and inherits the main runtime when the
entry is blank or `"auto"`.

```toml
# Summarize context with a cheap model while chatting on a big one
[auxiliary.compression]
provider = "openai"
model = "gpt-5.2-mini"

# Vision on a dedicated endpoint (base_url/api_key/key_env optional;
# unset values inherit the main provider's endpoint and key)
[auxiliary.vision]
model = "gpt-5.2"

# Session titles after the first exchange (fire-and-forget background
# task). Extra knobs: `enabled` kill switch (bool or "on"/"off"/...
# spellings, default true) and `language` pin (blank = match the user).
[auxiliary.title_generation]
model = "gpt-5.2-mini"
```

Fields per task: `provider`, `model`, `base_url`, `api_key`, `key_env`.
Resolution rules (`provider::auxiliary::resolve_aux_task`):

- No entry (or everything blank/`"auto"`) → the main provider instance is
  reused with the main model.
- Model-only override → a fresh instance of the main provider is built with
  the overridden model (same endpoint/key).
- Provider/endpoint/key override → a dedicated client is built
  (`anthropic` uses the Messages API transport, everything else
  OpenAI-compatible). The key falls back to `key_env`, then the main
  runtime key; keyless providers (`ollama`, `llamacpp`, `local`) need none.

Wired tasks: `compression` (context compressor summary call) and `vision`
(`vision_analyze`, `browser_vision`).

## Mixture of Agents (MoA)

MoA fans a prompt out to several *reference* models in parallel, then asks an
*aggregator* model to synthesize their answers into concise guidance — a implementation
of hermes' `moa_loop.py` synthesis path (`aggregate_moa_context`).

Configure presets in `config.toml`:

```toml
[moa]
default_preset = "default"

[[moa.presets.default.reference_models]]
provider = "ollama"
model = "qwen3:32b"

[[moa.presets.default.reference_models]]
provider = "openai"
model = "gpt-5.2"
# optional per-slot: base_url, api_key, key_env, enabled = false

[moa.presets.default.aggregator]
provider = "openai"
model = "gpt-5.2"
# optional preset keys: reference_temperature, reference_max_tokens,
# aggregator_temperature, degraded_reference_policy = "loud" | "silent"
```

Usage:

```bash
ulnclaw moa list                      # show presets
ulnclaw moa run "your question"       # fan-out + synthesis (stdout)
ulnclaw moa run "..." --preset other  # explicit preset
ulnclaw moa delete other              # remove a preset (rewrites config)
# inside the REPL:
/moa your question
```

Behavior (mirrors hermes):

- References run in parallel; each slot builds its own client (Anthropic
  slots use the Messages API transport). Credentials fall back
  slot `api_key` → `key_env` → main runtime key; keyless locals need none.
- Failed references are excluded from synthesis; with the default `loud`
  policy the aggregator prompt carries
  `[Reference models unavailable: <labels>]`.
- If every reference fails, the aggregator call is skipped and a notice is
  returned instead.
- If the aggregator itself fails, the joined reference outputs are returned
  as the synthesis.

Not implemented: the persistent `provider: moa` client facade (MoA as the acting
model for whole sessions), MoA traces, and the privacy filter.

## Provider Fallback Chain

`[model] fallbacks` lists backup `"provider:model"` entries tried in order
when a model call fails — an implementation matching hermes' `fallback_providers` chain
(`try_activate_fallback` + `restore_primary_runtime`):

```toml
[model]
provider = "openai"
model = "gpt-5.2"
max_retries = 1                 # retries happen first, per provider
fallbacks = [
  "openai:gpt-5.2-mini",        # same provider, cheaper model
  "ollama:qwen3:32b",           # local escape hatch (no key needed)
]
```

Semantics:

- The primary provider's own retries (`max_retries`, exponential backoff)
  run first; the chain advances only after the call truly fails.
- Each entry builds lazily on first use: OpenAI-compatible or Anthropic
  transport per provider name, endpoint = the provider default (or
  `OPENAI_BASE_URL`), key = main runtime key (keyless locals exempt).
- The first fallback that answers stays active for the rest of the turn;
  the next turn restores the primary (per-turn restore, like hermes).
- Malformed specs (`"provider"` without a model) are skipped with a
  warning; models may contain `:` (`ollama:qwen3:1.7b`).
- Delegated sub-agents and cron runs inherit the configured specs.

## Troubleshooting

### Common Issues

**Issue**: "API error (401): Invalid API key"
- **Solution**: Verify API key is set correctly, check environment variable

**Issue**: "API error (404): Model not found"
- **Solution**: Check model name spelling, verify model is available

**Issue**: "Connection timeout"
- **Solution**: Check network connectivity, verify endpoint URL

**Issue**: "API error (429): Rate limit exceeded"
- **Solution**: Implement retry logic, reduce request frequency

**Issue**: "Failed to parse response"
- **Solution**: Check API compatibility, verify endpoint returns OpenAI format

### Debug Mode

Enable detailed logging:

```rust
use tracing_subscriber;

tracing_subscriber::fmt()
    .with_max_level(tracing::Level::DEBUG)
    .init();

// Now you'll see detailed HTTP request/response logs
```

## Next Steps

- Read [Tool System Guide](tools.md) for implementing tools
- Check [API Reference](api-reference.md) for complete type documentation
- See [Integration Guide](integration.md) for using providers in applications
- Review [Development Guide](development.md) for contributing new providers

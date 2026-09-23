# API Reference

Complete API documentation for ulnclaw.

## Core Types

### Agent

Main orchestrator for AI conversations.

```rust
pub struct Agent {
    provider: Arc<dyn Provider>,
    tools: Arc<Mutex<ToolRegistry>>,
    config: AgentConfig,
    callbacks: Arc<Mutex<AgentCallbacks>>,
}
```

**Methods:**

#### `new(provider: Arc<dyn Provider>, tools: ToolRegistry) -> Self`

Create a new agent with a provider and tool registry.

**Parameters:**
- `provider` - AI model provider (wrapped in Arc for shared ownership)
- `tools` - Tool registry containing available tools

**Example:**
```rust
let provider = OpenAiProvider::builder()
    .endpoint("https://api.openai.com/v1")
    .api_key("sk-...")
    .model("gpt-4")
    .build()?;

let tools = ToolRegistry::new();
let agent = Agent::new(Arc::new(provider), tools);
```

#### `with_config(self, config: AgentConfig) -> Self`

Set agent configuration.

**Parameters:**
- `config` - Agent configuration struct

**Example:**
```rust
let agent = agent.with_config(AgentConfig {
    max_iterations: 100,
    max_tokens: Some(4096),
    temperature: Some(0.7),
    system_prompt: Some("You are helpful.".into()),
    ..Default::default()
});
```

#### `with_callbacks(self, callbacks: AgentCallbacks) -> Self`

Set event callbacks for UI integration.

**Parameters:**
- `callbacks` - Callback functions for various events

#### `async run(user_message: &str, conversation_history: Option<Vec<Message>>) -> Result<RunResult>`

Run the agent with a user message and optional conversation history.

**Parameters:**
- `user_message` - User's input message
- `conversation_history` - Optional previous messages

**Returns:**
- `RunResult` containing final response, history, usage stats

**Example:**
```rust
let result = agent.run("What's the weather?", None).await?;
println!("{}", result.content);
println!("Tokens used: {}", result.usage.total_tokens);
```

#### `async run_with_session(user_message: &str, conversation_history: Option<Vec<Message>>, resume_session_id: Option<&str>) -> Result<RunResult>`

Like `run`, but resumes an existing session id instead of creating a new
one. When `resume_session_id` is `Some`, the store row is ensured and only
the new user message is appended (history rows are assumed to exist).
Used by the HTTP gateway for session continuity.

**Parameters:**
- `user_message` - User's input message
- `conversation_history` - Optional previous messages (typically loaded
  from the store for the resumed session)
- `resume_session_id` - Session id to continue, or `None` for a fresh one

**Returns:**
- `RunResult` whose `session_id` equals the resumed id

#### `async chat(user_message: &str) -> Result<String>`

Simple interface that returns only the text response.

**Parameters:**
- `user_message` - User's input message

**Returns:**
- Final text response as String

**Example:**
```rust
let response = agent.chat("Hello!").await?;
println!("{}", response);
```

### AgentConfig

Configuration for the agent.

```rust
pub struct AgentConfig {
    pub max_iterations: usize,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub system_prompt: Option<String>,
    pub strip_thinking_blocks: bool,
    pub model: Option<String>,
    pub concurrent_tool_execution: bool,
    pub max_concurrent_tools: usize,
}
```

**Fields:**
- `max_iterations` - Maximum tool calling iterations (default: 50)
- `max_tokens` - Maximum tokens in response
- `temperature` - Sampling temperature (0.0-2.0)
- `system_prompt` - System message for the agent
- `strip_thinking_blocks` - Remove `<think>` tags (default: true)
- `model` - Model name override
- `concurrent_tool_execution` - Execute tools in parallel (default: false)
- `max_concurrent_tools` - Max parallel tools (default: 5)

### RunResult

Result from running the agent.

```rust
pub struct RunResult {
    pub content: String,
    pub conversation: Vec<Message>,
    pub usage: Usage,
    pub iterations: usize,
    pub tool_calls: Vec<ToolCallRecord>,
}
```

**Fields:**
- `content` - Final text response
- `conversation` - Complete message history
- `usage` - Token usage statistics
- `iterations` - Number of iterations executed
- `tool_calls` - Record of all tool calls made

### ToolCallRecord

Record of a single tool call.

```rust
pub struct ToolCallRecord {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
    pub result: serde_json::Value,
}
```

## Provider Types

### Provider Trait

```rust
#[async_trait]
pub trait Provider: Send + Sync {
    async fn chat_completion(&self, request: ProviderRequest) -> Result<ProviderResponse>;
    async fn chat_completion_stream(&self, request: ProviderRequest) -> Result<ProviderStream>;
    fn supports_streaming(&self) -> bool;
    fn model(&self) -> &str;
    fn name(&self) -> &str;
    async fn is_available(&self) -> bool;
}
```

`chat_completion_stream` defaults to an "unsupported" error; providers opt in
by implementing it and returning `true` from `supports_streaming()`.

### Streaming Types

```rust
/// One SSE chunk of a streaming chat completion.
pub struct StreamChunk {
    pub delta_content: Option<String>,      // text delta
    pub delta_reasoning: Option<String>,    // thinking delta (where supported)
    pub tool_call_deltas: Vec<ToolCallDelta>,
    pub finish_reason: Option<String>,      // final chunk: stop/tool_calls/length
    pub usage: Option<Usage>,               // final chunk (include_usage)
    pub model: Option<String>,
}

/// Incremental tool-call fragment (OpenAI delta.tool_calls[i]).
pub struct ToolCallDelta {
    pub index: usize,
    pub id: Option<String>,
    pub name_delta: Option<String>,
    pub arguments_delta: Option<String>,
}

pub type ProviderStream = Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>;

/// Reassemble streamed tool-call deltas into complete tool calls.
pub fn assemble_tool_calls(deltas: &[ToolCallDelta]) -> Vec<ToolCall>;
```

`openai::parse_stream_chunk(data)` parses a single SSE `data:` payload.

### OpenAiProvider

OpenAI-compatible provider implementation.

```rust
pub struct OpenAiProvider { /* fields hidden */ }
```

**Builder Methods:**

#### `builder() -> OpenAiProviderBuilder`

Create a new builder.

#### `OpenAiProviderBuilder::endpoint(self, endpoint: &str) -> Self`

Set API endpoint URL.

#### `OpenAiProviderBuilder::api_key(self, key: &str) -> Self`

Set API key.

#### `OpenAiProviderBuilder::model(self, model: &str) -> Self`

Set model name.

#### `OpenAiProviderBuilder::name(self, name: &str) -> Self`

Set provider name (for display).

#### `OpenAiProviderBuilder::max_tokens(self, max_tokens: u32) -> Self`

Set maximum tokens.

#### `OpenAiProviderBuilder::temperature(self, temp: f32) -> Self`

Set sampling temperature.

#### `OpenAiProviderBuilder::build(self) -> Result<OpenAiProvider>`

Build the provider.

**Example:**
```rust
let provider = OpenAiProvider::builder()
    .endpoint("https://api.openai.com/v1")
    .api_key("sk-...")
    .model("gpt-4")
    .max_tokens(4096)
    .temperature(0.7)
    .build()?;
```

### Message

Conversation message.

```rust
pub struct Message {
    pub role: Role,
    pub content: Option<String>,
    pub tool_calls: Option<Vec<ToolCall>>,
    pub tool_call_id: Option<String>,
    pub name: Option<String>,
}
```

### Role

Message role.

```rust
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}
```

### ToolCall

Tool call requested by the model.

```rust
pub struct ToolCall {
    pub id: String,
    pub call_type: String,
    pub function: FunctionCall,
}
```

### FunctionCall

Function call details.

```rust
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}
```

### Usage

Token usage statistics.

```rust
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}
```

**Methods:**

#### `merge(&mut self, other: &Usage)`

Merge usage from another request.

### ProviderConfig

Dynamic provider configuration.

```rust
pub struct ProviderConfig {
    pub name: String,
    pub kind: ProviderKind,
    pub endpoint: String,
    pub api_key: Option<String>,
    pub model: String,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub fallback_providers: Vec<String>,
}
```

**Methods:**

#### `build(&self) -> Result<Box<dyn Provider>>`

Build a provider from this configuration.

### ProviderKind

Supported provider types.

```rust
pub enum ProviderKind {
    OpenAiCompatible,
    Ollama,
    LlamaCpp,
    Anthropic,
    Local,
}
```

## Tool Types

### ToolRegistry

Central registry for all tools.

```rust
pub struct ToolRegistry { /* fields hidden */ }
```

**Methods:**

#### `new() -> Self`

Create a new empty registry.

#### `register(&mut self, tool: Tool)`

Register a tool.

#### `unregister(&mut self, name: &str) -> Option<Tool>`

Unregister a tool by name.

#### `get(&self, name: &str) -> Option<&Tool>`

Get a tool by name.

#### `async dispatch(&self, name: &str, arguments: Value) -> Result<Value>`

Execute a tool with given arguments.

#### `definitions(&self) -> Vec<ToolDefinition>`

Get all enabled tool definitions.

#### `names(&self) -> Vec<String>`

Get all tool names.

#### `has(&self, name: &str) -> bool`

Check if a tool exists.

#### `len(&self) -> usize`

Get number of registered tools.

#### `is_empty(&self) -> bool`

Check if registry is empty.

#### `enable_toolset(&mut self, name: &str)`

Enable a toolset.

#### `disable_toolset(&mut self, name: &str)`

Disable a toolset.

#### `toolset_names(&self) -> Vec<String>`

Get all toolset names.

#### `toolset_tools(&self, toolset: &str) -> Vec<&Tool>`

Get tools in a specific toolset.

### Tool

A registered tool with definition and handler.

```rust
pub struct Tool {
    pub definition: ToolDefinition,
    pub handler: ToolHandler,
    pub toolset: String,
    pub dangerous: bool,
}
```

### ToolDefinition

Tool schema exposed to the model.

```rust
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}
```

### ToolBuilder

Fluent API for creating tools.

#### `tool(name: impl Into<String>) -> ToolBuilder`

Create a new tool builder.

**Builder Methods:**

- `description(self, desc: impl Into<String>) -> Self` - Set description
- `parameters(self, params: Value) -> Self` - Set JSON schema
- `handler<F, Fut>(self, handler: F) -> Self` - Set async handler
- `toolset(self, toolset: impl Into<String>) -> Self` - Set toolset
- `dangerous(self, dangerous: bool) -> Self` - Mark as dangerous
- `build(self) -> Result<Tool>` - Build the tool

**Example:**
```rust
let tool = tool("calculate")
    .description("Perform arithmetic operations")
    .parameters(json!({
        "type": "object",
        "properties": {
            "expression": {"type": "string"}
        },
        "required": ["expression"]
    }))
    .handler(|args| async move {
        let expr = args["expression"].as_str().unwrap();
        // Evaluate expression
        Ok(json!({"result": 42}))
    })
    .toolset("math")
    .build()?;
```

### ToolResult

Result of tool execution.

```rust
pub struct ToolResult {
    pub success: bool,
    pub data: Option<Value>,
    pub error: Option<String>,
}
```

**Methods:**

#### `ok(data: Value) -> Self`

Create a successful result.

#### `err(msg: impl Into<String>) -> Self`

Create an error result.

#### `to_value(&self) -> Value`

Convert to JSON value.

## Session Types

### SessionStore Trait

```rust
pub trait SessionStore: Send + Sync {
    fn save_session(&self, session: &Session) -> Result<()>;
    fn load_session(&self, session_id: &str) -> Result<Option<Session>>;
    fn list_sessions(&self, limit: usize) -> Result<Vec<Session>>;
    fn delete_session(&self, session_id: &str) -> Result<()>;
    fn search_sessions(&self, query: &str, limit: usize) -> Result<Vec<Session>>;
}
```

### MemorySessionStore

In-memory session storage.

```rust
pub struct MemorySessionStore { /* fields hidden */ }
```

**Methods:**

#### `new() -> Self`

Create a new in-memory store.

Implements `SessionStore` trait.

### Session

Conversation session.

```rust
pub struct Session {
    pub id: String,
    pub conversation_id: String,
    pub messages: Vec<Message>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub parent_id: Option<String>,
    pub metadata: SessionMetadata,
}
```

### SessionMetadata

Session metadata.

```rust
pub struct SessionMetadata {
    pub user_id: Option<String>,
    pub platform: Option<String>,
    pub model: Option<String>,
    pub total_tokens: Option<u32>,
    pub iteration_count: Option<u32>,
}
```

#### `new_session(conversation_id: &str) -> Session`

Create a new session with unique ID.

## Context Types

### PromptBuilder

Build system prompts with tiered assembly.

```rust
pub struct PromptBuilder { /* fields hidden */ }
```

**Methods:**

#### `new() -> Self`

Create a new builder.

#### `identity(self, identity: impl Into<String>) -> Self`

Set agent identity.

#### `tool_guidance(self, guidance: impl Into<String>) -> Self`

Set tool usage guidance.

#### `add_skill(self, skill: impl Into<String>) -> Self`

Add a skill/instruction.

#### `add_context_file(self, content: impl Into<String>) -> Self`

Add context file content.

#### `add_env_hint(self, key: impl Into<String>, value: impl Into<String>) -> Self`

Add environment hint.

#### `memory(self, memory: impl Into<String>) -> Self`

Set persistent memory.

#### `suffix(self, suffix: impl Into<String>) -> Self`

Set custom suffix.

#### `build(&self) -> String`

Build the complete system prompt.

**Example:**
```rust
let prompt = PromptBuilder::new()
    .identity("You are a helpful assistant.")
    .tool_guidance("Use tools when needed.")
    .add_skill("Always be polite.")
    .add_env_hint("OS", "Linux")
    .memory("User prefers dark mode.")
    .build();
```

### ContextCompressor

Context window optimization.

```rust
pub struct ContextCompressor {
    pub max_context_tokens: usize,
    pub target_ratio: f32,
}
```

**Methods:**

#### `estimate_tokens(messages: &[Message]) -> usize`

Estimate token count (rough: 4 chars per token).

#### `needs_compression(&self, messages: &[Message]) -> bool`

Check if compression is needed.

#### `compress(&self, messages: Vec<Message>) -> Vec<Message>`

Compress messages (placeholder implementation).

## Error Types

### AgentError

```rust
pub enum AgentError {
    Provider(String),
    Tool(String),
    ToolNotFound(String),
    Session(String),
    Context(String),
    Config(String),
    IterationLimit(usize),
    Http(reqwest::Error),
    Json(serde_json::Error),
    Internal(String),
}
```

**Helper Methods:**

- `provider(msg: impl Into<String>) -> Self`
- `tool(msg: impl Into<String>) -> Self`
- `session(msg: impl Into<String>) -> Self`
- `config(msg: impl Into<String>) -> Self`

### Result Type

```rust
pub type Result<T> = std::result::Result<T, AgentError>;
```

## Utility Functions

### strip_thinking_blocks(text: &str) -> String

Remove `<think>...</think>` and `<thinking>...</thinking>` blocks from text.

**Example:**
```rust
let cleaned = strip_thinking_blocks("Hello <think>thinking</think> world");
assert_eq!(cleaned, "Hello  world");
```

## Prelude Module

Convenient imports for common use cases.

```rust
pub mod prelude {
    pub use crate::agent::{Agent, AgentConfig, RunResult};
    pub use crate::error::{AgentError, Result};
    pub use crate::provider::openai::OpenAiProvider;
    pub use crate::provider::{Message, Provider, Role};
    pub use crate::tools::{tool, ToolRegistry};
    pub use serde_json::json;
    pub use std::sync::Arc;
}
```

**Usage:**
```rust
use ulnclaw::prelude::*;
```

## Constants

```rust
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const NAME: &str = env!("CARGO_PKG_NAME");
```

## Functions

### version() -> &'static str

Get version string.

## Complete Example

```rust
use ulnclaw::prelude::*;

#[tokio::main]
async fn main() -> Result<()> {
    // Create provider
    let provider = OpenAiProvider::builder()
        .endpoint("https://api.openai.com/v1")
        .api_key("sk-...")
        .model("gpt-4")
        .build()?;

    // Create tools
    let mut tools = ToolRegistry::new();
    
    tools.register(tool("get_time")
        .description("Get current time")
        .handler(|_| async {
            Ok(json!({"time": "2026-08-04 12:00:00"}))
        })
        .build()?);

    // Create agent
    let agent = Agent::new(Arc::new(provider), tools)
        .with_config(AgentConfig {
            system_prompt: Some("You are helpful.".into()),
            max_iterations: 50,
            ..Default::default()
        });

    // Run
    let result = agent.run("What time is it?", None).await?;
    println!("{}", result.content);
    
    Ok(())
}
```

## Browser Module (`browser/`)

CDP client backing the `browser_*` tools. Requires `ULNCLAW_BROWSER_CDP`
(`ws://...` endpoint or `http://host:port` discovery base).

### Endpoint Resolution

#### `resolve_endpoint(raw: &str) -> Result<BrowserEndpoint>`

Parses the configured endpoint into `browser_ws` (direct WebSocket) or
`http_base` (HTTP discovery) form.

#### `async discover_browser_ws(http_base: &str) -> Result<String>`

Fetches `/json/version` and returns `webSocketDebuggerUrl`.

#### `async list_page_targets(http_base: &str) -> Result<Vec<Value>>` / `create_page_target(http_base, url)`

Target discovery via `/json` and `PUT /json/new?<url>`.

### CdpClient

#### `async connect(ws_url: &str) -> Result<Arc<CdpClient>>`

Opens the WebSocket and starts reader/writer loops.

#### `async call(method: &str, params: Value) -> Result<Value>`

Sends a CDP command and awaits its result (30s timeout).

#### `fn notify(method: &str, params: Value) -> Result<()>`

Sends a CDP event/notification (no response expected).

#### `async subscribe(prefix: &str) -> broadcast::Receiver<Value>`

Subscribes to events whose method starts with `prefix`.

### BrowserSession

#### `async open(endpoint: &BrowserEndpoint) -> Result<Arc<BrowserSession>>`

Finds (or creates) a page target, attaches, and enables
Page/Runtime/DOM/Accessibility domains.

| Method | Description |
|--------|-------------|
| `navigate(url)` | Navigate and wait for the load event (bounded) |
| `go_back()` | `window.history.back()` |
| `evaluate(expression, timeout_ms)` | `Runtime.evaluate` with `returnByValue` |
| `snapshot()` | Accessibility listing + numbered `ElementRef`s |
| `click(element)` | Click by element ref (`"3"`) or CSS selector |
| `type_text(element, text)` | Focus + set value + input/change events |
| `scroll(direction, pixels)` | `window.scrollBy` |
| `press(key)` | `Input.dispatchKeyEvent` (Enter/Tab/Escape/arrows/chars) |
| `get_images()` | First 100 `document.images` with sizes |
| `screenshot()` | PNG screenshot as base64 |
| `page_info()` | `{title, url}` |
| `handle_dialog(accept, prompt_text)` | Resolve the most recent JS dialog |

### Browser Supervisor

#### `find_browser_binary() -> Option<PathBuf>`

Locates Chrome/Chromium (`ULNCLAW_BROWSER_PATH` override, then PATH and
well-known install dirs).

#### `async launch_managed_browser() -> Result<String>`

Launches (or reuses) a managed headless browser on a free port and returns
its HTTP discovery base. Used automatically when `ULNCLAW_BROWSER_CDP` is
unset or `auto`.

#### `async stop_managed_browser()`

Terminates the managed browser and drops the shared session.

### Session Manager

#### `async with_session<F, Fut, T>(func: F) -> Result<T>`

Gets (or opens) the process-wide shared session and runs `func` against it.
This is the entry point used by every `browser_*` tool handler.

## Environments (`environments`)

Terminal backends for the `terminal` tool (hermes `tools/environments/`).

#### `enum TerminalBackend { Local, Docker { container, image }, Ssh { host, user, port, identity } }`

#### `resolve(config: &TerminalConfig) -> TerminalBackend`

Reads `[terminal] backend` (`"local"` default, `"docker"`, `"ssh"`).

#### `async ensure_docker_container(container: &str, image: &str) -> Result<()>`

`docker inspect` first; when missing, `docker run -d --name <container> <image> sleep infinity`.

#### `wrap_command(backend: &TerminalBackend, command: &str) -> Vec<String>`

Local → plain shell; Docker → `docker exec --workdir <cwd> <container> bash -lc <quoted>`;
Ssh → `ssh -o BatchMode=yes [-i identity] [-p port] [user@]host cd <cwd> && <command>`.

Config (`config.toml`):

```toml
[terminal]
backend = "docker"          # "local" (default) | "docker" | "ssh"
container = "ulnclaw-dev"   # docker container name
image = "ubuntu:24.04"      # image used to create the container if missing
ssh_host = "build-box"
ssh_user = "dev"
ssh_port = 22
ssh_identity = "~/.ssh/id_ed25519"
```

## Checkpoints (`checkpoint`)

Transparent filesystem snapshots (hermes `checkpoint_manager.py`).  A single
shared bare git store under `<home>/checkpoints/store` keeps per-project
snapshot chains in `refs/hermes/<hash16>` with per-project indexes — the LLM
never sees this subsystem.

#### `CheckpointManager::new(base: PathBuf, config: &CheckpointsConfig) -> Self`

#### `new_turn(&self)`

Reset per-turn dedup (called by the agent at the start of every iteration).

#### `async ensure_checkpoint(&self, working_dir: &str, reason: &str) -> bool`

Snapshot once per directory per turn; skips `/`, `$HOME`, >50k-file dirs,
files larger than `max_file_size_mb`.  The agent calls this before
`write_file`/`patch` dispatch.

#### `async list_checkpoints(&self, working_dir: &str) -> Vec<CheckpointEntry>`

#### `async restore(&self, working_dir: &str, commit_hash: &str, file_path: Option<&str>) -> Result<Value, String>`

Takes a pre-rollback snapshot first (undo the undo), then `git checkout <hash> -- <path|.`.

#### `async diff(&self, ...) / session_diff(&self, ...)`

Diff the working tree against a checkpoint / against the earliest retained checkpoint.

#### `async status(&self) -> StoreStatus` / `async prune(&self, retention_days: u64, delete_orphans: bool) -> PruneStats` / `async maybe_auto_prune(&self)`

CLI: `ulnclaw checkpoints list [dir] | status | restore <hash> [file] [--dir D] | diff <hash> [--dir D] | prune [--days N]`.
REPL: `/rollback` (list), `/rollback <N|hash> [file]` (restore),
`/rollback diff <N|hash>` (preview), `/diff [session|N|hash]`.

Config (`config.toml`):

```toml
[checkpoints]
enabled = false          # master switch (opt-in)
max_snapshots = 20       # keep at most N checkpoints per project
max_total_size_mb = 500  # store size cap (oldest dropped across projects)
max_file_size_mb = 10    # skip files larger than this
retention_days = 7       # auto-prune stale projects
auto_prune_hours = 24    # auto-prune cadence
```

## HTTP Gateway (`gateway/`)

OpenAI-compatible API server (`ulnclaw gateway`).

### Library API

#### `GatewayState::new(agent: Arc<Agent>, model_name: String, provider_name: String, key: Option<String>, router: Arc<ApprovalRouter>) -> Result<Arc<GatewayState>>`

Builds gateway state from a wired agent (SQLite store must be attached).
The `ApprovalRouter` connects the agent's approve callback to run-scoped
HTTP approval resolution (see below).

#### `ApprovalRouter::new() -> Arc<ApprovalRouter>` / `with_options(timeout, persist_path)` / `current_run_id() -> Option<String>`

Run-approval plumbing.  The router maps `run_id → (session_id, channel)`;
the agent's approve callback (installed by the CLI when starting the
gateway) reads the task-local run id and awaits `router.request(run_id,
reason, command)`.  `once` grants a single use, `session` remembers the
command for the run's session, `always` remembers it for the gateway's
lifetime **and persists it to `approvals.json`**, `deny` rejects.
Requests without a run context (e.g. `/v1/chat/completions`) auto-deny
confirm-tier commands.

`request_outcome` distinguishes an explicit `Denied` from `TimedOut`.
When no decision arrives within `[approvals] timeout` (default 300s,
hermes parity) the approval fails closed, the run's pending approval is
cleaned up, and the command is blocked — the run never parks forever.

```toml
[approvals]
timeout = 300     # seconds before fail-closed auto-deny (0 = wait forever)
```

`gateway_approve_fn(router, state)` builds the approve callback with the
late-bound gateway state used for timeout cleanup.

#### `router(state: Arc<GatewayState>) -> Router`

The axum route table (used by `serve` and by tests).

#### `async serve(state: Arc<GatewayState>, host: &str, port: u16) -> Result<()>`

Binds and serves until interrupted.

### HTTP Endpoints

| Method | Path | Auth | Description |
|--------|------|------|-------------|
| GET | `/health`, `/v1/health` | no | Liveness probe |
| GET | `/health/detailed` | no | Rich status (model, provider, auth, runs) |
| GET | `/v1/models` | yes | Advertised model list |
| GET | `/api/model/options` | yes | Provider/model inventory for pickers (configured provider row) |
| GET | `/v1/capabilities` | yes | Machine-readable endpoint catalog |
| GET | `/metrics` | yes | Prometheus text-format counters and gauges (ulnclaw ops extension) |
| GET | `/api/usage?limit=N` | yes | Token accounting: process counters since gateway start + all-time store totals + recent per-session rows (ulnclaw ops extension) |
| POST | `/v1/chat/completions` | yes | OpenAI Chat Completions; session continuity via `X-Ulnclaw-Session-Id` (also accepts `X-Hermes-Session-Id`); id echoed back in the response header; `stream: true` → SSE `chat.completion.chunk` |
| POST | `/v1/responses` | yes | OpenAI Responses format; `input` string or message array; chain turns with `previous_response_id`; `stream: true` → Responses-API SSE events |
| GET | `/v1/responses/:id` | yes | Retrieve a stored response |
| DELETE | `/v1/responses/:id` | yes | Delete a stored response |
| GET | `/api/sessions?limit=N` | yes | Recent sessions (newest first) |
| POST | `/api/sessions` | yes | Create an empty session |
| GET | `/api/sessions/:id` | yes | Session row |
| PATCH | `/api/sessions/:id` | yes | Update `title` and/or `end_reason` (unknown fields → 400) |
| POST | `/api/sessions/:id/model` | yes | Lock the session to a model (`{"model": "...", "provider": "..."}`) |
| GET | `/api/sessions/:id/recap` | yes | Instant local activity recap (no LLM call) |
| DELETE | `/api/sessions/:id` | yes | Hard delete (messages + FTS entries) |
| POST | `/api/sessions/:id/fork` | yes | Branch a session → `201` child session (transcript copied, source marked `branched`) |
| GET | `/api/sessions/:id/messages` | yes | Message history |
| POST | `/api/sessions/:id/chat` | yes | Run one turn inside the session |
| POST | `/api/sessions/:id/chat/stream` | yes | Same, streamed as SSE chunks |
| GET | `/api/jobs?include_disabled=true` | yes | Cron jobs (disabled hidden unless asked) |
| POST | `/api/jobs` | yes | Create cron job (`name`, `schedule`, `prompt`, optional `skills`, `repeat`, `deliver`) |
| GET | `/api/jobs/delivery-targets` | yes | Delivery targets for the dropdown: implicit `local` + connected platforms with `home_target_set` |
| POST | `/api/jobs/fire` | **no**¹ | Chronos NAS fire webhook: NAS-minted JWT (`[cron.chronos]`) → 401/400/200-gone/202 + background run |
| GET | `/api/jobs/:id` | yes | Single job |
| PATCH | `/api/jobs/:id` | yes | Update whitelisted fields (`name`, `schedule`, `prompt`, `skills`, `repeat`, `enabled`, `deliver`) |
| DELETE | `/api/jobs/:id` | yes | Delete job |
| POST | `/api/jobs/:id/pause` | yes | Disable job (clears `next_run`) |
| POST | `/api/jobs/:id/resume` | yes | Re-enable job (recomputes `next_run`) |
| POST | `/api/jobs/:id/run` | yes | Trigger one immediate execution as a tracked run |
| GET | `/api/learning/graph` | yes | Learning "star map": learned non-base skills + memory chunks with edges, clusters, stats |
| GET | `/api/learning/node?id=<node>` | yes | Node content prefill (`kind`, `label`, `content`); `404` when unknown |
| PUT | `/api/learning/node` | yes | Rewrite node content (`{"id", "content"}`) — SKILL.md or memory entry |
| DELETE | `/api/learning/node` | yes | Archive a learned skill / remove a memory entry (`{"id"}`) |
| GET | `/v1/skills` | yes | Installed skills (`<home>/skills/*/SKILL.md`) |
| GET | `/v1/toolsets` | yes | Toolsets with resolved tool lists and enabled state |
| POST | `/v1/runs` | yes | Start async run → `202` + `run_id` |
| GET | `/v1/runs` | yes | Tracked runs, newest first |
| GET | `/v1/runs/:id` | yes | Run status/result |
| GET | `/v1/runs/:id/events` | yes | SSE lifecycle events (`run.progress`, `approval.request` → `run.completed`/`run.failed`), closes at terminal state |
| POST | `/v1/runs/:id/stop` | yes | Best-effort stop request |
| POST | `/v1/runs/:id/approval` | yes | Resolve a pending approval: `{"decision": "once"\|"session"\|"always"\|"deny"}` |

**Auth:** `Authorization: Bearer <key>` when `[gateway] key` /
`ULNCLAW_GATEWAY_KEY` is set; open otherwise.

**Request/response example:**
```bash
curl -H "Authorization: Bearer $ULNCLAW_GATEWAY_KEY" \
     -H "Content-Type: application/json" \
     -d '{"messages":[{"role":"user","content":"Hello"}]}' \
     http://127.0.0.1:8642/v1/chat/completions
```

**Run approval flow** (requires `[agent] approval = true`):

```bash
# 1. Start a run; if the model hits a confirm-tier command (sudo, rm -rf,
#    force-push, ...) the run parks in waiting_for_approval:
RUN=$(curl -s -X POST -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
     -d '{"message":"run sudo whoami in the terminal"}' \
     http://127.0.0.1:8642/v1/runs | jq -r .run_id)
curl -s -H "Authorization: Bearer $KEY" http://127.0.0.1:8642/v1/runs/$RUN
# {"status":"waiting_for_approval",
#  "approval":{"command":"sudo whoami","reason":"sudo (elevated privileges)",
#              "choices":["once","session","always","deny"]}, ...}

# 2. Resolve it; the run resumes with the decision:
curl -s -X POST -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
     -d '{"decision":"once"}' http://127.0.0.1:8642/v1/runs/$RUN/approval
```

The SSE stream (`/v1/runs/:id/events`) emits `approval.request` when the run
parks, so frontends can prompt without polling.

**Token streaming** (`stream: true`):

```bash
curl -N -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
     -d '{"stream": true, "messages": [{"role": "user", "content": "Hello"}]}' \
     http://127.0.0.1:8642/v1/chat/completions
# data: {"choices":[{"delta":{"role":"assistant"},...}]}
# data: {"choices":[{"delta":{"content":"He"},...}]}
# ...
# event: hermes.tool.progress        (when the agent runs a tool)
# data: {"tool":"terminal","status":"started"}
# ...
# data: {"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{...}}
# data: [DONE]
```

Content deltas stream as they are produced; `hermes.tool.progress` events
report tool start/complete so frontends can show activity without storing
markers in history.  If the client disconnects mid-stream the agent task is
aborted.  Keepalive comments (`: keepalive`) are sent every 15s.

**Responses streaming** (`POST /v1/responses` with `stream: true`) emits
spec-compliant events as the agent runs:

```
event: response.created                  # envelope, status=in_progress
event: response.output_item.added        # item.type = "function_call"
event: response.output_item.done         # finalized arguments
event: response.output_item.added        # item.type = "function_call_output"
event: response.output_item.done
event: response.output_text.delta        # streamed assistant text
event: response.output_text.done
event: response.completed                # full envelope: output + usage
```

Every event carries a monotonically increasing `sequence_number`.  The
terminal `response.completed` payload has the same shape as the
non-streaming response and is persisted, so `GET /v1/responses/{id}` and
`previous_response_id` chaining keep working.  On agent error a
`response.failed` event terminates the stream.  Client disconnect aborts
the agent task.

**Cron jobs API** (`/api/jobs`, hermes parity):

Jobs live in the gateway's state DB (`<home>/state.db`, `cron_jobs` table —
the same store the `ulnclaw cron` CLI uses).  Schedules accept interval
shorthands (`30m`, `every 2h`, `1d`), 5-field cron expressions
(`0 9 * * *`), and ISO timestamps for one-shot runs.  Validation limits:
name ≤ 200 chars, prompt ≤ 5000 chars, `repeat` a positive integer.

`deliver` (default `local`) controls where the final response is
auto-delivered when a run finishes: `local` (save only), `origin` (the
chat the job was created in — captured by the `cronjob` tool), a
platform name (`telegram`, `discord`, `slack`, `signal`, `matrix`,
`email`, ...), an explicit `platform:chat_id[:thread_id]`, or a
comma-separated mix incl. the `all` routing token. Targets resolve at
fire time against the registered platform senders and the platform home
channel env vars (`TELEGRAM_HOME_CHANNEL`, `MATRIX_HOME_ROOM`, ...);
`[SILENT]` responses suppress delivery, wrapped output carries a
`Cronjob Response: <name>` header (`[cron] wrap_response = false`
disables), failed runs deliver a compact failure summary, and delivery
errors land in the job's `last_delivery_error` field.

¹ `POST /api/jobs/fire` bypasses the bearer key (hermes
`PUBLIC_API_PATHS` parity) — the NAS-minted JWT is the gate. Configure
`[cron.chronos] expected_audience` + `nas_jwks_url` (JWKS URL or inline
PEM public key) + optional `portal_url` issuer; tokens must carry
`purpose = "cron_fire"`. Responses: `401` invalid token, `400` missing
`job_id`, `200 {"status":"gone"}` unknown job (NAS must not retry),
`202 {"status":"accepted"}` with the job run in the background
(claim-based dedup for NAS retries).

```bash
# Create + inspect
JOB=$(curl -s -X POST -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
     -d '{"name":"morning digest","schedule":"0 9 * * *","prompt":"summarize the news"}' \
     http://127.0.0.1:8642/api/jobs | jq -r .job.id)
curl -s -H "Authorization: Bearer $KEY" http://127.0.0.1:8642/api/jobs/$JOB

# Trigger one immediate execution (runs as a tracked run):
RUN=$(curl -s -X POST -H "Authorization: Bearer $KEY" \
     http://127.0.0.1:8642/api/jobs/$JOB/run | jq -r .run_id)
curl -s -H "Authorization: Bearer $KEY" http://127.0.0.1:8642/v1/runs/$RUN
```

`POST /api/jobs/:id/run` returns `{"job": ..., "run_id": ...}`; the job row
records `last_run` and settles `last_status` to `ok` / `error: ...` when the
run finishes.  Errors use a plain `{"error": "message"}` envelope with the
matching HTTP status (400 validation, 404 unknown job, 503 when the cron
store is not attached).

**Session patch & fork** (hermes `_handle_patch_session` /
`_handle_fork_session` parity):

```bash
# Rename / end a session:
curl -s -X PATCH -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
     -d '{"title":"my session"}' http://127.0.0.1:8642/api/sessions/$SID

# Fork: marks the source ended with reason "branched" and creates a child
# session carrying the full transcript forward (201):
curl -s -X POST -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
     -d '{"id":"my-fork","title":"exploration branch"}' \
     http://127.0.0.1:8642/api/sessions/$SID/fork
```

Fork accepts an optional `id`/`session_id` (default: generated
`api_<ts>_<rand>`) and `title` (default: `"<source title> fork"`).  Existing
target id → `409 session_exists`; ids containing newline/NUL → `400`.

**Session model lock** (`POST /api/sessions/:id/model`, hermes
`_handle_session_model_lock` parity):

```bash
curl -s -X POST -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
     -d '{"model":"llama3.1:8b","provider":"ollama"}' \
     http://127.0.0.1:8642/api/sessions/$SID/model
# {"object":"ulnclaw.session.model_lock","session_id":"...",
#  "runtime":{"provider":"ollama","model":"llama3.1:8b",
#             "route_source":"api_request","model_lock":"accepted"}}
```

The lock is persisted on the session row and **enforced**: every later turn
of that session (session chat, chat completions with the session header,
responses chaining, runs, streaming variants) sends the locked model to the
provider via a per-task model override.  Locking back to the gateway's
configured model clears the override.  Forks inherit the lock.  Missing
`model` → `400 model_required`.

**Model options** (`GET /api/model/options`): picker inventory in hermes'
`{providers, model, provider}` shape.  ulnclaw runs one configured
provider, so the inventory is that single row (`slug`, `models`,
`is_user_defined`, `authenticated`, `current`); live catalog probing,
pricing, and capability enrichment are not implemented.

**Session recap** (`GET /api/sessions/:id/recap`): returns
`{"object":"ulnclaw.session.recap","session_id":...,"recap":"..."}` —
the same instant, local-only summary as the CLI `sessions recap` /
REPL `/recap` (recent turn counts, top tools, files touched, last
ask/reply).  No model call is made.

**Metrics** (`GET /metrics`): Prometheus text format (version 0.0.4).
Gauges: `ulnclaw_uptime_seconds`, `ulnclaw_build_info{version,provider,model}`,
`ulnclaw_sessions_total`, `ulnclaw_messages_total`, `ulnclaw_runs_active`,
`ulnclaw_cron_jobs{state}`. Counters:
`ulnclaw_http_requests_total{endpoint}` (chat_completions/responses/session_chat),
`ulnclaw_runs_total{outcome}` (started/completed/failed),
`ulnclaw_tokens_total{direction}` (prompt/completion), `ulnclaw_tool_calls_total`.
Requires the bearer token like every API route; hermes' api_server has no
equivalent — this is an ulnclaw operations extension.

**Usage** (`GET /api/usage?limit=N`): token accounting in JSON.
`process` — counters since this gateway process started: prompt/completion
tokens, tool calls, requests by endpoint (chat_completions/responses/
session_chats), runs by outcome (started/completed/failed). `store` —
all-time totals from the session database (sessions, messages,
input/output tokens). `sessions` — the newest `limit` session rows
(default 20, max 200) with their per-session token counts. Like
`/metrics`, an ulnclaw operations extension — hermes' api_server has no
equivalent.

**Discovery endpoints**: `GET /v1/skills` returns
`{"object":"list","data":[{name, description, category, path}]}` from the
gateway's skills directory.  `GET /v1/toolsets` returns each toolset with
its description, resolved `tools` list, and `enabled` — true when the
gateway agent actually exposes at least one of the toolset's tools.

### Provider Retry

`OpenAiProvider` retries transient failures — network errors and HTTP
408/429/500/502/503/504 — with exponential backoff (500ms → 1s → 2s…,
capped at 8s, plus jitter) before surfacing the error.  Both the
non-streaming and streaming paths retry the initial request.  Configure via
`[model] max_retries` (default 2) or `OpenAiProviderBuilder::max_retries`.

# Architecture Guide

ulnclaw is designed as a modular, high-performance AI agent engine with clear separation of concerns.

## System Overview

```
┌─────────────────────────────────────────────────────────────────┐
│                        Entry Points                              │
│  CLI (chat REPL / run / sessions / cron)   HTTP Gateway          │
│  Agent::run() / run_with_session()         (OpenAI-compatible)   │
└──────────┬──────────────────┬───────────────────────┬───────────┘
           │                  │                       │
           ▼                  ▼                       ▼
┌─────────────────────────────────────────────────────────────────┐
│                     Agent (conversation loop)                    │
│                                                                  │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐           │
│  │ Prompt       │  │ Provider     │  │ Tool         │           │
│  │ Builder      │  │ Resolution   │  │ Dispatch     │           │
│  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘           │
│         │                 │                 │                   │
│  ┌──────┴───────┐  ┌──────┴───────┐  ┌──────┴───────┐           │
│  │ Compression  │  │ OpenAI       │  │ Tool Registry│           │
│  │ (budget)     │  │ Compatible   │  │ + MCP tools  │           │
│  └──────────────┘  └──────────────┘  └──────────────┘           │
└─────────┴─────────────────┴─────────────────┴───────────────────┘
           │                │                   │
           ▼                ▼                   ▼
┌───────────────────┐ ┌──────────────┐ ┌──────────────────────────┐
│ SQLite State      │ │ Providers    │ │ Tool Backends             │
│ sessions/messages │ │ OpenAI/Ollama│ │ terminal, files, web,     │
│ FTS5 + lineage    │ │ DashScope/…  │ │ browser (CDP), HA, kanban │
└───────────────────┘ └──────────────┘ └──────────────────────────┘
```

## Core Components

### 1. Agent Module (`agent/mod.rs`)

The heart of ulnclaw, implementing the conversation loop that orchestrates all components.

**Key Types:**
- `Agent` - Main orchestrator struct
- `AgentConfig` - Configuration (max iterations, temperature, etc.)
- `RunResult` - Output containing content, history, usage stats
- `AgentCallbacks` - Event hooks for UI integration

**Conversation Loop:**
```
User Message → Build Prompt → Call Provider → 
  ├─ Tool Calls → Execute Tools → Append Results → Loop
  └─ Final Response → Return to User
```

**Design Decisions:**
- **Iteration Limits**: Prevents infinite loops (default: 50 iterations)
- **Tool Execution**: Sequential by default, concurrent option available
- **Thinking Blocks**: Automatically strips `<think>...</think>` tags from responses
- **Error Recovery**: Graceful degradation when tools fail

### 2. Provider Module (`provider/`)

Abstract interface for AI model backends with OpenAI-compatible implementation.

**Key Types:**
- `Provider` - Trait defining the provider interface
- `OpenAiProvider` - OpenAI-compatible implementation
- `AnthropicProvider` - native Anthropic Messages API (hermes'
  `anthropic_messages` transport)
- `Message`, `ToolCall`, `Usage` - Core conversation types
- `ProviderConfig` - Dynamic provider instantiation

**Supported Providers:**
- OpenAI (GPT-4, GPT-3.5)
- Anthropic (Claude, via OpenAI-compatible format)
- DashScope (Alibaba Qwen models)
- OpenRouter (multi-model gateway)
- Ollama (local models)
- llama.cpp (local server)

**Request/Response Flow:**
```
ProviderRequest {
  messages: Vec<Message>,
  tools: Vec<ToolDefinition>,
  model: String,
  max_tokens: Option<u32>,
  temperature: Option<f32>,
}

↓ HTTP POST to /v1/chat/completions

ProviderResponse {
  content: Option<String>,
  tool_calls: Vec<ToolCall>,
  usage: Option<Usage>,
  model: String,
  reasoning: Option<String>,
  finish_reason: Option<String>,
}
```

### 3. Tools Module (`tools/mod.rs`)

Self-registering tool system with JSON schema validation and async handlers.

**Key Types:**
- `ToolRegistry` - Central registry managing all tools
- `Tool` - Tool definition with handler
- `ToolBuilder` - Fluent API for tool creation
- `ToolDefinition` - JSON schema for model-facing descriptions

**Tool Registration Pattern:**
```rust
let tool = tool("get_weather")
    .description("Get current weather for a city")
    .parameters(json!({
        "type": "object",
        "properties": {
            "city": {"type": "string"}
        },
        "required": ["city"]
    }))
    .handler(|args, _ctx| async move {
        let city = args["city"].as_str().unwrap();
        Ok(json!({"temp": "22°C", "condition": "sunny"}))
    })
    .toolset("weather")
    .build()?;

registry.register(tool);
```

**Toolset Management:**
- Group related tools into toolsets
- Enable/disable entire toolsets at runtime
- Query available toolsets and their tools

### 4. Session Module (`session/mod.rs`)

Conversation persistence with lineage tracking for compression scenarios.

**Key Types:**
- `SessionStore` - Trait for storage backends
- `MemorySessionStore` - In-memory implementation
- `SqliteSessionStore` - Production backend (hermes_state schema: sessions,
  messages, system_prompts, state_meta, async_delegations; FTS5 full-text
  search with LIKE fallback; parent/child lineage; session-row queries for
  the gateway API)
- `Session` - Conversation state with metadata
- `SessionMetadata` - User ID, platform, model info
- `export` - verifiable Markdown session export (hermes
  `session_export_md.py` / `session_export_html.py` implementation): frontmatter +
  message headings + tool-call blocks + SHA256 verification footer, or a
  standalone styled HTML document — both with `manifest.jsonl` entries
- `recap` - instant local-only session recap (hermes `session_recap.py`
  implementation): recent-window turn counts, tool-usage top list, files touched,
  last ask/reply previews; ANSI/control-char sanitized

**Session Lifecycle:**
```
Create Session → Add Messages → Save → Load → Continue → Save
     │                                                    │
     └──────────────── Lineage Tracking ──────────────────┘
```

**Features:**
- Unique session IDs (UUID v4)
- Conversation history preservation
- Full-text search across sessions
- Metadata tracking (user, platform, model)
- Parent-child relationships for compressed sessions

### 5. Context Module (`context/mod.rs`)

Intelligent prompt building and context window management.

**Key Types:**
- `PromptBuilder` - Tiered system prompt assembly
- `ContextCompressor` - Context window optimization

**Prompt Building Strategy:**
```
System Prompt = Identity + Tool Guidance + Skills + 
                Context Files + Environment Hints + 
                Memory + Timestamp
```

**Tiered Assembly:**
1. **Stable Tier**: Identity, tool guidance, skills (rarely changes)
2. **Context Tier**: Context files, environment hints (per-session)
3. **Volatile Tier**: Memory, timestamp (per-request)

**Context Compression:**
- Estimates token count (rough: 4 chars per token)
- Detects when context exceeds limits
- Placeholder for future model-based summarization

### 6. Error Module (`error.rs`)

Comprehensive error types with automatic conversion.

**Error Categories:**
- `AgentError::Provider` - API failures, auth errors, rate limits
- `AgentError::Tool` - Tool execution failures
- `AgentError::ToolNotFound` - Missing tool in registry
- `AgentError::Session` - Persistence errors
- `AgentError::Context` - Context management errors
- `AgentError::Config` - Configuration errors
- `AgentError::IterationLimit` - Max iterations exceeded
- `AgentError::Http` - Network errors (from reqwest)
- `AgentError::Json` - Serialization errors
- `AgentError::Internal` - Generic internal errors

### 7. Browser Module (`browser/mod.rs`)

Chrome DevTools Protocol client backing the `browser_*` tools.

**Key Types:**
- `BrowserEndpoint` / `resolve_endpoint` - parses `ULNCLAW_BROWSER_CDP`
  (`ws://...` direct endpoint, or `http://host:port` discovery)
- `CdpClient` - WebSocket JSON-RPC with request/response demultiplexing and
  prefix-based event subscriptions (`Page.*`, `Runtime.*`, ...); liveness
  tracking (`is_connected`) — the read/write loops flip a closed flag on
  socket loss and fail every in-flight call fast, so a dead browser never
  wedges later calls on the 30s per-call timeout
- `BrowserSession` - one attached page target: navigate (load-event
  bounded), accessibility snapshot with numbered element refs, click/type
  through `DOM.resolveNode` + `Runtime.callFunctionOn` (CSS selector
  fallback), scroll/press/images/screenshot/evaluate, JS dialog tracking
- `with_session` - global shared-session manager used by all browser tools;
  a cached session with a dead CDP connection is dropped and reopened
  transparently (hermes session-health parity)
- Browser supervisor: `ULNCLAW_BROWSER_CDP=auto` (the default) launches a
  managed headless Chrome/Chromium (`find_browser_binary`,
  `launch_managed_browser`, `stop_managed_browser`) and waits for the
  DevTools implementation before connecting
- SSRF/private-page guard (`browser/guard.rs`, hermes browser-tool guards):
  sensitive-query and cloud-metadata floors fire unconditionally; the
  private-address guard activates for non-loopback endpoints or
  containerized terminals (`[security] allow_private_urls` opts out);
  post-redirect recheck blanks pages that land on blocked addresses;
  console/eval expressions are pre-screened for private URL literals; raw
  `browser_cdp` calls are allowlisted while the page is private; browser
  outputs are force-redacted before reaching the model
- Local CDP attach layer (`browser/connect.rs`, hermes `browser_connect.py`
  implementation): Chromium-family candidate discovery across macOS/Windows/Linux
  (incl. WSL `/mnt/c` installs), dual-stack loopback probes
  (`discover_local_cdp_url` checks `127.0.0.1` then `[::1]`), port
  arbitration (`local_port_in_use` → `find_free_debug_port`), and a
  diagnostics-rich visible debug-browser launch (`launch_chrome_debug`:
  per-candidate attempts, stderr tail in `<home>/chrome-debug/launch-stderr.log`,
  single-instance absorption hint, `manual_chrome_debug_command` fallback)
- Live endpoint override: REPL `/browser connect [url]` and gateway
  `POST /v1/browser/connect` (verified against `/json/version` before it
  sticks) set a process-lifetime override ahead of `ULNCLAW_BROWSER_CDP`;
  bare `/browser connect` runs the hermes default flow — probe both
  loopbacks on implementation 9222, arbitrate a squatted implementation, auto-launch the debug
  browser — and injects a system note so the model knows browser tools are
  live; `/browser disconnect` + `POST /v1/browser/disconnect` clear it;
  `GET /v1/browser/status` reports source/mode
- Camofox backend (`browser/camofox.rs`): when `CAMOFOX_URL` is set and no
  CDP override is active, all 12 browser tools route through the Camofox
  REST API (Camoufox anti-detect browser) instead of CDP — tab sessions
  with identity override/adoption, auto-snapshot on navigate, Docker
  loopback URL rewriting, VNC discovery, and the same SSRF private-page
  guard on reads

**Flow:**
```
browser_navigate → with_session → resolve_endpoint → CDP connect
  → Page.enable/Runtime.enable/DOM.enable/Accessibility.enable
  → Page.navigate → wait Page.loadEventFired → page_info
```

### 8. Gateway Module (`gateway/mod.rs`)

HTTP surface (axum) exposing the agent to OpenAI-compatible clients —
an implementation matching hermes' `gateway/platforms/api_server.py` core.

**Key Types:**
- `GatewayState` - agent + store + model identity + optional bearer key +
  run registry + approval router + optional cron store / skills dir
- `RunState` - tracked async run (`/v1/runs`): status, result, stop flag,
  pending approval payload
- `ApprovalRouter` - run_id → approval channel; `once`/`session`/`always`
  grants are remembered per scope; task-local `current_run_id()`
- `router()` / `serve()` - route table and listener

**Endpoints:**
- `GET /health`, `/health/detailed`, `/v1/health` (always open)
- `GET /v1/models`, `GET /api/model/options`, `GET /v1/capabilities`
- `POST /v1/chat/completions` - OpenAI format; opt-in session continuity
  via `X-Ulnclaw-Session-Id` (history loaded from the SQLite store and the
  same session id resumed via `Agent::run_with_session`); `stream: true`
  returns SSE `chat.completion.chunk` events (token deltas,
  `hermes.tool.progress` events, usage, `[DONE]`)
- `POST/GET/DELETE /v1/responses` - OpenAI Responses format, stateful via
  `previous_response_id`
- `GET/POST /api/sessions`, `GET/DELETE /api/sessions/:id`,
  `PATCH /api/sessions/:id` (title/end_reason),
  `POST /api/sessions/:id/fork` (branch into a child session),
  `POST /api/sessions/:id/model` (per-session model lock, enforced via a
  task-local provider model override on every turn),
  `GET /api/sessions/:id/messages`, `POST /api/sessions/:id/chat`,
  `POST /api/sessions/:id/chat/stream` (SSE),
  `GET /api/sessions/:id/recap` (local turn recap, no LLM call)
- `GET/POST /api/jobs`, `GET/PATCH/DELETE /api/jobs/:id`,
  `POST /api/jobs/:id/pause|resume|run` — cron job management over HTTP
  (shares the CLI's SQLite job store)
- `GET /v1/skills`, `GET /v1/toolsets` — discovery of installed skills and
  resolved toolsets
- `POST /v1/runs` (202 + run_id), `GET /v1/runs`, `GET /v1/runs/:id`,
  `GET /v1/runs/:id/events` (SSE lifecycle events incl.
  `approval.request`), `POST /v1/runs/:id/stop`,
  `POST /v1/runs/:id/approval` (resolve pending approval)
- `GET /v1/browser/status`, `POST /v1/browser/connect`,
  `POST /v1/browser/disconnect` — live browser CDP endpoint control:
  connect verifies the endpoint against `/json/version` before applying
  the process-lifetime override; status reports backend/source/mode
  (managed/endpoint/camofox); disconnect reverts to default mode
- `GET /metrics` — Prometheus counters/gauges (requests, tokens, tool
  calls, active sessions/runs; ulnclaw ops extension)
- `GET /api/usage` — token accounting: process counters, all-time store
  totals, per-session rows (ulnclaw ops extension)
- `GET /v1/delegations`, `GET /v1/delegations/:id` — background
  delegation registry (ulnclaw ops extension)

**Profile multiplexing (`/p/<profile>`):** every route is additionally
mirrored under `/p/<profile>/...` (hermes api_server parity). With
`[gateway] multiplex_profiles = true` each mirror is backed by its own
gateway stack — agent built from the `[profiles.<name>]` override
(model/toolsets) with a profile-scoped home (`<home>/profiles/<name>`:
state.db, approvals.json, cron store, skills dir) — built lazily on first
use and cached (`ProfileHub`); unknown profiles get a 404
`Unknown or unconfigured profile`. With multiplexing off the prefix is
accepted but ignored (the default profile serves it), matching hermes
`_resolve_request_profile`. Mirrored requests pass through the same
bearer-auth middleware as native routes.

**Security:** bearer-token middleware (constant-time compare) on all routes
except health probes; key comes from `[gateway] key` or
`ULNCLAW_GATEWAY_KEY`. Runs always own a session; confirm-tier commands
park the run in `waiting_for_approval` until resolved over HTTP
(`once`/`session`/`always`/`deny`) or the `[approvals] timeout` expires
(fail-closed auto-deny with run-state cleanup). `always` grants persist to
`approvals.json`. Requests without a run context (chat-completions)
auto-deny confirm-tier commands.

### 9. Models.dev Catalog (`models_dev.rs`)

Multi-provider model inventory aligned with hermes `agent/models_dev.py`.
Fetches the community models.dev registry with a three-tier cache:

1. **In-memory** — 1h TTL; stale data is served immediately while a single
   background thread refreshes (callers never block on the network while a
   cache exists).
2. **Disk** — `$ULNCLAW_HOME/models_dev_cache.json` (atomic write), served
   at any age; fresh files anchor the in-memory TTL to the file age.
3. **Network** — singleflight foreground fetch only when no cache exists;
   failed refreshes arm a 5-minute process-wide backoff and fall back to any
   stale disk cache.

The registry URL is overridable via `ULNCLAW_MODELS_DEV_URL` (http(s) or
`file://` mirrors) and the disk cache path via `ULNCLAW_MODELS_DEV_CACHE`.
Query surface: context-window lookup (case-insensitive + `:cloud`/`-cloud`
suffix fallback), capability metadata, provider/model info, and agentic
model lists (noise patterns + Google hidden list). The gateway enriches
`GET /api/model/options` from the catalog (`?refresh=true` forces a
re-read); the CLI exposes `ulnclaw models providers|list|info|refresh`.

### Supporting Modules

- `config/` - `config.toml` + `.env` + profiles (`UlncLawConfig`,
  `GatewayConfig`, env overrides such as `ULNCLAW_HOME`)
- `toolsets.rs` - hermes-compatible tool grouping (33 toolsets, composition,
  enable/disable policy)
- `cron/` - schedule parsing (`30m`, `every 2h`, cron expressions, ISO
  one-shot) + job store + poll scheduler
- `skills/` - SKILL.md discovery and prompt injection
- `mcp/` - MCP stdio client; server tools register as
  `mcp__<server>__<tool>`
- `environments.rs` - terminal backends: local / docker (auto container
  creation) / ssh; command wrapping + shell quoting
- `checkpoint.rs` - transparent git-backed snapshots (shared bare store,
  per-project refs/indexes, pre-edit hooks, restore/diff/prune)
- `logs.rs` - rotating file logging (`<home>/logs/agent.log` INFO+,
  `errors.log` WARNING+, `gateway.log` gateway targets; hermes
  `_LOG_FORMAT` line format) plus the `ulnclaw logs` viewer
  (tail/follow/level/session/since/component filters)
- `backup.rs` - backup/restore: full zip of `<home>` with exclusion
  rules + WAL-safe SQLite snapshots, import overlay, quick state
  snapshots (`state-snapshots/`), cron safety net; wired into
  `ulnclaw update` (pre-update snapshot)

## Data Flow

### Complete Conversation Flow

```
1. User Input
   ↓
2. Agent::run()
   ├─ Load conversation history (optional)
   ├─ Add system prompt
   ├─ Add user message
   ↓
3. Conversation Loop (iteration 1..N)
   ├─ Build API request
   │   ├─ Serialize messages
   │   ├─ Collect tool definitions
   │   └─ Set model parameters
   ├─ Call Provider
   │   ├─ HTTP POST to API endpoint
   │   ├─ Parse response
   │   └─ Extract content/tool_calls
   ├─ Check for tool calls
   │   ├─ Yes: Execute tools
   │   │   ├─ Dispatch to handler
   │   │   ├─ Capture result
   │   │   ├─ Add to messages
   │   │   └─ Continue loop
   │   └─ No: Final response
   └─ Check iteration limit
   ↓
4. Return RunResult
   ├─ Final content
   ├─ Complete conversation history
   ├─ Token usage statistics
   ├─ Iteration count
   └─ Tool call records
```

## Design Principles

### 1. Prompt Stability
System prompt doesn't change mid-conversation. This enables:
- Efficient caching (Anthropic prefix caching)
- Consistent agent behavior
- Predictable token usage

### 2. Observable Execution
Every operation is visible through callbacks:
- `on_tool_start` - Before tool execution
- `on_tool_complete` - After tool execution
- `on_stream_delta` - Streaming text chunks
- `on_thinking` - Model thinking phase
- `on_step` - Each iteration complete

### 3. Interruptible
All async operations can be cancelled:
- HTTP requests via `tokio::select!`
- Tool execution via cancellation tokens
- Clean shutdown without resource leaks

### 4. Platform-Agnostic Core
One `Agent` serves multiple use cases:
- CLI applications
- Web services
- API servers
- Embedded systems

### 5. Loose Coupling
Optional subsystems use registry patterns:
- Tools register themselves, no hard dependencies
- Providers implement traits, swappable at runtime
- Sessions use trait objects, backend-agnostic

## Performance Considerations

### Memory Management
- **Arc for Providers**: Shared ownership across async tasks
- **Mutex for Registry**: Thread-safe tool access
- **Efficient Serialization**: serde_json with zero-copy where possible

### Concurrency
- **Async/Await**: Non-blocking I/O throughout
- **Concurrent Tool Execution**: Optional parallel tool calls
- **Tokio Runtime**: Configurable thread pool

### Network Optimization
- **Connection Pooling**: reqwest client reuse
- **Timeout Configuration**: 120s default, configurable
- **Error Retry**: Automatic retry with exponential backoff (planned)

## Extensibility Points

### Adding New Providers
1. Implement `Provider` trait
2. Add request/response types
3. Register in `ProviderConfig::build()`

### Adding New Tools
1. Use `ToolBuilder` fluent API
2. Define JSON schema
3. Implement async handler
4. Register with `ToolRegistry`

### Adding New Session Backends
1. Implement `SessionStore` trait
2. Add persistence logic
3. Swap in `Agent` constructor

### Custom Context Management
1. Extend `PromptBuilder` with new tiers
2. Implement compression algorithms
3. Hook into agent loop

## Security Considerations

### API Key Management
- Keys stored in `ProviderConfig`, not hardcoded
- Support for environment variables
- Never logged or exposed in errors

### Tool Execution Sandboxing
- Tools run in async context
- No direct filesystem access by default
- Handlers responsible for their own security

### Input Validation
- JSON schema validation for tool arguments
- Error types for invalid inputs
- Graceful degradation on malformed data

## Future Enhancements

### Planned Features
- **Messaging Platforms**: hermes' Telegram/WhatsApp/QQ adapters
- **More Environments**: modal/daytona/vercel terminal backends
  (local/docker/ssh are implemented)
- **Anthropic Native**: Direct Claude API support
- **Retry Logic**: Automatic retry with backoff
- **Metrics**: Prometheus/OpenTelemetry integration

### Architecture Evolution
- **Plugin System**: Dynamic tool/provider loading
- **Distributed Agents**: Multi-node agent coordination

## References

- [Hermes Agent Architecture](https://github.com/NousResearch/hermes-agent)
- [OpenAI API Documentation](https://platform.openai.com/docs)
- [Rust Async Book](https://rust-lang.github.io/async-book/)
- [Tokio Documentation](https://tokio.rs/tokio/tutorial)

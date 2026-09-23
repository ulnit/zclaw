# API 参考

ulnclaw 完整 API 文档。

## 目录

- [核心类型](#核心类型)
- [提供商类型](#提供商类型)
- [工具类型](#工具类型)
- [会话类型](#会话类型)
- [上下文类型](#上下文类型)
- [错误类型](#错误类型)
- [实用函数](#实用函数)
- [Prelude 模块](#prelude-模块)

## 核心类型

### Agent

AI 对话的主编排器。

```rust
pub struct Agent {
    provider: Arc<dyn Provider>,
    tools: Arc<Mutex<ToolRegistry>>,
    config: AgentConfig,
    callbacks: Arc<Mutex<AgentCallbacks>>,
}
```

**方法：**

#### `new(provider: Arc<dyn Provider>, tools: ToolRegistry) -> Self`

创建新的代理，带有提供商和工具注册表。

**参数：**
- `provider` - AI 模型提供商（用 Arc 包装以共享所有权）
- `tools` - 包含可用工具的工具注册表

**示例：**
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

设置代理配置。

**参数：**
- `config` - 代理配置结构体

**示例：**
```rust
let agent = agent.with_config(AgentConfig {
    max_iterations: 100,
    max_tokens: Some(4096),
    temperature: Some(0.7),
    system_prompt: Some("你是一个有帮助的助手。".into()),
    ..Default::default()
});
```

#### `with_callbacks(self, callbacks: AgentCallbacks) -> Self`

设置 UI 集成的事件回调。

**参数：**
- `callbacks` - 各种事件的回调函数

#### `async run(user_message: &str, conversation_history: Option<Vec<Message>>) -> Result<RunResult>`

使用用户消息和可选对话历史运行代理。

**参数：**
- `user_message` - 用户的输入消息
- `conversation_history` - 可选的先前消息

**返回：**
- `RunResult` 包含最终响应、历史、使用统计

**示例：**
```rust
let result = agent.run("天气怎么样？", None).await?;
println!("{}", result.content);
println!("使用令牌数：{}", result.usage.total_tokens);
```

#### `async run_with_session(user_message: &str, conversation_history: Option<Vec<Message>>, resume_session_id: Option<&str>) -> Result<RunResult>`

与 `run` 相同，但续用已有会话 id 而非新建。当 `resume_session_id` 为
`Some` 时，确保会话行存在且仅追加新的用户消息（历史行视为已存在）。
HTTP 网关的会话续接即基于此方法。

**参数：**
- `user_message` - 用户输入消息
- `conversation_history` - 可选的历史消息（通常取自该会话的存储）
- `resume_session_id` - 要续用的会话 id，`None` 则新建

**返回：**
- `RunResult`，其 `session_id` 等于续用的 id

#### `async chat(user_message: &str) -> Result<String>`

简单接口，只返回文本响应。

**参数：**
- `user_message` - 用户的输入消息

**返回：**
- 最终文本响应作为 String

**示例：**
```rust
let response = agent.chat("你好！").await?;
println!("{}", response);
```

### AgentConfig

代理配置。

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

**字段：**
- `max_iterations` - 最大工具调用迭代次数（默认：50）
- `max_tokens` - 响应中的最大令牌数
- `temperature` - 采样温度（0.0-2.0）
- `system_prompt` - 代理的系统消息
- `strip_thinking_blocks` - 移除 `<think>` 标签（默认：true）
- `model` - 模型名称覆盖
- `concurrent_tool_execution` - 并行执行工具（默认：false）
- `max_concurrent_tools` - 最大并行工具数（默认：5）

### RunResult

运行代理的结果。

```rust
pub struct RunResult {
    pub content: String,
    pub conversation: Vec<Message>,
    pub usage: Usage,
    pub iterations: usize,
    pub tool_calls: Vec<ToolCallRecord>,
}
```

**字段：**
- `content` - 最终文本响应
- `conversation` - 完整消息历史
- `usage` - 令牌使用统计
- `iterations` - 执行的迭代次数
- `tool_calls` - 所有工具调用的记录

### ToolCallRecord

单个工具调用的记录。

```rust
pub struct ToolCallRecord {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
    pub result: serde_json::Value,
}
```

## 提供商类型

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

`chat_completion_stream` 默认返回"不支持"错误；provider 实现该方法并让
`supports_streaming()` 返回 `true` 即启用流式。

### 流式类型

```rust
/// 流式 chat completion 的单个 SSE chunk。
pub struct StreamChunk {
    pub delta_content: Option<String>,      // 文本增量
    pub delta_reasoning: Option<String>,    // 思考增量（支持的模型）
    pub tool_call_deltas: Vec<ToolCallDelta>,
    pub finish_reason: Option<String>,      // 末块：stop/tool_calls/length
    pub usage: Option<Usage>,               // 末块（include_usage）
    pub model: Option<String>,
}

/// 增量工具调用片段（OpenAI delta.tool_calls[i]）。
pub struct ToolCallDelta {
    pub index: usize,
    pub id: Option<String>,
    pub name_delta: Option<String>,
    pub arguments_delta: Option<String>,
}

pub type ProviderStream = Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>;

/// 将流式工具调用增量重组为完整工具调用。
pub fn assemble_tool_calls(deltas: &[ToolCallDelta]) -> Vec<ToolCall>;
```

`openai::parse_stream_chunk(data)` 解析单个 SSE `data:` 载荷。

### OpenAiProvider

OpenAI 兼容提供商实现。

```rust
pub struct OpenAiProvider { /* 字段隐藏 */ }
```

**构建器方法：**

#### `builder() -> OpenAiProviderBuilder`

创建新的构建器。

#### `OpenAiProviderBuilder::endpoint(self, endpoint: &str) -> Self`

设置 API 端点 URL。

#### `OpenAiProviderBuilder::api_key(self, key: &str) -> Self`

设置 API 密钥。

#### `OpenAiProviderBuilder::model(self, model: &str) -> Self`

设置模型名称。

#### `OpenAiProviderBuilder::name(self, name: &str) -> Self`

设置提供商名称（用于显示）。

#### `OpenAiProviderBuilder::max_tokens(self, max_tokens: u32) -> Self`

设置最大令牌数。

#### `OpenAiProviderBuilder::temperature(self, temp: f32) -> Self`

设置采样温度。

#### `OpenAiProviderBuilder::build(self) -> Result<OpenAiProvider>`

构建提供商。

**示例：**
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

对话消息。

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

消息角色。

```rust
pub enum Role {
    System,    // 系统
    User,      // 用户
    Assistant, // 助手
    Tool,      // 工具
}
```

### ToolCall

模型请求的工具调用。

```rust
pub struct ToolCall {
    pub id: String,
    pub call_type: String,
    pub function: FunctionCall,
}
```

### FunctionCall

函数调用详情。

```rust
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}
```

### Usage

令牌使用统计。

```rust
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}
```

**方法：**

#### `merge(&mut self, other: &Usage)`

合并另一个请求的使用统计。

### ProviderConfig

动态提供商配置。

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

**方法：**

#### `build(&self) -> Result<Box<dyn Provider>>`

从此配置构建提供商。

### ProviderKind

支持的提供商类型。

```rust
pub enum ProviderKind {
    OpenAiCompatible,  // OpenAI 兼容
    Ollama,            // Ollama
    LlamaCpp,          // llama.cpp
    Anthropic,         // Anthropic
    Local,             // 本地
}
```

## 工具类型

### ToolRegistry

所有工具的中心注册表。

```rust
pub struct ToolRegistry { /* 字段隐藏 */ }
```

**方法：**

#### `new() -> Self`

创建新的空注册表。

#### `register(&mut self, tool: Tool)`

注册工具。

#### `unregister(&mut self, name: &str) -> Option<Tool>`

按名称注销工具。

#### `get(&self, name: &str) -> Option<&Tool>`

按名称获取工具。

#### `async dispatch(&self, name: &str, arguments: Value) -> Result<Value>`

使用给定参数执行工具。

#### `definitions(&self) -> Vec<ToolDefinition>`

获取所有启用的工具定义。

#### `names(&self) -> Vec<String>`

获取所有工具名称。

#### `has(&self, name: &str) -> bool`

检查工具是否存在。

#### `len(&self) -> usize`

获取注册的工具数量。

#### `is_empty(&self) -> bool`

检查注册表是否为空。

#### `enable_toolset(&mut self, name: &str)`

启用工具集。

#### `disable_toolset(&mut self, name: &str)`

禁用工具集。

#### `toolset_names(&self) -> Vec<String>`

获取所有工具集名称。

#### `toolset_tools(&self, toolset: &str) -> Vec<&Tool>`

获取特定工具集中的工具。

### Tool

带有定义和处理器的已注册工具。

```rust
pub struct Tool {
    pub definition: ToolDefinition,
    pub handler: ToolHandler,
    pub toolset: String,
    pub dangerous: bool,
}
```

### ToolDefinition

暴露给模型的工具 Schema。

```rust
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}
```

### ToolBuilder

创建工具的流畅 API。

#### `tool(name: impl Into<String>) -> ToolBuilder`

创建新的工具构建器。

**构建器方法：**

- `description(self, desc: impl Into<String>) -> Self` - 设置描述
- `parameters(self, params: Value) -> Self` - 设置 JSON Schema
- `handler<F, Fut>(self, handler: F) -> Self` - 设置异步处理器
- `toolset(self, toolset: impl Into<String>) -> Self` - 设置工具集
- `dangerous(self, dangerous: bool) -> Self` - 标记为危险
- `build(self) -> Result<Tool>` - 构建工具

**示例：**
```rust
let tool = tool("calculate")
    .description("执行算术运算")
    .parameters(json!({
        "type": "object",
        "properties": {
            "expression": {"type": "string"}
        },
        "required": ["expression"]
    }))
    .handler(|args| async move {
        let expr = args["expression"].as_str().unwrap();
        // 计算表达式
        Ok(json!({"result": 42}))
    })
    .toolset("math")
    .build()?;
```

### ToolResult

工具执行结果。

```rust
pub struct ToolResult {
    pub success: bool,
    pub data: Option<Value>,
    pub error: Option<String>,
}
```

**方法：**

#### `ok(data: Value) -> Self`

创建成功结果。

#### `err(msg: impl Into<String>) -> Self`

创建错误结果。

#### `to_value(&self) -> Value`

转换为 JSON 值。

## 会话类型

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

内存会话存储。

```rust
pub struct MemorySessionStore { /* 字段隐藏 */ }
```

**方法：**

#### `new() -> Self`

创建新的内存存储。

实现 `SessionStore` trait。

### Session

对话会话。

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

会话元数据。

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

创建具有唯一 ID 的新会话。

## 上下文类型

### PromptBuilder

使用分层组装构建系统提示。

```rust
pub struct PromptBuilder { /* 字段隐藏 */ }
```

**方法：**

#### `new() -> Self`

创建新的构建器。

#### `identity(self, identity: impl Into<String>) -> Self`

设置代理身份。

#### `tool_guidance(self, guidance: impl Into<String>) -> Self`

设置工具使用指导。

#### `add_skill(self, skill: impl Into<String>) -> Self`

添加技能/指令。

#### `add_context_file(self, content: impl Into<String>) -> Self`

添加上下文文件内容。

#### `add_env_hint(self, key: impl Into<String>, value: impl Into<String>) -> Self`

添加环境提示。

#### `memory(self, memory: impl Into<String>) -> Self`

设置持久记忆。

#### `suffix(self, suffix: impl Into<String>) -> Self`

设置自定义后缀。

#### `build(&self) -> String`

构建完整的系统提示。

**示例：**
```rust
let prompt = PromptBuilder::new()
    .identity("你是一个有帮助的助手。")
    .tool_guidance("在需要时使用工具。")
    .add_skill("始终保持礼貌。")
    .add_env_hint("OS", "Linux")
    .memory("用户偏好深色模式。")
    .build();
```

### ContextCompressor

上下文窗口优化。

```rust
pub struct ContextCompressor {
    pub max_context_tokens: usize,
    pub target_ratio: f32,
}
```

**方法：**

#### `estimate_tokens(messages: &[Message]) -> usize`

估计令牌数量（粗略：每令牌 4 个字符）。

#### `needs_compression(&self, messages: &[Message]) -> bool`

检查是否需要压缩。

#### `compress(&self, messages: Vec<Message>) -> Vec<Message>`

压缩消息（占位符实现）。

## 错误类型

### AgentError

```rust
pub enum AgentError {
    Provider(String),       // 提供商错误
    Tool(String),           // 工具错误
    ToolNotFound(String),   // 工具未找到
    Session(String),        // 会话错误
    Context(String),        // 上下文错误
    Config(String),         // 配置错误
    IterationLimit(usize),  // 迭代限制
    Http(reqwest::Error),   // HTTP 错误
    Json(serde_json::Error),// JSON 错误
    Internal(String),       // 内部错误
}
```

**辅助方法：**

- `provider(msg: impl Into<String>) -> Self`
- `tool(msg: impl Into<String>) -> Self`
- `session(msg: impl Into<String>) -> Self`
- `config(msg: impl Into<String>) -> Self`

### Result 类型

```rust
pub type Result<T> = std::result::Result<T, AgentError>;
```

## 实用函数

### strip_thinking_blocks(text: &str) -> String

从文本中移除 `<think>...</think>` 和 `<thinking>...</thinking>` 块。

**示例：**
```rust
let cleaned = strip_thinking_blocks("你好 <think>思考中</think> 世界");
assert_eq!(cleaned, "你好  世界");
```

## Prelude 模块

常用用例的便捷导入。

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

**用法：**
```rust
use ulnclaw::prelude::*;
```

## 常量

```rust
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const NAME: &str = env!("CARGO_PKG_NAME");
```

## 函数

### version() -> &'static str

获取版本字符串。

## 完整示例

```rust
use ulnclaw::prelude::*;

#[tokio::main]
async fn main() -> Result<()> {
    // 创建提供商
    let provider = OpenAiProvider::builder()
        .endpoint("https://api.openai.com/v1")
        .api_key("sk-...")
        .model("gpt-4")
        .build()?;

    // 创建工具
    let mut tools = ToolRegistry::new();
    
    tools.register(tool("get_time")
        .description("获取当前时间")
        .handler(|_| async {
            Ok(json!({"time": "2026-08-04 12:00:00"}))
        })
        .build()?);

    // 创建代理
    let agent = Agent::new(Arc::new(provider), tools)
        .with_config(AgentConfig {
            system_prompt: Some("你是一个有帮助的助手。".into()),
            max_iterations: 50,
            ..Default::default()
        });

    // 运行
    let result = agent.run("现在几点了？", None).await?;
    println!("{}", result.content);
    
    Ok(())
}
```

## Browser 模块 (`browser/`)

支撑 `browser_*` 工具的 CDP 客户端。需设置 `ULNCLAW_BROWSER_CDP`
（`ws://...` 端点或 `http://host:port` 发现基址）。

### 端点解析

#### `resolve_endpoint(raw: &str) -> Result<BrowserEndpoint>`

将配置解析为 `browser_ws`（直连 WebSocket）或 `http_base`（HTTP 发现）形式。

#### `async discover_browser_ws(http_base: &str) -> Result<String>`

请求 `/json/version` 并返回 `webSocketDebuggerUrl`。

#### `async list_page_targets(http_base: &str) -> Result<Vec<Value>>` / `create_page_target(http_base, url)`

经 `/json` 发现目标、`PUT /json/new?<url>` 创建页面。

### CdpClient

#### `async connect(ws_url: &str) -> Result<Arc<CdpClient>>`

建立 WebSocket 并启动读/写循环。

#### `async call(method: &str, params: Value) -> Result<Value>`

发送 CDP 命令并等待结果（30 秒超时）。

#### `fn notify(method: &str, params: Value) -> Result<()>`

发送 CDP 事件/通知（不期待响应）。

#### `async subscribe(prefix: &str) -> broadcast::Receiver<Value>`

订阅 method 以 `prefix` 开头的事件。

### BrowserSession

#### `async open(endpoint: &BrowserEndpoint) -> Result<Arc<BrowserSession>>`

查找（或创建）页面目标并附加，启用 Page/Runtime/DOM/Accessibility 域。

| 方法 | 说明 |
|--------|-------------|
| `navigate(url)` | 导航并等待 load 事件（有界等待） |
| `go_back()` | `window.history.back()` |
| `evaluate(expression, timeout_ms)` | `Runtime.evaluate`（returnByValue） |
| `snapshot()` | 可访问性列表 + 编号 `ElementRef` |
| `click(element)` | 按元素引用（`"3"`）或 CSS 选择器点击 |
| `type_text(element, text)` | 聚焦 + 赋值 + input/change 事件 |
| `scroll(direction, pixels)` | `window.scrollBy` |
| `press(key)` | `Input.dispatchKeyEvent`（Enter/Tab/Escape/方向键/字符） |
| `get_images()` | 前 100 个 `document.images` 及尺寸 |
| `screenshot()` | PNG 截图（base64） |
| `page_info()` | `{title, url}` |
| `handle_dialog(accept, prompt_text)` | 处理最近一次 JS 对话框 |

### 浏览器监督器

#### `find_browser_binary() -> Option<PathBuf>`

定位 Chrome/Chromium（`ULNCLAW_BROWSER_PATH` 覆盖，随后是 PATH 与常见安装目录）。

#### `async launch_managed_browser() -> Result<String>`

启动（或复用）托管的无头浏览器于空闲端口，返回其 HTTP 发现基址。
`ULNCLAW_BROWSER_CDP` 未设置或为 `auto` 时自动使用。

#### `async stop_managed_browser()`

终止托管浏览器并释放共享会话。

### 会话管理器

#### `async with_session<F, Fut, T>(func: F) -> Result<T>`

获取（或打开）进程级共享会话并在其上执行 `func`。
所有 `browser_*` 工具处理器以此为入口。

## 环境 (`environments`)

`terminal` 工具的执行后端（hermes `tools/environments/`）。

#### `enum TerminalBackend { Local, Docker { container, image }, Ssh { host, user, port, identity } }`

#### `resolve(config: &TerminalConfig) -> TerminalBackend`

读取 `[terminal] backend`（默认 `"local"`，可选 `"docker"`、`"ssh"`）。

#### `async ensure_docker_container(container: &str, image: &str) -> Result<()>`

先 `docker inspect`；不存在则 `docker run -d --name <container> <image> sleep infinity`。

#### `wrap_command(backend: &TerminalBackend, command: &str) -> Vec<String>`

Local → 普通 shell；Docker → `docker exec --workdir <cwd> <container> bash -lc <quoted>`；
Ssh → `ssh -o BatchMode=yes [-i identity] [-p port] [user@]host cd <cwd> && <command>`。

配置（`config.toml`）：

```toml
[terminal]
backend = "docker"          # "local"（默认）| "docker" | "ssh"
container = "ulnclaw-dev"   # docker 容器名
image = "ubuntu:24.04"      # 容器不存在时用于创建的镜像
ssh_host = "build-box"
ssh_user = "dev"
ssh_port = 22
ssh_identity = "~/.ssh/id_ed25519"
```

## 检查点 (`checkpoint`)

透明文件系统快照（hermes `checkpoint_manager.py`）。`<home>/checkpoints/store`
下的单个共享 bare git 存储，以 `refs/hermes/<hash16>` 维护按项目的快照链、
按项目独立 index —— LLM 完全感知不到该子系统。

#### `CheckpointManager::new(base: PathBuf, config: &CheckpointsConfig) -> Self`

#### `new_turn(&self)`

重置每轮去重（agent 每个迭代开始时调用）。

#### `async ensure_checkpoint(&self, working_dir: &str, reason: &str) -> bool`

每轮每目录最多一次快照；跳过 `/`、`$HOME`、超过 5 万文件的目录、
大于 `max_file_size_mb` 的文件。agent 在 `write_file`/`patch` 分发前调用。

#### `async list_checkpoints(&self, working_dir: &str) -> Vec<CheckpointEntry>`

#### `async restore(&self, working_dir: &str, commit_hash: &str, file_path: Option<&str>) -> Result<Value, String>`

先做一次回滚前快照（可撤销撤销），再 `git checkout <hash> -- <path|.`。

#### `async diff(&self, ...) / session_diff(&self, ...)`

工作区对指定检查点 / 对最早保留检查点的 diff。

#### `async status(&self) -> StoreStatus` / `async prune(&self, retention_days: u64, delete_orphans: bool) -> PruneStats` / `async maybe_auto_prune(&self)`

CLI：`ulnclaw checkpoints list [dir] | status | restore <hash> [file] [--dir D] | diff <hash> [--dir D] | prune [--days N]`。
REPL：`/rollback`（列表）、`/rollback <N|hash> [file]`（恢复）、
`/rollback diff <N|hash>`（预览）、`/diff [session|N|hash]`。

配置（`config.toml`）：

```toml
[checkpoints]
enabled = false          # 总开关（默认关闭）
max_snapshots = 20       # 每项目最多保留 N 个检查点
max_total_size_mb = 500  # 存储总量上限（跨项目丢最旧）
max_file_size_mb = 10    # 跳过大于该值的文件
retention_days = 7       # 过期项目自动清理窗口
auto_prune_hours = 24    # 自动清理周期
```

## HTTP 网关 (`gateway/`)

OpenAI 兼容 API 服务器（`ulnclaw gateway`）。

### 库 API

#### `GatewayState::new(agent: Arc<Agent>, model_name: String, provider_name: String, key: Option<String>, router: Arc<ApprovalRouter>) -> Result<Arc<GatewayState>>`

从已装配的 agent 构建网关状态（必须附加 SQLite 存储）。
`ApprovalRouter` 将 agent 的审批回调连接到 run 级 HTTP 审批处理（见下）。

#### `ApprovalRouter::new() -> Arc<ApprovalRouter>` / `with_options(timeout, persist_path)` / `current_run_id() -> Option<String>`

运行审批管线。router 维护 `run_id → (session_id, channel)` 映射；
CLI 启动网关时为 agent 安装的审批回调读取 task-local 的 run id，
并等待 `router.request(run_id, reason, command)`。`once` 单次放行，
`session` 在该 run 所属会话内记住命令，`always` 在网关生命周期内记住
**并持久化到 `approvals.json`**，`deny` 拒绝。没有 run 上下文的请求
（如 `/v1/chat/completions`）对确认级命令按设计自动拒绝。

`request_outcome` 区分显式 `Denied` 与 `TimedOut`。`[approvals] timeout`
（默认 300s，对齐 hermes）内无人处理时审批 fail-closed：清理该 run 的
待审批项并阻止命令——run 不会永久停泊。

```toml
[approvals]
timeout = 300     # fail-closed 自动拒绝前的秒数（0 = 永久等待）
```

`gateway_approve_fn(router, state)` 构建审批回调，延迟绑定的网关状态
用于超时清理。

#### `router(state: Arc<GatewayState>) -> Router`

axum 路由表（供 `serve` 与测试使用）。

#### `async serve(state: Arc<GatewayState>, host: &str, port: u16) -> Result<()>`

绑定并持续服务直到中断。

### HTTP 端点

| 方法 | 路径 | 鉴权 | 说明 |
|--------|------|------|-------------|
| GET | `/health`、`/v1/health` | 否 | 存活探测 |
| GET | `/health/detailed` | 否 | 详细状态（模型、provider、鉴权、runs） |
| GET | `/v1/models` | 是 | 对外模型列表 |
| GET | `/api/model/options` | 是 | 供选择器使用的 provider/模型清单（已配置的 provider 行） |
| GET | `/v1/capabilities` | 是 | 机器可读的端点目录 |
| GET | `/metrics` | 是 | Prometheus 文本格式计数器与量表（ulnclaw 运维扩展） |
| GET | `/api/usage?limit=N` | 是 | 令牌核算：网关启动以来的进程计数器 + 会话库全时总量 + 最近会话明细（ulnclaw 运维扩展） |
| POST | `/v1/chat/completions` | 是 | OpenAI Chat Completions；经 `X-Ulnclaw-Session-Id` 会话续接（兼容 `X-Hermes-Session-Id`）；id 回显于响应头；`stream: true` → SSE `chat.completion.chunk` |
| POST | `/v1/responses` | 是 | OpenAI Responses 格式；`input` 为字符串或消息数组；经 `previous_response_id` 链式续接；`stream: true` → Responses-API SSE 事件 |
| GET | `/v1/responses/:id` | 是 | 取回已存储的 response |
| DELETE | `/v1/responses/:id` | 是 | 删除已存储的 response |
| GET | `/api/sessions?limit=N` | 是 | 最近会话（新→旧） |
| POST | `/api/sessions` | 是 | 创建空会话 |
| GET | `/api/sessions/:id` | 是 | 会话行 |
| PATCH | `/api/sessions/:id` | 是 | 更新 `title` 和/或 `end_reason`（未知字段 → 400） |
| POST | `/api/sessions/:id/model` | 是 | 将会话锁定到指定模型（`{"model": "...", "provider": "..."}`） |
| GET | `/api/sessions/:id/recap` | 是 | 即时本地活动回顾（不调用 LLM） |
| DELETE | `/api/sessions/:id` | 是 | 硬删除（消息 + FTS 条目） |
| POST | `/api/sessions/:id/fork` | 是 | 分叉会话 → `201` 子会话（转录复制，源会话标记 `branched`） |
| GET | `/api/sessions/:id/messages` | 是 | 消息历史 |
| POST | `/api/sessions/:id/chat` | 是 | 在该会话内执行一轮对话 |
| POST | `/api/sessions/:id/chat/stream` | 是 | 同上，以 SSE chunk 流式返回 |
| GET | `/api/jobs?include_disabled=true` | 是 | 定时任务列表（默认隐藏已停用任务） |
| POST | `/api/jobs` | 是 | 创建定时任务（`name`、`schedule`、`prompt`，可选 `skills`、`repeat`、`deliver`） |
| GET | `/api/jobs/delivery-targets` | 是 | 投递目标下拉清单：隐式 `local` + 已连接平台（含 `home_target_set`） |
| POST | `/api/jobs/fire` | **否**¹ | Chronos NAS 触发 webhook：NAS 铸造 JWT（`[cron.chronos]`）→ 401/400/200-gone/202 + 后台运行 |
| GET | `/api/jobs/:id` | 是 | 单个任务 |
| PATCH | `/api/jobs/:id` | 是 | 更新白名单字段（`name`、`schedule`、`prompt`、`skills`、`repeat`、`enabled`、`deliver`） |
| DELETE | `/api/jobs/:id` | 是 | 删除任务 |
| POST | `/api/jobs/:id/pause` | 是 | 停用任务（清空 `next_run`） |
| POST | `/api/jobs/:id/resume` | 是 | 重新启用任务（重算 `next_run`） |
| POST | `/api/jobs/:id/run` | 是 | 立即触发一次执行（作为被跟踪的运行） |
| GET | `/api/learning/graph` | 是 | 学习"星图"：已学非基础技能 + 记忆块，含连边、簇与统计 |
| GET | `/api/learning/node?id=<node>` | 是 | 节点内容预填充（`kind`、`label`、`content`）；未知返回 `404` |
| PUT | `/api/learning/node` | 是 | 重写节点内容（`{"id", "content"}`）——SKILL.md 或记忆条目 |
| DELETE | `/api/learning/node` | 是 | 归档已学技能 / 删除记忆条目（`{"id"}`） |
| GET | `/v1/skills` | 是 | 已安装技能（`<home>/skills/*/SKILL.md`） |
| GET | `/v1/toolsets` | 是 | 工具集及其解析后的工具列表与启用状态 |
| POST | `/v1/runs` | 是 | 启动异步运行 → `202` + `run_id` |
| GET | `/v1/runs` | 是 | 被跟踪的运行（新→旧） |
| GET | `/v1/runs/:id` | 是 | 运行状态/结果 |
| GET | `/v1/runs/:id/events` | 是 | SSE 生命周期事件（`run.progress`、`approval.request` → `run.completed`/`run.failed`），终态后关闭 |
| POST | `/v1/runs/:id/stop` | 是 | 尽力而为的停止请求 |
| POST | `/v1/runs/:id/approval` | 是 | 解决待审批请求：`{"decision": "once"\|"session"\|"always"\|"deny"}` |

**鉴权：** 设置 `[gateway] key` / `ULNCLAW_GATEWAY_KEY` 后需
`Authorization: Bearer <key>`；未设置则开放。

**请求/响应示例：**
```bash
curl -H "Authorization: Bearer $ULNCLAW_GATEWAY_KEY" \
     -H "Content-Type: application/json" \
     -d '{"messages":[{"role":"user","content":"你好"}]}' \
     http://127.0.0.1:8642/v1/chat/completions
```

**运行审批流程**（需 `[agent] approval = true`）：

```bash
# 1. 启动 run；模型命中确认级命令（sudo、rm -rf、强制推送等）时
#    run 停泊在 waiting_for_approval：
RUN=$(curl -s -X POST -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
     -d '{"message":"run sudo whoami in the terminal"}' \
     http://127.0.0.1:8642/v1/runs | jq -r .run_id)
curl -s -H "Authorization: Bearer $KEY" http://127.0.0.1:8642/v1/runs/$RUN
# {"status":"waiting_for_approval",
#  "approval":{"command":"sudo whoami","reason":"sudo (elevated privileges)",
#              "choices":["once","session","always","deny"]}, ...}

# 2. 解决审批，run 按决定恢复执行：
curl -s -X POST -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
     -d '{"decision":"once"}' http://127.0.0.1:8642/v1/runs/$RUN/approval
```

SSE 流（`/v1/runs/:id/events`）在 run 停泊时发出 `approval.request`
事件，前端无需轮询即可弹出审批提示。

**令牌流式**（`stream: true`）：

```bash
curl -N -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
     -d '{"stream": true, "messages": [{"role": "user", "content": "你好"}]}' \
     http://127.0.0.1:8642/v1/chat/completions
# data: {"choices":[{"delta":{"role":"assistant"},...}]}
# data: {"choices":[{"delta":{"content":"你"},...}]}
# ...
# event: hermes.tool.progress        （agent 执行工具时）
# data: {"tool":"terminal","status":"started"}
# ...
# data: {"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{...}}
# data: [DONE]
```

内容增量随生成随推送；`hermes.tool.progress` 事件报告工具开始/完成，
前端可据此展示活动而不把标记存入历史。客户端中途断开时 agent 任务被
中止。每 15 秒发送 keepalive 注释（`: keepalive`）。

**Responses 流式**（`POST /v1/responses` 带 `stream: true`）在 agent 运行
过程中发出符合规范的事件：

```
event: response.created                  # 信封，status=in_progress
event: response.output_item.added        # item.type = "function_call"
event: response.output_item.done         # 最终 arguments
event: response.output_item.added        # item.type = "function_call_output"
event: response.output_item.done
event: response.output_text.delta        # 流式助手文本
event: response.output_text.done
event: response.completed                # 完整信封：output + usage
```

每个事件携带单调递增的 `sequence_number`。终态 `response.completed` 载荷
与非流式响应同形并持久化，`GET /v1/responses/{id}` 与
`previous_response_id` 链式续接照常可用。agent 出错时以
`response.failed` 事件终止流。客户端断连会中止 agent 任务。

**定时任务 API**（`/api/jobs`，对标 hermes）：

任务存于网关状态库（`<home>/state.db` 的 `cron_jobs` 表——与
`ulnclaw cron` CLI 共用同一存储）。调度支持间隔简写（`30m`、
`every 2h`、`1d`）、5 段 cron 表达式（`0 9 * * *`）以及 ISO 时间戳
一次性任务。校验上限：name ≤ 200 字符、prompt ≤ 5000 字符、
`repeat` 为正整数。

`deliver`（默认 `local`）控制运行结束时最终响应的自动投递去向：
`local`（仅保存）、`origin`（任务创建所在的聊天——由 `cronjob` 工具捕获）、
平台名（`telegram`、`discord`、`slack`、`signal`、`matrix`、`email` 等）、
显式 `platform:chat_id[:thread_id]`，或含 `all` 路由令牌的逗号混合。
目标在触发时按已注册的平台发送器与平台 home 频道环境变量
（`TELEGRAM_HOME_CHANNEL`、`MATRIX_HOME_ROOM` 等）解析；`[SILENT]`
响应抑制投递，包装输出带 `Cronjob Response: <name>` 抬头
（`[cron] wrap_response = false` 关闭），失败运行投递紧凑失败摘要，
投递错误记入任务的 `last_delivery_error` 字段。

¹ `POST /api/jobs/fire` 绕过 bearer key（对齐 hermes `PUBLIC_API_PATHS`）——
NAS 铸造的 JWT 即门闸。配置 `[cron.chronos] expected_audience` +
`nas_jwks_url`（JWKS URL 或内联 PEM 公钥）+ 可选 `portal_url` 签发者；
令牌必须携带 `purpose = "cron_fire"`。应答：`401` 令牌无效、`400` 缺
`job_id`、`200 {"status":"gone"}` 任务不存在（NAS 不应重试）、
`202 {"status":"accepted"}` 任务后台运行（认领去重 NAS 重试）。

```bash
# 创建 + 查看
JOB=$(curl -s -X POST -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
     -d '{"name":"morning digest","schedule":"0 9 * * *","prompt":"summarize the news"}' \
     http://127.0.0.1:8642/api/jobs | jq -r .job.id)
curl -s -H "Authorization: Bearer $KEY" http://127.0.0.1:8642/api/jobs/$JOB

# 立即触发一次执行（作为被跟踪的运行）：
RUN=$(curl -s -X POST -H "Authorization: Bearer $KEY" \
     http://127.0.0.1:8642/api/jobs/$JOB/run | jq -r .run_id)
curl -s -H "Authorization: Bearer $KEY" http://127.0.0.1:8642/v1/runs/$RUN
```

`POST /api/jobs/:id/run` 返回 `{"job": ..., "run_id": ...}`；任务行记录
`last_run`，运行结束后 `last_status` 落为 `ok` / `error: ...`。
错误使用朴素 `{"error": "message"}` 信封并匹配 HTTP 状态码
（400 校验失败、404 任务不存在、503 未挂载 cron 存储）。

**会话补丁与分叉**（对标 hermes `_handle_patch_session` /
`_handle_fork_session`）：

```bash
# 重命名 / 结束会话：
curl -s -X PATCH -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
     -d '{"title":"my session"}' http://127.0.0.1:8642/api/sessions/$SID

# 分叉：源会话以 "branched" 结束，创建携带完整转录的子会话（201）：
curl -s -X POST -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
     -d '{"id":"my-fork","title":"exploration branch"}' \
     http://127.0.0.1:8642/api/sessions/$SID/fork
```

fork 可传 `id`/`session_id`（缺省生成 `api_<ts>_<rand>`）与 `title`
（缺省 `"<源标题> fork"`）。目标 id 已存在 → `409 session_exists`；
含换行/NUL 的 id → `400`。

**会话模型锁**（`POST /api/sessions/:id/model`，对标 hermes
`_handle_session_model_lock`）：

```bash
curl -s -X POST -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
     -d '{"model":"llama3.1:8b","provider":"ollama"}' \
     http://127.0.0.1:8642/api/sessions/$SID/model
# {"object":"ulnclaw.session.model_lock","session_id":"...",
#  "runtime":{"provider":"ollama","model":"llama3.1:8b",
#             "route_source":"api_request","model_lock":"accepted"}}
```

锁定持久化在会话行上并且**实际生效**：该会话之后的每一轮（会话聊天、
带会话头的 chat completions、responses 链式续接、runs、流式变体）都会
通过 per-task 模型覆盖把锁定模型发给 provider。重新锁定回网关配置的
模型即清除覆盖。fork 继承锁定。缺少 `model` → `400 model_required`。

**模型清单**（`GET /api/model/options`）：以 hermes 的
`{providers, model, provider}` 形状返回选择器清单。ulnclaw 只运行一个
已配置的 provider，因此清单就是这一行（`slug`、`models`、
`is_user_defined`、`authenticated`、`current`）；在线目录探测、定价与
能力富化未实现。

**会话回顾**（`GET /api/sessions/:id/recap`）：返回
`{"object":"ulnclaw.session.recap","session_id":...,"recap":"..."}` ——
与 CLI `sessions recap` / REPL `/recap` 相同的即时本地摘要
（近期轮次数、常用工具、涉及文件、最近的提问/回复）。
不调用模型。

**指标**（`GET /metrics`）：Prometheus 文本格式（version 0.0.4）。
量表：`ulnclaw_uptime_seconds`、`ulnclaw_build_info{version,provider,model}`、
`ulnclaw_sessions_total`、`ulnclaw_messages_total`、`ulnclaw_runs_active`、
`ulnclaw_cron_jobs{state}`。计数器：
`ulnclaw_http_requests_total{endpoint}`（chat_completions/responses/session_chat）、
`ulnclaw_runs_total{outcome}`（started/completed/failed）、
`ulnclaw_tokens_total{direction}`（prompt/completion）、`ulnclaw_tool_calls_total`。
与其他 API 路由一样需要 Bearer 令牌；hermes api_server 无对应端点——
此为 ulnclaw 运维扩展。

**用量**（`GET /api/usage?limit=N`）：JSON 格式的令牌核算。
`process` —— 本网关进程启动以来的计数器：prompt/completion 令牌、
工具调用数、按端点统计的请求数（chat_completions/responses/session_chats）、
按结果统计的运行数（started/completed/failed）。`store` —— 会话数据库的
全时总量（会话数、消息数、输入/输出令牌）。`sessions` —— 最近 `limit` 条
会话明细（默认 20，上限 200）及各会话令牌计数。与 `/metrics` 一样，
属于 ulnclaw 运维扩展——hermes api_server 无对应端点。

**发现端点**：`GET /v1/skills` 返回
`{"object":"list","data":[{name, description, category, path}]}`，
来自网关 skills 目录。`GET /v1/toolsets` 返回每个工具集的描述、
解析后的 `tools` 列表与 `enabled`（网关 agent 实际暴露该工具集至少
一个工具时为 true）。

### Provider 重试

`OpenAiProvider` 对瞬态故障——网络错误与 HTTP 408/429/500/502/503/504——
按指数退避（500ms → 1s → 2s……，上限 8s，附抖动）重试后才上抛错误。
非流式与流式路径都会重试初始请求。经 `[model] max_retries`（默认 2）或
`OpenAiProviderBuilder::max_retries` 配置。

## 下一步

- 阅读 [架构指南](architecture.md) 了解系统设计
- 阅读 [集成指南](integration.md) 了解在应用中使用 ulnclaw
- 阅读 [工具系统](tools.md) 了解构建自定义工具
- 阅读 [提供商系统](providers.md) 了解实现新提供商

# 架构指南

ulnclaw 采用模块化、高性能的 AI Agent 引擎设计，具有清晰的关注点分离。

## 目录

- [系统概览](#系统概览)
- [核心组件](#核心组件)
- [数据流](#数据流)
- [设计原则](#设计原则)
- [性能考虑](#性能考虑)
- [扩展点](#扩展点)
- [安全考虑](#安全考虑)
- [未来增强](#未来增强)

## 系统概览

```
┌─────────────────────────────────────────────────────────────────┐
│                         入口点                                   │
│  CLI（chat REPL / run / sessions / cron）   HTTP 网关             │
│  Agent::run() / run_with_session()         （OpenAI 兼容）        │
└──────────┬──────────────────┬───────────────────────┬───────────┘
           │                  │                       │
           ▼                  ▼                       ▼
┌─────────────────────────────────────────────────────────────────┐
│                    Agent (对话循环)                               │
│                                                                  │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐           │
│  │ Prompt       │  │ Provider     │  │ Tool         │           │
│  │ Builder      │  │ Resolution   │  │ Dispatch     │           │
│  │ 提示构建器    │  │ 提供商解析    │  │ 工具分发      │           │
│  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘           │
│         │                 │                 │                   │
│  ┌──────┴───────┐  ┌──────┴───────┐  ┌──────┴───────┐           │
│  │ Compression  │  │ OpenAI       │  │ Tool Registry│           │
│  │ （预算触发）   │  │ Compatible   │  │ + MCP 工具    │           │
│  │ 上下文压缩    │  │ 兼容提供商    │  │ 工具注册表    │           │
│  └──────────────┘  └──────────────┘  └──────────────┘           │
└─────────┴─────────────────┴─────────────────┴───────────────────┘
           │                │                   │
           ▼                ▼                   ▼
┌───────────────────┐ ┌──────────────┐ ┌──────────────────────────┐
│ SQLite 状态库      │ │ Providers    │ │ 工具后端                  │
│ sessions/messages │ │ OpenAI/Ollama│ │ terminal、files、web、    │
│ FTS5 + 谱系       │ │ DashScope/…  │ │ browser(CDP)、HA、kanban  │
└───────────────────┘ └──────────────┘ └──────────────────────────┘
```

## 核心组件

### 1. Agent 模块 (`agent/mod.rs`)

ulnclaw 的核心，实现编排所有组件的对话循环。

**关键类型：**
- `Agent` - 主编排器结构体
- `AgentConfig` - 配置（最大迭代次数、温度等）
- `RunResult` - 输出，包含内容、历史、使用统计
- `AgentCallbacks` - UI 集成的事件钩子

**对话循环：**
```
用户消息 → 构建提示 → 调用提供商 → 
  ├─ 工具调用 → 执行工具 → 追加结果 → 循环
  └─ 最终响应 → 返回给用户
```

**设计决策：**
- **迭代限制**：防止无限循环（默认：50 次迭代）
- **工具执行**：默认顺序执行，可选并发执行
- **思考块**：自动从响应中剥离 `<think>...</think>` 标签
- **错误恢复**：工具失败时优雅降级

**代码示例：**
```rust
let agent = Agent::new(Arc::new(provider), tools)
    .with_config(AgentConfig {
        max_iterations: 50,
        system_prompt: Some("你是一个有帮助的助手。".into()),
        ..Default::default()
    });

let result = agent.run("你好", None).await?;
println!("{}", result.content);
println!("使用了 {} 次迭代", result.iterations);
```

### 2. Provider 模块 (`provider/`)

AI 模型后端的抽象接口，带有 OpenAI 兼容实现。

**关键类型：**
- `Provider` - 定义提供商接口的 trait
- `OpenAiProvider` - OpenAI 兼容实现
- `AnthropicProvider` - 原生 Anthropic Messages API（hermes 的
  `anthropic_messages` 传输层）
- `Message`, `ToolCall`, `Usage` - 核心对话类型
- `ProviderConfig` - 动态提供商实例化

**支持的提供商：**
- OpenAI (GPT-4, GPT-3.5)
- Anthropic (Claude，通过 OpenAI 兼容格式)
- DashScope (阿里巴巴 Qwen 模型)
- OpenRouter (多模型网关)
- Ollama (本地模型)
- llama.cpp (本地服务器)

**请求/响应流程：**
```
ProviderRequest {
  messages: Vec<Message>,        // 对话历史
  tools: Vec<ToolDefinition>,    // 可用工具
  model: String,                 // 模型名称
  max_tokens: Option<u32>,       // 最大令牌数
  temperature: Option<f32>,      // 采样温度
}

↓ HTTP POST 到 /v1/chat/completions

ProviderResponse {
  content: Option<String>,       // 文本响应
  tool_calls: Vec<ToolCall>,     // 工具调用
  usage: Option<Usage>,          // 令牌使用统计
  model: String,                 // 实际使用的模型
  reasoning: Option<String>,     // 推理内容（如果有）
  finish_reason: Option<String>, // 完成原因
}
```

### 3. Tools 模块 (`tools/mod.rs`)

自注册工具系统，支持 JSON Schema 验证和异步处理器。

**关键类型：**
- `ToolRegistry` - 管理所有工具的中心注册表
- `Tool` - 工具定义和处理器
- `ToolBuilder` - 工具创建的流畅 API
- `ToolDefinition` - 面向模型的 JSON Schema

**工具注册模式：**
```rust
let tool = tool("get_weather")
    .description("获取城市的当前天气")
    .parameters(json!({
        "type": "object",
        "properties": {
            "city": {"type": "string"}
        },
        "required": ["city"]
    }))
    .handler(|args| async move {
        let city = args["city"].as_str().unwrap();
        Ok(json!({"temp": "22°C", "condition": "晴朗"}))
    })
    .toolset("weather")
    .build()?;

registry.register(tool);
```

**工具集管理：**
- 将相关工具分组到工具集
- 在运行时启用/禁用整个工具集
- 查询可用工具集及其工具

### 4. Session 模块 (`session/mod.rs`)

对话持久化，支持压缩场景的谱系追踪。

**关键类型：**
- `SessionStore` - 存储后端的 trait
- `MemorySessionStore` - 内存实现
- `SqliteSessionStore` - 生产后端（hermes_state 模式：sessions、messages、
  system_prompts、state_meta、async_delegations；FTS5 全文检索 + LIKE
  回退；父子谱系；面向网关 API 的会话行查询）
- `Session` - 带元数据的对话状态
- `SessionMetadata` - 用户 ID、平台、模型信息
- `export` - 可校验的 Markdown 会话导出（实现 hermes
  `session_export_md.py` / `session_export_html.py`）：frontmatter +
  消息标题 + 工具调用块 + SHA256 校验页脚，或带内联样式的独立 HTML
  文档 —— 两者均写入 `manifest.jsonl`
- `recap` - 即时的纯本地会话回顾（实现 hermes `session_recap.py`）：
  近期窗口轮次统计、工具使用排行、触碰过的文件、最近提问/回复预览；
  过滤 ANSI/控制字符

**会话生命周期：**
```
创建会话 → 添加消息 → 保存 → 加载 → 继续 → 保存
   │                                                    │
   └──────────────── 谱系追踪 ──────────────────────────┘
```

**特性：**
- 唯一会话 ID (UUID v4)
- 对话历史保存
- 跨会话全文搜索
- 元数据追踪（用户、平台、模型）
- 压缩会话的父子关系

### 5. Context 模块 (`context/mod.rs`)

智能提示构建和上下文窗口管理。

**关键类型：**
- `PromptBuilder` - 分层系统提示组装
- `ContextCompressor` - 上下文窗口优化

**提示构建策略：**
```
系统提示 = 身份 + 工具指导 + 技能 + 
          上下文文件 + 环境提示 + 
          记忆 + 时间戳
```

**分层组装：**
1. **稳定层**：身份、工具指导、技能（很少变化）
2. **上下文层**：上下文文件、环境提示（每个会话）
3. **易变层**：记忆、时间戳（每个请求）

**上下文压缩：**
- 估计令牌数量（粗略：每令牌 4 个字符）
- 检测上下文何时超出限制
- 未来基于模型的摘要的占位符

### 6. Error 模块 (`error.rs`)

完整的错误类型，支持自动转换。

**错误类别：**
- `AgentError::Provider` - API 失败、认证错误、速率限制
- `AgentError::Tool` - 工具执行失败
- `AgentError::ToolNotFound` - 注册表中缺少工具
- `AgentError::Session` - 持久化错误
- `AgentError::Context` - 上下文管理错误
- `AgentError::Config` - 配置错误
- `AgentError::IterationLimit` - 超出最大迭代次数
- `AgentError::Http` - 网络错误（来自 reqwest）
- `AgentError::Json` - 序列化错误
- `AgentError::Internal` - 通用内部错误

### 7. Browser 模块 (`browser/mod.rs`)

支撑 `browser_*` 工具的 Chrome DevTools Protocol 客户端。

**关键类型：**
- `BrowserEndpoint` / `resolve_endpoint` - 解析 `ULNCLAW_BROWSER_CDP`
  （`ws://...` 直连端点，或 `http://host:port` 发现）
- `CdpClient` - WebSocket JSON-RPC，请求/响应分流 + 前缀事件订阅；
  连接存活跟踪（`is_connected`）—— 读/写循环在套接字断开时翻转 closed
  标志并让所有在途调用快速失败，浏览器挂掉后不会让后续调用空等 30 秒超时
  （`Page.*`、`Runtime.*`……）
- `BrowserSession` - 单个已附加的页面目标：导航（等待 load 事件）、
  带编号元素引用的可访问性快照、经 `DOM.resolveNode` +
  `Runtime.callFunctionOn` 的点击/输入（CSS 选择器回退）、滚动/按键/
  图片列表/截图/执行 JS、JS 对话框跟踪
- `with_session` - 全部浏览器工具共用的全局共享会话管理器；缓存会话的
  CDP 连接已死时会被透明地丢弃并重建（对齐 hermes 会话健康检查）
- 浏览器监督器：`ULNCLAW_BROWSER_CDP=auto`（默认）启动托管的无头
  Chrome/Chromium（`find_browser_binary`、`launch_managed_browser`、
  `stop_managed_browser`），等待 DevTools 端口就绪后再连接
- SSRF/私有页面防护（`browser/guard.rs`，hermes 浏览器工具防护实现）：
  敏感查询参数与云元数据底线无条件拦截；非回环端点或容器化终端启用
  私网地址防护（`[security] allow_private_urls` 可退出）；重定向落地复检，
  落入受限地址即清空页面；console/eval 表达式预筛私网 URL 字面量；页面
  处于私网时原始 `browser_cdp` 仅放行白名单方法；浏览器输出进入模型前
  强制脱敏
- 本地 CDP 接入层（`browser/connect.rs`，hermes `browser_connect.py`
  实现）：Chromium 系候选二进制发现（macOS/Windows/Linux，含 WSL
  `/mnt/c` 安装位）、双栈回环探测（`discover_local_cdp_url` 先
  `127.0.0.1` 后 `[::1]`）、端口仲裁（`local_port_in_use` →
  `find_free_debug_port`）、带诊断的可视调试浏览器启动
  （`launch_chrome_debug`：逐候选尝试、stderr 尾部写入
  `<home>/chrome-debug/launch-stderr.log`、单实例吸收提示、
  `manual_chrome_debug_command` 兜底命令）
- 实时端点切换：REPL `/browser connect [url]` 与网关
  `POST /v1/browser/connect`（先经 `/json/version` 验证再生效）设置进程级
  覆盖，优先于 `ULNCLAW_BROWSER_CDP`；裸 `/browser connect` 走 hermes
  默认流程 —— 双栈探测 9222 端口、被占用时自动换端口、自动启动调试浏览
  器——并注入系统备注让模型知晓浏览器工具已接管；`/browser disconnect`
  与 `POST /v1/browser/disconnect` 清除；`GET /v1/browser/status` 报告
  来源/模式
- Camofox 后端（`browser/camofox.rs`）：设置 `CAMOFOX_URL` 且无 CDP 覆盖
  时，全部 12 个 browser 工具改走 Camofox REST API（Camoufox 反检测浏览
  器）而非 CDP——带身份覆盖/标签页收养的会话、导航后自动快照、Docker
  环回 URL 重写、VNC 发现，读取操作沿用同样的 SSRF 私有页面防护

**流程：**
```
browser_navigate → with_session → resolve_endpoint → CDP 连接
  → Page.enable/Runtime.enable/DOM.enable/Accessibility.enable
  → Page.navigate → 等待 Page.loadEventFired → page_info
```

### 8. Gateway 模块 (`gateway/mod.rs`)

将 Agent 暴露给 OpenAI 兼容客户端的 HTTP 层（axum）——
hermes `gateway/platforms/api_server.py` 核心的实现。

**关键类型：**
- `GatewayState` - agent + store + 模型标识 + 可选 bearer 密钥 +
  运行记录表 + 审批 router + 可选 cron 存储 / skills 目录
- `RunState` - 被跟踪的异步运行（`/v1/runs`）：状态、结果、停止标志、
  待审批载荷
- `ApprovalRouter` - run_id → 审批通道；`once`/`session`/`always`
  授权按作用域记忆；task-local `current_run_id()`
- `router()` / `serve()` - 路由表与监听器

**端点：**
- `GET /health`、`/health/detailed`、`/v1/health`（始终开放）
- `GET /v1/models`、`GET /api/model/options`、`GET /v1/capabilities`
- `POST /v1/chat/completions` - OpenAI 格式；经 `X-Ulnclaw-Session-Id`
  可选会话续接（从 SQLite 加载历史，并通过 `Agent::run_with_session`
  续用同一会话 id）；`stream: true` 返回 SSE `chat.completion.chunk`
  事件（令牌增量、`hermes.tool.progress` 事件、usage、`[DONE]`）
- `POST/GET/DELETE /v1/responses` - OpenAI Responses 格式，经
  `previous_response_id` 有状态续接
- `GET/POST /api/sessions`、`GET/DELETE /api/sessions/:id`、
  `PATCH /api/sessions/:id`（title/end_reason）、
  `POST /api/sessions/:id/fork`（分叉为子会话）、
  `POST /api/sessions/:id/model`（会话级模型锁，经 task-local provider
  模型覆盖在每一轮生效）、
  `GET /api/sessions/:id/messages`、`POST /api/sessions/:id/chat`、
  `POST /api/sessions/:id/chat/stream`（SSE）、
  `GET /api/sessions/:id/recap`（本地回合回顾，不调用 LLM）
- `GET/POST /api/jobs`、`GET/PATCH/DELETE /api/jobs/:id`、
  `POST /api/jobs/:id/pause|resume|run` — 经 HTTP 管理定时任务
  （与 CLI 共用 SQLite 任务存储）
- `GET /v1/skills`、`GET /v1/toolsets` — 已安装技能与解析后工具集的
  发现端点
- `POST /v1/runs`（202 + run_id）、`GET /v1/runs`、`GET /v1/runs/:id`、
  `GET /v1/runs/:id/events`（SSE 生命周期事件，含 `approval.request`）、
  `POST /v1/runs/:id/stop`、`POST /v1/runs/:id/approval`（解决待审批）
- `GET /v1/browser/status`、`POST /v1/browser/connect`、
  `POST /v1/browser/disconnect` —— 浏览器 CDP 端点实时控制：connect 先经
  `/json/version` 验证再应用进程级覆盖；status 报告后端/来源/模式
  （managed/endpoint/camofox）；disconnect 恢复默认模式
- `GET /metrics` —— Prometheus 计数器/仪表（请求数、令牌、工具调用、活跃
  会话/运行；ulnclaw 运维扩展）
- `GET /api/usage` —— 令牌核算：进程计数、全库历史总量、按会话明细
  （ulnclaw 运维扩展）
- `GET /v1/delegations`、`GET /v1/delegations/:id` —— 后台委派登记
  （ulnclaw 运维扩展）

**Profile 多路复用（`/p/<profile>`）：** 所有路由额外镜像到
`/p/<profile>/...`（对齐 hermes api_server）。`[gateway] multiplex_profiles
= true` 时每个镜像背后是独立的网关栈 —— agent 由 `[profiles.<name>]`
覆盖（模型/工具集）构建，home 按 profile 隔离（`<home>/profiles/<name>`：
state.db、approvals.json、cron 存储、skills 目录）—— 首次访问时惰性构建
并缓存（`ProfileHub`）；未知 profile 返回 404
`Unknown or unconfigured profile`。多路复用关闭时前缀被接受但忽略
（由默认 profile 服务），与 hermes `_resolve_request_profile` 一致。
镜像请求与原生路由经过同一 bearer 鉴权中间件。

**安全：** 除健康探测外所有路由经 bearer 令牌中间件（常量时间比较）；
密钥来自 `[gateway] key` 或 `ULNCLAW_GATEWAY_KEY`。run 总是拥有自己的
会话；确认级命令使 run 停泊在 `waiting_for_approval`，经 HTTP 解决
（`once`/`session`/`always`/`deny`）或在 `[approvals] timeout` 到期时
fail-closed 自动拒绝（并清理 run 状态）。`always` 授权持久化到
`approvals.json`。没有 run 上下文的请求（chat-completions）对确认级
命令自动拒绝。

### 9. Models.dev 目录 (`models_dev.rs`)

对齐 hermes `agent/models_dev.py` 的多 provider 模型清单。拉取社区
models.dev 注册表，三级缓存：

1. **内存** —— 1 小时 TTL；过期数据立即返回，由单个后台线程刷新
   （只要存在缓存，调用方绝不阻塞在网络）。
2. **磁盘** —— `$ULNCLAW_HOME/models_dev_cache.json`（原子写入），任意
   陈旧度可用；新鲜文件按文件年龄锚定内存 TTL。
3. **网络** —— 仅在无任何缓存时单飞前台获取；刷新失败触发进程级 5 分钟
   退避，并回退到陈旧的磁盘缓存。

注册表 URL 可用 `ULNCLAW_MODELS_DEV_URL` 覆盖（http(s) 或 `file://`
镜像），磁盘缓存路径可用 `ULNCLAW_MODELS_DEV_CACHE` 覆盖。查询面：
上下文窗口查询（大小写不敏感 + `:cloud`/`-cloud` 后缀回退）、能力元数据、
provider/模型信息、agentic 模型清单（噪声模式 + Google 隐藏清单）。
网关 `GET /api/model/options` 用目录增强（`?refresh=true` 强制刷新）；
CLI 提供 `ulnclaw models providers|list|info|refresh`。

### 支撑模块

- `config/` - `config.toml` + `.env` + profiles（`UlncLawConfig`、
  `GatewayConfig`，`ULNCLAW_HOME` 等环境变量覆盖）
- `toolsets.rs` - hermes 兼容工具分组（33 个工具集、组合、启用/禁用策略）
- `cron/` - 计划解析（`30m`、`every 2h`、cron 表达式、ISO 一次性）+
  任务存储 + 轮询调度器
- `skills/` - SKILL.md 发现与提示注入
- `mcp/` - MCP stdio 客户端；服务器工具注册为 `mcp__<server>__<tool>`
- `environments.rs` - terminal 后端：local / docker（自动创建容器）/
  ssh；命令包装 + shell 引号
- `checkpoint.rs` - 透明的 git 快照（共享 bare 存储、按项目
  ref/index、编辑前钩子、restore/diff/prune）
- `logs.rs` - 滚动文件日志（`<home>/logs/agent.log` INFO+、
  `errors.log` WARNING+、`gateway.log` 网关目标；hermes `_LOG_FORMAT`
  行格式）+ `ulnclaw logs` 查看器（tail/follow/level/session/since/
  component 过滤）
- `backup.rs` - 备份/恢复：`<home>` 完整 zip（排除规则 + WAL 安全
  SQLite 快照）、导入覆盖、快速状态快照（`state-snapshots/`）、
  cron 安全网；接入 `ulnclaw update`（更新前快照）

## 数据流

### 完整对话流程

```
1. 用户输入
   ↓
2. Agent::run()
   ├─ 加载对话历史（可选）
   ├─ 添加系统提示
   ├─ 添加用户消息
   ↓
3. 对话循环（迭代 1..N）
   ├─ 构建 API 请求
   │   ├─ 序列化消息
   │   ├─ 收集工具定义
   │   └─ 设置模型参数
   ├─ 调用提供商
   │   ├─ HTTP POST 到 API 端点
   │   ├─ 解析响应
   │   └─ 提取内容/工具调用
   ├─ 检查工具调用
   │   ├─ 是：执行工具
   │   │   ├─ 分发到处理器
   │   │   ├─ 捕获结果
   │   │   ├─ 添加到消息
   │   │   └─ 继续循环
   │   └─ 否：最终响应
   └─ 检查迭代限制
   ↓
4. 返回 RunResult
   ├─ 最终内容
   ├─ 完整对话历史
   ├─ 令牌使用统计
   ├─ 迭代次数
   └─ 工具调用记录
```

## 设计原则

### 1. 提示稳定性
系统提示在对话过程中不会改变。这实现了：
- 高效缓存（Anthropic 前缀缓存）
- 一致的代理行为
- 可预测的令牌使用

### 2. 可观察执行
每个操作都通过回调可见：
- `on_tool_start` - 工具执行前
- `on_tool_complete` - 工具执行后
- `on_stream_delta` - 流式文本块
- `on_thinking` - 模型思考阶段
- `on_step` - 每次迭代完成

### 3. 可中断
所有异步操作都可以取消：
- 通过 `tokio::select!` 取消 HTTP 请求
- 通过取消令牌取消工具执行
- 干净关闭，无资源泄漏

### 4. 平台无关核心
一个 `Agent` 服务多种用例：
- CLI 应用程序
- Web 服务
- API 服务器
- 嵌入式系统

### 5. 松耦合
可选子系统使用注册表模式：
- 工具自注册，无硬依赖
- 提供商实现 trait，运行时可交换
- 会话使用 trait 对象，后端无关

## 性能考虑

### 内存管理
- **Arc 用于提供商**：跨异步任务的共享所有权
- **Mutex 用于注册表**：线程安全的工具访问
- **高效序列化**：serde_json，尽可能零拷贝

### 并发
- **Async/Await**：全程非阻塞 I/O
- **并发工具执行**：可选的并行工具调用
- **Tokio 运行时**：可配置的线程池

### 网络优化
- **连接池**：reqwest 客户端重用
- **超时配置**：默认 120 秒，可配置
- **错误重试**：自动重试与指数退避（计划中）

## 扩展点

### 添加新提供商
1. 实现 `Provider` trait
2. 添加请求/响应类型
3. 在 `ProviderConfig::build()` 中注册

### 添加工具
1. 使用 `ToolBuilder` 流畅 API
2. 定义 JSON Schema
3. 实现异步处理器
4. 用 `ToolRegistry` 注册

### 添加新会话后端
1. 实现 `SessionStore` trait
2. 添加持久化逻辑
3. 在 `Agent` 构造函数中交换

### 自定义上下文管理
1. 用新层扩展 `PromptBuilder`
2. 实现压缩算法
3. 钩入代理循环

## 安全考虑

### API 密钥管理
- 密钥存储在 `ProviderConfig` 中，不硬编码
- 支持环境变量
- 永远不会在错误中记录或暴露

### 工具执行沙箱
- 工具在异步上下文中运行
- 默认没有直接文件系统访问
- 处理器负责自己的安全性

### 输入验证
- 工具参数的 JSON Schema 验证
- 无效输入的错误类型
- 格式错误数据的优雅降级

## 未来增强

### 计划特性
- **消息平台**：hermes 的 Telegram/WhatsApp/QQ 适配器
- **更多运行环境**：modal/daytona/vercel 终端后端
  （local/docker/ssh 已实现）
- **Anthropic 原生**：直接 Claude API 支持
- **重试逻辑**：带退避的自动重试
- **指标**：Prometheus/OpenTelemetry 集成

### 架构演进
- **插件系统**：动态工具/提供商加载
- **分布式代理**：多节点代理协调

## 参考资料

- [Hermes Agent 架构](https://github.com/NousResearch/hermes-agent)
- [OpenAI API 文档](https://platform.openai.com/docs)
- [Rust 异步编程指南](https://rust-lang.github.io/async-book/)
- [Tokio 文档](https://tokio.rs/tokio/tutorial)

## 下一步

- 阅读 [工具系统指南](tools.md) 了解高级工具模式
- 阅读 [提供商系统指南](providers.md) 了解实现自定义提供商
- 查看 [API 参考](api-reference.md) 了解完整类型文档
- 参见 [集成指南](integration.md) 了解在项目中使用 ulnclaw

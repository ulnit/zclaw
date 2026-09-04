# ulnclaw 🦞

<p align="center">
  <img src="https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-green?style=for-the-badge" alt="License: MIT OR Apache-2.0">
  <img src="https://img.shields.io/badge/Language-Rust-DEA584?style=for-the-badge&logo=rust&logoColor=white" alt="Rust">
  <img src="https://img.shields.io/badge/Parity-hermes--agent%20v2026.8.3-blueviolet?style=for-the-badge" alt="hermes-agent v2026.8.3 parity">
  <a href="README.md"><img src="https://img.shields.io/badge/Lang-English-blue?style=for-the-badge" alt="English"></a>
</p>

**Rust 编写的高性能 AI Agent 引擎 —— [Nous Research](https://nousresearch.com) [hermes-agent v2026.8.3](https://github.com/NousResearch/hermes-agent/tree/v2026.8.3) 的 Rust 移植。**

ulnclaw 用 Rust 重新实现了 Hermes Agent 引擎：相同的工具面（50+ 内置工具）、
相同的 SQLite 会话 / 记忆 / 技能 / 定时任务存储布局、相同的工具集组合方式 ——
原生性能，单一静态 musl 二进制。任意模型随意用（OpenAI 兼容端点、Ollama、
Anthropic、Dashscope 等），`ulnclaw model` 一键切换，无需改代码。可以跑在
笔记本、5 美元 VPS 或 Docker 沙箱里 —— 通过终端、桌面应用或 26 个消息平台
与它对话。

与 hermes-agent v2026.8.3 的核心对齐已完成 —— 逐项对标见
[对标矩阵](docs/zh/hermes-parity.md)（含 HTTP 路由对标附录）。

## 核心特性

<table>
<tr><td><b>🔧 50+ 内置工具</b></td><td>terminal/process、文件读/写/补丁/搜索、web 搜索/抽取、记忆、todo、委派、<code>execute_code</code>、视觉、图像/视频生成、浏览器自动化、TTS、kanban、工具搜索 —— 按 hermes 兼容工具集分组（<code>coding</code>、<code>web</code>、<code>file</code>、<code>safe</code>、<code>debugging</code>……），支持启用/禁用策略。</td></tr>
<tr><td><b>💬 真正的终端界面</b></td><td>交互式聊天 + 斜杠命令（<code>/new /search /memory /skills /sessions /rollback /diff /recap /goal /kanban …</code>），工具输出流式渲染，会话续接（<code>--continue</code> / <code>--resume</code>）。</td></tr>
<tr><td><b>📡 随处可达</b></td><td>单网关进程接入 26 个消息平台 —— Telegram、Discord、Slack、Signal、微信、QQ、元宝、邮件、Mattermost、Matrix、钉钉、企微、飞书、Home Assistant、SMS、WhatsApp、IRC、ntfy、SimpleX、Teams、LINE、Google Chat、Buzz、Photon (iMessage)、Raft、A2A —— 支持跨频道 <code>send_message</code> 与语音转写。</td></tr>
<tr><td><b>🧠 记忆与技能</b></td><td>持久记忆 + 提示词注入，FTS5 会话全文检索，技能安全扫描，技能同步，学习时间线（<code>ulnclaw journey</code>）。</td></tr>
<tr><td><b>⏰ 定时自动化</b></td><td>内置 cron 调度器，可投递到任意平台；可排期的技能 blueprints；自动化建议。</td></tr>
<tr><td><b>🤝 委派与并行</b></td><td>后台子代理委派（持久化异步注册表）、支持 swarm 模式的 kanban 任务引擎、<code>execute_code</code> 流水线、Mixture-of-Agents 扇出（<code>moa</code>）。</td></tr>
<tr><td><b>🌐 浏览器与电脑操作</b></td><td>12 个经 CDP 的 <code>browser_*</code> 工具 —— 托管无头 Chrome/Chromium、Camofox 反检测、云会话（Browserbase / Browser Use / Firecrawl）—— 另有经 cua-driver 守护进程的 <code>computer_use</code>，与 hermes 相同的审批门控。</td></tr>
<tr><td><b>🔌 可扩展</b></td><td>MCP 客户端（stdio + Streamable HTTP/SSE、OAuth 2.1 + PKCE、懒加载）、插件 + shell 钩子（触发 hermes 全部 13 个钩子事件）、ACP 适配器、OpenAI 兼容 HTTP 网关、MCP 频道桥。</td></tr>
<tr><td><b>🛡️ 默认安全</b></td><td>审批系统（网关审批失败关闭）、Secrets 保险库（Bitwarden SM / 1Password / command）、SSRF 防护、密钥脱敏、沙箱凭据清洗、Docker 沙箱出站防火墙、透明 checkpoint。</td></tr>
<tr><td><b>🗜️ 上下文管理</b></td><td>预算触发的回合中压缩、三层工具结果持久化、<code>/context</code> 窗口构成视图。</td></tr>
<tr><td><b>🖥️ 桌面 GUI</b></td><td><b>ulnclaw desktop</b> —— hermes Electron 桌面端的忠实移植：十六个视图、命令面板、xterm.js 终端面板、经 WebSocket/SSE 的实时回合流式、整壳静默自动更新。</td></tr>
<tr><td><b>📦 随处运行</b></td><td>单一静态 musl 二进制；终端后端：local、Docker、SSH。</td></tr>
</table>

## 快速开始

### 源码构建

```bash
cargo build --release --target x86_64-unknown-linux-musl   # 静态二进制
# 产物：target/x86_64-unknown-linux-musl/release/ulnclaw
```

### 首次运行

```bash
ulnclaw setup        # 交互式安装向导（provider、终端、消息平台、工具）
ulnclaw model        # 交互式切换 provider/模型
ulnclaw init         # 生成默认配置 ~/.ulnclaw/config.toml

ulnclaw run "Summarize the README.md file"   # 一次性运行
ulnclaw chat                                 # 交互式聊天
ulnclaw chat --continue                      # 续接最近会话
ulnclaw chat --resume <session-id>           # 按 id 或唯一前缀恢复会话

ulnclaw gui          # 启动桌面应用（别名：desktop）
```

## CLI 速查

所有命令均支持 `--help`；`ulnclaw completion bash|zsh|fish|elvish|powershell`
生成 shell 补全。

```bash
# 会话与历史
ulnclaw sessions list|search|browse        # browse = 交互式会话选择器 + 全文查看
ulnclaw sessions export <id> --format md   # 另支持：import / delete / rename / optimize
ulnclaw sessions recover|repair            # 受损 state.db 的离线恢复
ulnclaw insights                           # 会话用量分析

# 技能与自动化
ulnclaw skills list|blueprints|scan        # scan = 信任技能前的安全扫描
ulnclaw cron list                          # create/show/pause/resume/run + blueprints
ulnclaw suggestions                        # 自动化建议（accept/dismiss）
ulnclaw journey                            # 学习时间线

# 工具与模型
ulnclaw tools                              # 工具集 + 已启用工具（enable/disable X）
ulnclaw models providers                   # models.dev 目录（list/info/refresh）
ulnclaw moa list|run                       # Mixture of Agents 预设
ulnclaw fallback add|remove                # provider:model 失败切换链

# 网关与平台
ulnclaw gateway --host 127.0.0.1 --port 8642   # OpenAI 兼容 API + 消息平台
ulnclaw dashboard status                   # 仪表盘服务（run/stop）
ulnclaw pairing list                       # 私信配对码（approve/revoke）
ulnclaw weixin login                       # 微信扫码登录
ulnclaw spotify-auth login                 # spotify_* 工具的 PKCE OAuth

# 项目与看板
ulnclaw project list|create|scan           # 项目注册表 + git 仓库发现
ulnclaw kanban list                        # 任务引擎：create/claim/done/swarm/...

# 安全与密钥
ulnclaw approvals                          # manual | smart | off
ulnclaw secrets status|sync                # 外部保险库（bitwarden/onepassword setup）
ulnclaw security audit                     # 固定 MCP 包的 OSV.dev 审计
ulnclaw computer-use status                # 经 cua-driver 的桌面控制（doctor/install）
ulnclaw plugins list                       # 插件 + shell 钩子（hooks doctor）

# 运维
ulnclaw doctor                             # 诊断配置/依赖（--fix、--online）
ulnclaw status                             # 所有组件状态（--deep）
ulnclaw logs                               # 日志跟踪/过滤（-f、--level、--component）
ulnclaw update --check                     # stash -> ff pull -> rebuild
ulnclaw backup                             # home 目录 zip 备份（list/restore/prune）
ulnclaw config get|set|unset               # env 风格键写入 .env
ulnclaw dump                               # 可复制粘贴的支持信息摘要
```

## 桌面应用

每个 `v*` tag 都会在 Releases 页面产出安装包（产品名 **ulnclaw desktop**），
由 `desktop-electron/` 的 Electron 外壳构建 —— hermes 桌面端（v2026.8.3）的
忠实移植：React 19 渲染层，十六个视图（聊天、会话、任务、用量、模型、技能、
看板、项目、运行、Webhooks、插件、配对、配置档案、配置、诊断、设置）、
命令面板、xterm.js 终端面板、带 git 审查的文件树，以及经 JSON-RPC WebSocket
+ HTTP/SSE 的实时回合流式。

- **Windows** — `ulnclaw-<ver>-win-x64.exe`（NSIS 按用户安装）。完全自包含：
  静态链接的 `ulnclaw` 网关二进制内置于包内；外壳在 `127.0.0.1:8642` 拉起
  `ulnclaw gateway` 并探测 `/health`，启动失败时给出诊断信息。
- **macOS** — `ulnclaw-<ver>-mac-arm64.dmg`（Apple Silicon）/
  `ulnclaw-<ver>-mac-x64.dmg`（Intel）。ad-hoc 签名：首次启动请右键 › 打开；
  若 Gatekeeper 提示"已损坏"，执行
  `xattr -cr "/Applications/ulnclaw desktop.app"`。
- **Linux** — `ulnclaw-<ver>-linux-x86_64.AppImage`（另可构建 deb/rpm），
  每个 release 附 `SHA256SUMS.txt`。
- **静默自动更新（v0.7.1+）** — electron-updater 走 GitHub release 渠道，
  后台一次性替换整个外壳（应用 + 内置网关）；日志见
  `~/.ulnclaw/logs/desktop.log` 的 `[shell-update]`。

首次启动无需 API key：网关支持无 key 启动，首次引导与 Models 视图会指引你
添加 provider 密钥。配置位于 `~/.ulnclaw/config.toml`，与下面的 CLI 流程
完全一致。`ulnclaw gui` 启动打包应用（`--dev` 运行 `desktop-electron/`
的未打包应用）。详见 [desktop-electron/README.md](desktop-electron/README.md)。

## HTTP 网关

一个进程同时服务 OpenAI 兼容 API、消息平台、桌面应用与任意浏览器仪表盘：

```bash
ulnclaw gateway --host 127.0.0.1 --port 8642

curl -H "Authorization: Bearer $ULNCLAW_GATEWAY_KEY" \
     -H "Content-Type: application/json" \
     -d '{"messages":[{"role":"user","content":"你好！"}]}' \
     http://127.0.0.1:8642/v1/chat/completions
```

- `/v1/chat/completions` + `/v1/responses` SSE 流式；异步 `/v1/runs` 带审批
  解决；会话 API；100+ `/api` 管理端点（内置本地应用 CORS）。
- 消息平台随网关运行（`[messaging.telegram|discord|slack|signal|weixin|qq|
  yuanbao|email|...]`，另有 webhook 平台：whatsapp_cloud/msgraph/webhook/
  bluebubbles/feishu/sms/teams/line/google_chat/raft/a2a）。
- 可选 profile 多路复用（`multiplex_profiles`）：`/p/<profile>/...` 镜像 +
  失败关闭的密钥作用域 —— 见
  [多路复用网关设计文档](docs/design/multiplexing-gateway.md)。

## 配置

`ulnclaw init` 生成默认 `~/.ulnclaw/config.toml`：

```toml
# timezone = "Asia/Shanghai"        # 提示词时间戳的 IANA 时区

[model]
provider = "ollama"                 # 或 "openai"、"anthropic"、"dashscope"……
model = "qwen3:32b"
base_url = "http://localhost:11434/v1"
# max_retries = 2                   # 429/5xx/网络错误退避重试
# fallbacks = ["openai:gpt-5.2-mini", "ollama:qwen3:32b"]   # 失败切换链

# 辅助模型路由 —— 次要调用走另一个模型
# [auxiliary.compression]           # 上下文压缩摘要
# provider = "openai"
# model = "gpt-5.2-mini"
# [auxiliary.vision]                # vision_analyze / browser_vision
# [auxiliary.title_generation]      # 首轮交互后的会话标题

# MCP 服务器 —— stdio、远程 HTTP/SSE 或 OAuth 保护
[[mcp.servers]]
name = "filesystem"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/home/me"]
# lazy = false                      # true = 从 schema 缓存注册，首次调用时才拉起

# [[mcp.servers]]
# name = "remote"
# url = "https://mcp.example.com/mcp"   # 旧版 SSE 协议用 transport = "sse"
# auth = "oauth"                        # 首次使用走 OAuth 2.1 + PKCE，令牌自动缓存

# [gateway]
# host = "127.0.0.1"
# port = 8642
# key = "sk-..."                    # 环境变量 ULNCLAW_GATEWAY_KEY 优先

# [terminal]
# backend = "docker"                # "local"（默认）| "docker" | "ssh"

# [approvals]
# mode = "manual"                   # manual | smart（辅助 LLM 守卫）| off

# [security]
# allow_private_urls = false        # web 工具默认不访问内网/私有 IP

# [checkpoints]
# enabled = true                    # write_file/patch 前的透明快照
```

浏览器自动化可通过 `ULNCLAW_BROWSER_CDP`（已开启远程调试的浏览器）、
`CAMOFOX_URL`（反检测浏览器服务）或 `[browser] cloud_provider`
（browserbase / browser-use / firecrawl）接入。完整参考见
[docs/zh/tools.md](docs/zh/tools.md) 与 [docs/zh/providers.md](docs/zh/providers.md)。

## 库用法快速开始

```rust
use ulnclaw::prelude::*;
use ulnclaw::{register_builtin_tools, ToolRegistry, SqliteSessionStore};

#[tokio::main]
async fn main() -> Result<()> {
    let provider = OpenAiProvider::builder()
        .endpoint("http://localhost:11434/v1")
        .model("qwen3:32b")
        .build()?;

    let mut tools = ToolRegistry::new();
    register_builtin_tools(&mut tools);          // 全部 50+ hermes 风格工具

    let agent = Agent::new(Arc::new(provider), tools)
        .with_config(AgentConfig { approval: false, ..Default::default() })
        .with_store(Arc::new(SqliteSessionStore::open_default()?));

    println!("{}", agent.chat("列出当前目录的文件").await?);
    Ok(())
}
```

## 文档

| 主题 | 说明 |
|---|---|
| [Hermes 对标矩阵](docs/zh/hermes-parity.md) | 与 hermes-agent v2026.8.3 的工具/功能逐项对应，含 HTTP 路由对标 |
| [架构](docs/zh/architecture.md) | 项目结构、agent 循环、关键模块 |
| [工具与工具集](docs/zh/tools.md) | 50+ 工具、工具集组合、终端后端 |
| [Provider 系统](docs/zh/providers.md) | 模型 provider、凭据、失败切换 |
| [集成指南](docs/zh/integration.md) | 网关、嵌入、消息平台 |
| [API 参考](docs/zh/api-reference.md) | HTTP 网关端点 |
| [开发指南](docs/zh/development.md) | 开发环境、测试 |
| 设计文档 | [多路复用网关](docs/design/multiplexing-gateway.md) · [浏览器 CDP 客户端](docs/design/browser-cdp.md) · [桌面 Electron](docs/design/desktop-electron.md) |
| [桌面应用](desktop-electron/README.md) | Electron 外壳（`ulnclaw desktop`） |

## 从 hermes-agent 迁移

ulnclaw 以 hermes-agent v2026.8.3 的功能对齐为目标，并复用其存储布局
（`~/.ulnclaw/` 对应 `~/.hermes/`）：会话、记忆、技能、定时任务遵循相同的
SQLite 结构。逐项映射见[对标矩阵](docs/zh/hermes-parity.md)；
`ulnclaw import-agent` 可导入 Claude Code / Codex 配置。

## 构建与测试

```bash
cargo test                     # 990 个测试
cargo build --release --target x86_64-unknown-linux-musl   # 静态二进制
```

## 参与贡献

欢迎贡献！提交 PR 前请运行 `cargo test`；新增或变更 hermes 等价行为时，
请同步更新[对标矩阵](docs/zh/hermes-parity.md)。

## 许可证

MIT OR Apache-2.0

ulnclaw 是 [Nous Research](https://nousresearch.com)
[hermes-agent](https://github.com/NousResearch/hermes-agent) 的 Rust 移植，
原始设计与文档见该项目。

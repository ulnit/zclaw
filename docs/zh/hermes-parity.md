# Hermes Agent 对标矩阵 (v2026.8.3)

**目录** — [工具对标](#工具对标) · [功能对标](#功能对标) · [存储布局](#存储布局) · [已知差异](#已知差异) · [HTTP 路由对标附录](#http-路由对标附录) · [完成状态](#完成状态)

本文档跟踪 ulnclaw 与
[hermes-agent v2026.8.3](https://github.com/NousResearch/hermes-agent/tree/v2026.8.3)
的对应关系。ulnclaw 是 hermes agent 引擎的 Rust 重实现：相同的工具面、
相同的存储布局、相同的配置语义 —— 原生性能，单一静态二进制。

<a id="工具对标"></a>

## 工具对标

| hermes 工具 | ulnclaw | 说明 |
|---|---|---|
| `terminal`, `process` | ✅ 完整 | 前台/后台执行、超时、工作目录跟踪、后台会话管理（list/log/wait/kill）、失败智能（良性退出码语义 + 输出模式恢复提示） |
| 工具输出上限（`tool_output_limits.py`） | ✅ | `[tool_output] max_bytes/max_lines/max_line_length` 可调 terminal 输出头+尾上限（默认 10 万字符）、read_file 分页上限（2000 行）与每行截断（2000 字符，`... [truncated]` 标记）；非正值回退默认；未配置时行为不变 |
| 终端失败提示（`terminal_hints.py`、`_interpret_exit_code`） | ✅ | 良性非零退出码给出 `exit_code_meaning`（grep/rg/diff/find/test/curl/git 语义表，取管道/链的最后一段，跳过 `VAR=val` 前缀）；失败命令至多附加一条 `hint`，按生产频率排序的输出模式扫描（gh JSON 字段漂移、合并冲突、命令未找到——python/pip 特判、ModuleNotFoundError/ImportError、"already exists"、gh 限流、权限拒绝）+ 退出码 124/126/137 专属提示；扫描窗口限 4000 字符，首个匹配生效 |
| 密钥脱敏（`agent/redact.py`） | ✅ 核心 | terminal 输出（前台 + process log/wait）与 read_file 内容均经脱敏器：约 55 种厂商前缀令牌（sk-/ghp_/glpa … [详情](#密钥脱敏-agent-redact-py) |
| ANSI 剥离（`ansi_strip.py`） | ✅ | 完整 ECMA-48 覆盖（CSI 含私有模式/冒号参数/中间字节、OSC 的 BEL/ST 终止、DCS/SOS/PM/APC、nF 与单字节转义、8-bit C1），terminal 与 execute_code 输出在送达模型前剥离；`sanitize_display_text` 另去除裸控制字符并归一化 CR，供终端安全回显 |
| 二进制扩展守卫（`binary_extensions.py`） | ✅ | `read_file` 以纯字符串检查（无 I/O）拒绝约 80 种二进制扩展，并提示改用 vision_analyze/terminal；`.pdf` 保持可读（文本类） |
| `read_file`, `write_file`, `patch`, `search_files` | ✅ 完整 | 带行号读取与 `next_offset` 分页、模糊替换（容忍空白/缩进差异）、V4A 多文件补丁、unified diff、ripgrep 风格搜索 |
| `web_search`, `web_extract` | ✅ 完整 | 可插拔后端：Tavily / Brave / SearXNG / 内置 DuckDuckGo；HTML→文本抽取 |
| URL 安全 / SSRF 防护（`tools/url_safety.py`） | ✅ 核心 | `url_safety` 模块：拦截对私有/内网地址的 web 拉取（环回、RFC1918、链路本地、CGNAT 100.64/10、基准 198.18/15、ULA、IPv4 映射 IPv6）；云元数据端点（169.254.169.254、metadata.google.internal、ECS 任务元数据…）**永远**拦截；接入 `web_extract`（逐 URL 检查 + 经 reqwest 重定向策略逐跳重验 + 拒绝嵌入凭证的 URL：令牌前缀与敏感查询参数拦截），可通过 `[security] allow_private_urls` / `ULNCLAW_ALLOW_PRIVATE_URLS` 放开；DNS 失败默认拦截，配置代理时委托代理解析（hermes 语义） |
| `memory` | ✅ 完整 | `MEMORY.md` + `USER.md`，原子批量 `operations`，字符上限（2200/1375），每轮注入系统提示词 |
| `todo` | ✅ 完整 | 会话任务列表、merge 模式、强制单一 `in_progress` |
| `session_search` | ✅ 完整 | SQLite FTS5 检索 + 会话内滚动，会话血缘 |
| `clarify` | ✅ 完整 | 单选/多选/开放式提问（经前端回调） |
| `skills_list`, `skill_view`, `skill_manage` | ✅ 完整 | SKILL.md frontmatter、关联文件（references/templates/scripts）、路径穿越防护 |
| 蓝图 Blueprints（`tools/blueprints.py`） | ✅ 核心 | 在 frontmatter 声明 `metadata.hermes.blueprint.schedule` 的技能可排程：`skills blueprints` … [详情](#蓝图-blueprints-tools-blueprints-py) |
| 技能守卫（`tools/skills_guard.py`） | ✅ 核心 | `skills scan <name> [--source <repo>] [--json] [--force]`：静态扫描器 … [详情](#技能守卫-tools-skills-guard-py) |
| `delegate_task` | ✅ 完整 | 并行子代理、深度限制、隔离上下文、子会话；hermes v2026.8.3 后台语义：顶层委派即发即忘（`mode: background`、delegatio … [详情](#delegate-task) |
| `execute_code` | ✅ 完整 | python3 子进程沙箱，120 秒上限 |
| `cronjob` | ✅ 完整 | create/list/update/pause/resume/remove/run；`30m` / `every 2h` / `0 9 * * *` / IS … [详情](#cronjob) |
| `tool_search` | ✅ 完整 | 按关键词搜索已注册工具目录 |
| `vision_analyze` | ✅ 完整 | 经聊天 provider 的 `analyze_image` 路由，`[auxiliary.vision]` provider/模型覆盖 |
| `image_generate` | ✅ 完整 | OpenAI images API，PNG 存于 `<home>/images` |
| `text_to_speech` | ✅ 完整 | OpenAI TTS 或自定义 `ULNCLAW_TTS_ENDPOINT` |
| `ha_*`（4 个 Home Assistant 工具） | ✅ 完整 | Home Assistant REST API，依赖 `HASS_URL` + `HASS_TOKEN` |
| `kanban_*`（12 个工具） | ✅ 完整 | 本地 SQLite 协作看板，与 `ulnclaw kanban` CLI、网关 `/api/kanban/*` 端点共用同一 `KanbanStore` 引擎 … [详情](#kanban-12-个工具) |
| `browser_*`（12 个工具） | ✅ 完整 | CDP WebSocket 客户端（`browser` 模块）：端点发现、页面会话、带元素引用的可访问性快照、点击/输入/滚动/按键/截图/执行 JS/对话框 … [详情](#browser-12-个工具) |
| `close_terminal`、`read_terminal`、`focus_pane`、`open_preview` | ✅ 核心 | 桌面 GUI 工具（hermes `close_terminal_tool.py` / `read_terminal_tool.py` / … [详情](#close-terminal-read-terminal-focus-pane-open-preview) |
| `computer_use` | ✅ 核心 | cua-driver MCP 后端（`src/computer_use.rs`）—— 完整 hermes 工具 schema + 审批语义，见下方 Computer Use 行；驱动可达即注册（`ulnclaw computer-use doctor`） |
| `discord`, `discord_admin` | ✅ 核心 | P245 完整移植 hermes `tools/discord_tool.py`（`src/discord_tool.rs`）：15 个 REST API 动作 … [详情](#discord-discord-admin) |
| `feishu_doc_read`, `feishu_drive_*` | ✅ 核心 | P246 完整移植 hermes `tools/feishu_doc_tool.py` + `tools/feishu_drive_tool.py`（ … [详情](#feishu-doc-read-feishu-drive) |
| `spotify_*`（7 工具） | ✅ 核心 | P247 完整移植 hermes `plugins/spotify`（`src/spotify_tool.rs` + `src/spotify_auth.rs` … [详情](#spotify-7-工具) |
| `yb_*`（5 个元宝工具） | ✅ 核心 | P248 完整移植 hermes `tools/yuanbao_tools.py`（`src/yuanbao_tool.rs`）： … [详情](#yb-5-个元宝工具) |
| `send_message` | ✅ 核心 | P259 完整移植 hermes `tools/send_message_tool.py` + `gateway/channel_directory.py`（ … [详情](#send-message) |
| `x_search` | 🟡 门控 | hermes `x_search_tool.py` 完整移植：xAI Responses-API `x_search` 服务端工具，支持账号白/黑名单（最多 1 … [详情](#x-search) |
| `video_analyze` | ✅ 核心 | hermes `vision_tools.video_analyze_tool` 完整移植：本地文件 / `file://` / HTTP(S) 来源（远程下载经 SSRF 防护，缓存于 `cache/video/temp_video_files/` 并自动清理）、扩展名→mime 映射（mp4/webm/mov/avi/mkv/mpeg/mpg）、20 MB 警告 + 50 MB base64 硬上限、内联 `video_url` data-URL 载荷、`[auxiliary.vision]` 路由失败回落主 provider、空响应重试一次；可选 `video` 工具集（hermes 对齐）——需要支持视频的 provider |
| `video_generate`、`bfl_flux3_*` | ✅ 核心 | `video_gen.rs` provider 注册表（hermes 插件设计：单一可用后端自动选中、配置名 fail-closed … [详情](#video-generate-bfl-flux3) |
| `project_list`、`project_create`、`project_switch` | ✅ 核心 | hermes `tools/project_tools.py` + `hermes_cli/projects_db.py` 完整移植：每 profile … [详情](#project-list-project-create-project-switch) |
| 技能使用遥测 + 学习图谱（`skill_usage`、`learning_graph`、`learning_mutations`） | ✅ 核心 | hermes `tools/skill_usage.py` + `agent/learning_graph.py` + … [详情](#技能使用遥测-学习图谱-skill-usage-learning-graph-learning-mutations) |
| 学习时间线 / `journey` CLI（`learning_graph_render`、`journey`） | ✅ 核心 | hermes `agent/learning_graph_render.py` + `hermes_cli/journey.py` 移植： … [详情](#学习时间线-journey-cli-learning-graph-render-journey) |
| 技能策展 CLI（`curator`） | ✅ 核心 | hermes `hermes_cli/curator.py` 本地半区（LLM 整合运行留在桌面侧）：`curator.rs` —— 空闲天数计算（活动优先、c … [详情](#技能策展-cli-curator) |
| 持久化目标 / Ralph 循环（`goals`） | ✅ 核心 | hermes `hermes_cli/goals.py` 移植：`goals.rs` —— `GoalContract`（outcome/verificatio … [详情](#持久化目标-ralph-循环-goals) |
| 网关 profile 多路复用（`/p/<profile>`）+ CDP 会话存活 | ✅ 核心 | hermes api_server profile 前缀中间件移植：所有网关路由镜像到 `/p/<profile>/...` … [详情](#网关-profile-多路复用-p-profile-cdp-会话存活) |
| 启动提示（`tips.py`） | ✅ 核心 | `tips.rs`：面向 ulnclaw 自身功能面重写的特性发现一句话语料（斜杠命令、目标、CLI 子命令、配置项、工具、网关、隐藏技巧）+ 无依赖 xorshift64* `get_random_tip`；聊天 REPL 在启动与 `/new` 时打印 `✦ Tip:` 行（对齐 hermes 欢迎/新会话提示） |
| REPL 显示与输入体验（`hermes_cli/focus_view.py`、`prompt_stash.py`、`clipboard.py`） | ✅ 核心 | `src/focus_view.rs`、`src/prompt_stash.rs`、`src/clipboard.rs` —— 三个 hermes CLI 体验 … [详情](#repl-显示与输入体验-hermes-cli-focus-view-py-prompt-stash-py-clipboard-py) |
| 会话裁剪/归档/统计（`session_filters.py`） | ✅ 核心 | `session/filters.rs` —— 时长解析（`5h`/`30m`/`2d`/`1w`，裸数字 = 天）、时间点解析（时长 = 距今多久前；ISO … [详情](#会话裁剪-归档-统计-session-filters-py) |
| 皮肤/主题引擎（`skin_engine.py`） | ✅ 核心 | `skin.rs`：hermes 全部 9 个内置皮肤以数据形式内置（default、ares、mono、slate、daylight、warm-lightmo … [详情](#皮肤-主题引擎-skin-engine-py) |
| 欢迎横幅与更新检查（`banner.py`） | ✅ 核心 | `banner.rs`：皮肤着色的欢迎面板（盒线绘制）—— 盲文爪痕主视觉 + 模型行（缩短名称、去 `.gguf`、28 字符上限、models.dev 上下 … [详情](#欢迎横幅与更新检查-banner-py) |
| 浏览器 CDP 接入层（`browser_connect.py`） | ✅ 核心 | `browser/connect.rs`：Chromium 系候选发现（macOS/Windows/Linux，含 WSL `/mnt/c` 安装路径），覆盖 … [详情](#浏览器-cdp-接入层-browser-connect-py) |
| Doctor（`doctor.py`） | ✅ 核心 | `doctor.rs` + `ulnclaw doctor` CLI：hermes 盒线横幅报告，✓/⚠/✗/ℹ 分级检查按段组织 —— Version & U … [详情](#doctor-doctor-py) |
| 会话洞察（`agent/insights.py`） | ✅ 核心 | `insights.rs` + `ulnclaw insights [--days N] [--source S] [--json]` CLI + REPL … [详情](#会话洞察-agent-insights-py) |
| 宠物（`agent/pet/` + `hermes_cli/pets.py`） | ✅ 核心 | `src/pets.rs` + … [详情](#宠物-agent-pet-hermes-cli-pets-py) |
| 建议自动化（`cron/suggestions.py` + `suggestions_cmd.py`） | ✅ 核心 | `cron/suggestions.rs`：`<home>/cron/suggestions.json` JSON 存储（tmp+rename 属主权限写入）， … [详情](#建议自动化-cron-suggestions-py-suggestions-cmd-py) |
| 状态报告（`hermes_cli/status.py` + `hermes_cli/subcommands/status.py` + `timefmt.py`） | ✅ 核心 | `status.rs`：`show_status` 移植 —— 面板头 + Environment（版本 / home / config.toml / .env … [详情](#状态报告-hermes-cli-status-py-hermes-cli-subcommands-status-py-timefmt-py) |
| 日志查看 + 文件日志（`hermes_cli/logs.py` + `hermes_cli/subcommands/logs.py` + `hermes_logging.py` 滚动手柄） | ✅ 核心 | `logs.rs`：查看器移植 —— `LOG_FILES` 注册表（agent/errors/gateway）、`_parse_since`（Ns/m/h/d … [详情](#日志查看-文件日志-hermes-cli-logs-py-hermes-cli-subcommands-logs-py-hermes-logging-py-滚动手柄) |
| 自更新器（`hermes_cli/subcommands/update.py` + `update_cmd.py` git 核心） | ✅ 核心 | `update.rs`：`--check` 为 `_cmd_update_check` 移植 —— 分支解析（`--branch` > 当前分支 > maste … [详情](#自更新器-hermes-cli-subcommands-update-py-update-cmd-py-git-核心) |
| 备份与恢复（`hermes_cli/backup.py` + `hermes_cli/subcommands/backup.py`） | ✅ 核心 | `backup.rs`：完整 zip 备份（hermes `run_backup` —— 适配后的 … [详情](#备份与恢复-hermes-cli-backup-py-hermes-cli-subcommands-backup-py) |
| 回退链 CLI（`hermes_cli/fallback_cmd.py` + `fallback_config.py`） | ✅ 核心 | `fallback.rs`：运行时链已存在（`[model] fallbacks` 规格 + `agent::with_fallback_specs` / … [详情](#回退链-cli-hermes-cli-fallback-cmd-py-fallback-config-py) |
| 活跃会话租约（`hermes_cli/active_sessions.py`） | ✅ 核心 | `active_sessions.rs`：跨进程租约注册表 `<home>/runtime/active_sessions.json`，由 … [详情](#活跃会话租约-hermes-cli-active-sessions-py) |
| 配置管理 CLI（`hermes_cli/config.py` config_command） | ✅ 核心 | `config_cmd.rs`：`show`（面板头 + 路径 + 全量配置，机密键经 `status::redact_key` 脱敏） … [详情](#配置管理-cli-hermes-cli-config-py-config-command) |
| Shell 补全（`hermes_cli/completion.py`） | ✅ 核心 | `ulnclaw completion <shell>` 基于 clap_complete：bash / zsh / fish（hermes 集合）另加 elvish / powershell；从实时 clap 命令树生成，自动跟随子命令变化（hermes 遍历 argparse 树同理）；SIGPIPE 恢复默认处理，管道接入 `head` 时静默退出 |
| 环境转储与版本（`hermes_cli/dump.py`、`build_info.py`） | ✅ 核心 | `ulnclaw dump [--show-keys]`：纯文本、可直接粘贴的装机摘要——版本 + git SHA/提交日期、系统、profile、home、模 … [详情](#环境转储与版本-hermes-cli-dump-py-build-info-py) |
| 记忆 CLI（`main.py cmd_memory`） | ✅ 核心 | `ulnclaw memory`：分库状态（`memory/MEMORY.md` 代理笔记与 `memory/USER.md` 用户画像的条目数 + 字节数，二者会注入每轮系统提示词）；`ulnclaw memory reset [all\|memory\|user] [--yes]`：hermes 风格清除清单（`◆ 文件 (说明) — N 字节`）、非 `--yes` 时交互式输入 `yes` 确认、逐文件 `✓ Deleted` 报告；REPL 内 `/memory` 查看当前内容；P323 新增 `GET /api/memory`（状态概览）与 `POST /api/memory/reset`（定向清除），桌面配置视图提供持久记忆区块（对应 hermes `/api/memory`） |
| 审批模式 CLI（`hermes_cli/approval_mode.py`） | ✅ 核心 | `ulnclaw approvals [manual\|smart\|off]`：查看当前生效的终端审批模式，或经规范配置写入器持久化新模式（config.toml `approvals.mode`），写入后重新读回校验生效，并按 hermes 风格报告用法错误/受管配置失败；模式语义（`manual` 人工确认、`smart` 先问辅助守护 LLM、`off` 硬底线之外自动放行）与终端守卫一致 |
| 提示词体积诊断（`hermes_cli/prompt_size.py`） | ✅ 核心 | `ulnclaw prompt-size [--json]`：测量每次调用的固定负载——系统提示词按四层拆分（基础身份 / 持久记忆 / 环境 / 易变日期+模 … [详情](#提示词体积诊断-hermes-cli-prompt-size-py) |
| 调试分享包（`hermes_cli/debug.py`） | ✅ 核心 | `ulnclaw debug report [--lines N] [--no-redact] [--output DIR]`：以本地文件方式收集 hermes 风格分享包（不上传 pastebin）——`report.txt`（强制脱敏的 `ulnclaw dump` + agent/errors/gateway 日志尾部）加每份存在的完整日志，均带 dump 头部与脱敏横幅；每个文件一次快照同时派生摘要/全文（防轮转竞态），机密经脱敏引擎 + 邮箱掩码处理，支持 `.1` 轮转回退，绝不改动磁盘日志 |
| 技能束（`agent/skill_bundles.py`、`hermes_cli/bundles.py`） | ✅ 核心 | `ulnclaw bundles list\|show\|create\|delete\|reload`：`<home>/skill-bundles/` 下的 YAML 技能束，把一组技能合并加载（`name/description/skills/instruction`，缺省以文件名兜底、slug 归一化与技能一致、重名 slug 先到先得、坏 YAML 跳过不影响发现）；REPL `/<bundle> [指令]` 一次把全部成员技能的 SKILL.md 注入同一轮，带 hermes 风格头部（已载/缺失清单、束指令、用户指令），束优先于同名未知命令，连字符/下划线互通；缺失技能跳过并提示（与 `-s` 预载同样宽容） |
| 导入其他 Agent 配置（`hermes_cli/agent_import.py`） | ✅ 核心 |  … [详情](#导入其他-agent-配置-hermes-cli-agent-import-py) |
| 会话技能标题修复（`hermes_cli/sessions_cmd.py retitle-skills`） | ✅ 核心 | `ulnclaw sessions retitle-skills [--limit N] [--apply]`（默认干跑）： … [详情](#会话技能标题修复-hermes-cli-sessions-cmd-py-retitle-skills) |
| Secrets 保险库（`agent/secret_sources/`） | ✅ 核心 | `src/secrets.rs` + `ulnclaw secrets status\|sync [--apply]`：外部秘密源在启动时、任何 provider … [详情](#secrets-保险库-agent-secret-sources) |
| 凭证池（`agent/credential_pool.py` + `hermes_cli/auth.py` + 仪表盘 `/api/credentials/pool`） | ✅ 核心 | P330 精简移植：`src/credential_pool.rs` —— `<home>/credentials-pool.json` 存储手工登记的按 pr … [详情](#凭证池-agent-credential-pool-py-hermes-cli-auth-py-仪表盘-api-credentials-pool) |
| iron-proxy 出站防火墙（`hermes egress`、`agent/proxy_sources/iron_proxy.py`） | ✅ 核心 | `src/iron_proxy.rs` + `src/egress_cmd.rs` + … [详情](#iron-proxy-出站防火墙-hermes-egress-agent-proxy-sources-iron-proxy-py) |
| Computer Use（`tools/computer_use/`） | ✅ 核心 | `src/computer_use.rs` + `ulnclaw computer-use status\|doctor\|install`：经 cua-drive … [详情](#computer-use-tools-computer-use) |
| 插件系统（`hermes_cli/plugins.py`、`agent/shell_hooks.py`） | ✅ 核心 | `src/plugins.rs` + … [详情](#插件系统-hermes-cli-plugins-py-agent-shell-hooks-py) |
| 消息平台网关（`gateway/platforms/`） | ✅ 核心 | `src/messaging.rs` —— hermes 平台网关架构运行于 `ulnclaw gateway` 内：适配器将入站聊天消息归一化为 … [详情](#消息平台网关-gateway-platforms) |
| 交互式 clarify（`tools/clarify_gateway.py` + WhatsApp interactive） | ✅ 核心 | `src/clarify_gateway.rs` + 消息层集成 —— `clarify` 工具在消息会话中可用：提问登记于有上限的网关注册表（hermes s … [详情](#交互式-clarify-tools-clarify-gateway-py-whatsapp-interactive) |
| 语音转写（`tools/transcription_tools.py` + gateway STT 管道） | ✅ 核心 | `src/stt.rs` —— hermes 音频 STT 管道：`[stt]` 配置（enabled/echo_transcripts/provider/la … [详情](#语音转写-tools-transcription-tools-py-gateway-stt-管道) |
| OAuth 登录 + 技能同步（`hermes_cli/portal_cli.py`、`tools/skills_sync_client.py`） | ✅ 核心 | `src/oauth.rs` + `src/skills_sync.rs`：hermes 门户认证 + Skill Sync 的服务无关移植。 … [详情](#oauth-登录-技能同步-hermes-cli-portal-cli-py-tools-skills-sync-client-py) |
| OAuth 上游代理（`hermes_cli/proxy/`） | ✅ 核心 | `src/proxy_cmd.rs` + `ulnclaw proxy start\|status\|providers`（P179）：本地 OpenAI 兼容代理 … [详情](#oauth-上游代理-hermes-cli-proxy) |
| 桌面 GUI（`apps/desktop` Electron） | ✅ 核心 | `desktop-electron/` —— **ulnclaw desktop**：v0.7.0 起 ulnclaw 直接交付 hermes Electron … [详情](#桌面-gui-apps-desktop-electron) |
| 会话浏览（`hermes_cli/sessions_cmd.py browse` + curses 挑选器） | ✅ 核心 | `ulnclaw sessions browse [--source S] [--limit N]`：TTY 上启用原始模式 TUI（crossterm 移植 … [详情](#会话浏览-hermes-cli-sessions-cmd-py-browse-curses-挑选器) |
| 会话恢复与单会话连续性（`cli.py --resume/--continue`） | ✅ 核心 | 全局 `-r/--resume <id或前缀>` 与 `-c/--continue` 标志，适用于 `chat` 与 `run`：整个 REPL 会话存于同一条 … [详情](#会话恢复与单会话连续性-cli-py-resume-continue) |
| 会话库修复（`hermes_state.py repair_state_db_schema`） | ✅ 核心 | `ulnclaw sessions repair [--check-only] [--no-backup]`：健康探测（`db_opens_cleanly` — … [详情](#会话库修复-hermes-state-py-repair-state-db-schema) |
| 会话删除/重命名/优化（`hermes_cli/sessions_cmd.py`） | ✅ 核心 | `ulnclaw sessions delete <id> [--yes]`（id 或唯一前缀，`resolve_session_id` —— LIKE 转义前 … [详情](#会话删除-重命名-优化-hermes-cli-sessions-cmd-py) |
| 供应链安全审计（`hermes_cli/security_audit.py`） | ✅ 核心 | `ulnclaw security audit [--json]`：按需对固定版本的 MCP 服务器包做 OSV.dev 审计（`npx pkg@ver` / … [详情](#供应链安全审计-hermes-cli-security-audit-py) |

### 详细说明（工具对标）

<a id="密钥脱敏-agent-redact-py"></a>

#### 密钥脱敏（`agent/redact.py`） — ✅ 核心

- terminal 输出（前台 + process log/wait）与 read_file 内容均经脱敏器：约 55
  种厂商前缀令牌（sk-/ghp_/glpat-/AKIA/xox…/JWT/私钥/数据库连接串/Authorization 与 x-api-key 头）、env 转储命令的 KEY=value
  脱敏、其余场景的 JSON/YAML 密钥字段
- 文件读取内容使用不可复用哨兵 `«redacted:prefix…»`，避免代理把截断密钥写回（hermes #35519）
- web URL 查询参数脱敏保持可选
- Smart-DENY 业主覆盖流已完整移植 —— `smart_denied` 标志、仅 once/deny 选项、单操作范围、smart-DENY 下不持久化、人工批准重置断路器（见智能审批行）
- profile 密钥作用域已移植 —— 见多路复用行

<a id="蓝图-blueprints-tools-blueprints-py"></a>

#### 蓝图 Blueprints（`tools/blueprints.py`） — ✅ 核心

- 在 frontmatter 声明 `metadata.hermes.blueprint.schedule` 的技能可排程：`skills blueprints`（列表）、`skills
  schedule <name>`（创建 `blueprint:<skill>` 定时任务并挂载技能）、`skills unschedule <name>`
- 畸形 blueprint 块显式报错
- `skills list` 以日程标注蓝图。P229 接入 hermes 蓝图建议流：`skills blueprints` 为每个未排程蓝图惰性登记一条待处理 `blueprint`
  源建议（对标 hermes `register_blueprint_suggestion` —— 去重闩锁、MAX_PENDING 上限
- 接受后产生与 `skills schedule` 相同的 `blueprint:<skill>` 任务）。统一建议面为 `cron/suggestions.rs` + REPL/CLI
  `/suggestions`（accept/dismiss/catalog/clear，含精选起始目录）。`export_blueprint`（`skills publish` 分享路径）仍未移植
  —— ulnclaw 无 skills hub/发布面

<a id="技能守卫-tools-skills-guard-py"></a>

#### 技能守卫（`tools/skills_guard.py`） — ✅ 核心

- `skills scan <name> [--source <repo>] [--json] [--force]`：静态扫描器 `skills-guard-v1`，扫描 SKILL.md
  及关联文件——119 条威胁模式（外泄/破坏/持久化/供应链/提示注入）、不可见 Unicode 检测、结构限制（50 文件 / 1 MB / 单文件 256
  KB、符号链接逃逸与可执行位检查）、信任等级（builtin / agent-created / 受信任仓库含前缀别名 /
  community）、裁定策略（critical→dangerous、high→caution
- community+caution 拦截，trusted 源遇 dangerous 同样拦截，`--force` 仅对非 community 的 caution 可覆盖）

<a id="delegate-task"></a>

#### `delegate_task` — ✅ 完整

- 并行子代理、深度限制、隔离上下文、子会话
- hermes v2026.8.3 后台语义：顶层委派即发即忘（`mode:
  background`、delegation_id、`cache/delegation/live/<id>/task-N.log`
  实时记录），全部子任务完成后以**单条**汇总结果重新进入会话（REPL 与网关会话聊天前置排队消费）
- 编排子代理（深度 > 0）保持同步
- 一次性/无状态会话强制同步执行并附说明（`tools/async_delegation.py` 移植，含持久 sqlite 登记：派发与汇总结果持久化于
  `async_delegations`，启动恢复将崩溃后仍 `running` 的行转为终态 `unknown` 结果，drain 经持久投递认领生命周期认领未投递行——每次认领累计
  `delivery_attempts`、300 秒过期认领可接管、源会话已消失的完成结果在 8 次尝试后收敛为终态 `dropped`、成功注入在认领令牌下标记 `delivered`）
- `GET /v1/delegations` + `/v1/delegations/:id` 登记端点（ulnclaw 运维扩展）

<a id="cronjob"></a>

#### `cronjob` — ✅ 完整

- create/list/update/pause/resume/remove/run
- `30m` / `every 2h` / `0 9 * * *` / ISO 一次性计划
- SQLite 任务存储
- 调度循环——网关每 30s 自动派发到期任务为受跟踪的 cron 运行（cron 审批作用域，结果回写任务行），`ulnclaw cron run <id>` 可在 CLI 立即执行一次
- P219 投递对齐：`deliver` 参数（聊天内创建默认 `origin`，否则 `local`
- 平台名/`platform:chat[:thread]`/逗号混合/`all` 路由令牌）、从实时消息上下文捕获 origin，解析出的目标经平台发送器接收最终响应（或紧凑失败摘要），带
  hermes 包装抬头 + `[SILENT]` 抑制

<a id="kanban-12-个工具"></a>

#### `kanban_*`（12 个工具） — ✅ 完整

- 本地 SQLite 协作看板，与 `ulnclaw kanban` CLI、网关 `/api/kanban/*` 端点共用同一 `KanbanStore` 引擎与
  `kanban.db`（一块看板、三个界面 —— hermes 对齐）：create（支持 `parents`）/list/show/comment/heartbeat（自动认领
  todo→ready→running）/complete/block/unblock/link/attach/attach_url/attachments
- 唯一前缀 id 解析，`ULNCLAW_KANBAN_TASK`/`HERMES_KANBAN_TASK` 工作进程上下文（worker 省略 task_id 默认自身任务
- create/unblock/link 仅限编排者，hermes 门控语义），REPL `/kanban` 看板操作经 `run_slash`

<a id="browser-12-个工具"></a>

#### `browser_*`（12 个工具） — ✅ 完整

- CDP WebSocket 客户端（`browser` 模块）：端点发现、页面会话、带元素引用的可访问性快照、点击/输入/滚动/按键/截图/执行 JS/对话框
- `ULNCLAW_BROWSER_CDP` 支持 ws://、http://host:port 或 `auto`（监督器启动托管的无头 Chrome/Chromium）
- 已移植 hermes SSRF 防护（`browser/guard.rs`）：敏感查询参数 +
  云元数据底线无条件拦截，非本地端点或容器化终端启用私网地址防护，重定向落地复检，console/eval 表达式内 URL 字面量预筛，私有页面下原始 CDP 方法白名单
- 浏览器输出强制脱敏
- REPL `/browser connect` 与网关 `/v1/browser/connect|disconnect|status` 实时切换端点
- `CAMOFOX_URL` 接入 Camofox REST 后端
- 云浏览器 provider（Browserbase / Browser Use / Firecrawl）经 `browser/cloud.rs` 提供——复刻 hermes provider
  注册表语义，含 `[browser] cloud_provider` 选择、传统可用性遍历、会话过期退役与退出时释放（见「云浏览器 provider」行）

<a id="close-terminal-read-terminal-focus-pane-open-preview"></a>

#### `close_terminal`、`read_terminal`、`focus_pane`、`open_preview` — ✅ 核心

- 桌面 GUI 工具（hermes `close_terminal_tool.py` / `read_terminal_tool.py` / `focus_pane_tool.py` /
  `open_preview_tool.py`）：仅在 `ULNCLAW_DESKTOP=1` 下注册，经 `desktop`
  桥接层路由——宿主应用安装事件发射器（`ulnclaw::desktop::set_emitter`）接收 `(ui_session_id, event, payload)`
  事件（`terminal.close`、`pane.reveal`、`preview.open`）及阻塞式 `read_terminal` 回调
- 未接入宿主时返回 "desktop only"，从不杀进程，并规范化裸域名（`www.cnn.com` → https、`localhost:3000` → http）
- `react_to_message`（hermes `react_to_message_tool.py`
  移植）：代理表情回应——每作者一个、重发相同表情即撤回，默认最新用户消息（`messages_back` 回溯、`message_row_id` 精确指定），持久于
  `messages.display_metadata` 并经 `message.reaction` 桥接事件实时渲染
- 门控于 `ULNCLAW_DESKTOP=1` **与** `[display] message_reactions`。P231 起，当网关是桌面外壳的子进程时，桥接事件经 `GET
  /api/desktop/events` SSE 传输，桌面 webview 成为一等 emitter 宿主

<a id="discord-discord-admin"></a>

#### `discord`, `discord_admin` — ✅ 核心

- P245 完整移植 hermes `tools/discord_tool.py`（`src/discord_tool.rs`）：15 个 REST API 动作拆分为 `discord`
  核心（fetch_messages / search_members / create_thread）与 `discord_admin`（服务器/频道/角色/成员/置顶/线程管理）
- 令牌经 profile 密钥作用域解析
- schema 双重门控——特权意图经 `GET /applications/@me` 非阻塞探测（内存缓存 → 24 小时磁盘缓存，以 sha256(token)[:16] 为键 → 宽松默认 +
  一次后台探测，hermes `_detect_capabilities_nonblocking`）隐藏 GUILD_MEMBERS 动作并标注缺失 MESSAGE_CONTENT
  的情形，`[discord] server_actions`（逗号字符串或数组）在 schema 与调用时双重白名单过滤
- 按服务器权限不预检——403 于调用时富化为可操作的权限指引（hermes `_enrich_403`）
- 4 MiB 响应 / 64 KiB 错误体上限、15 秒超时

<a id="feishu-doc-read-feishu-drive"></a>

#### `feishu_doc_read`, `feishu_drive_*` — ✅ 核心

- P246 完整移植 hermes `tools/feishu_doc_tool.py` +
  `tools/feishu_drive_tool.py`（`src/feishu_doc_tool.rs`）：`feishu_doc_read`（工具集 `feishu_doc`）经
  `/open-apis/docx/v1/documents/:id/raw_content` 读取文档纯文本全文
- `feishu_drive_list_comments` / `feishu_drive_list_comment_replies` / `feishu_drive_reply_comment`
  / `feishu_drive_add_comment`（工具集 `feishu_drive`）覆盖评论线程的列出/回复/新增，`user_id_type=open_id`、`page_size`
  钳制 1–100、请求体与 hermes 一致（text_run elements / reply_elements）
- 凭据解析顺序 secret scope → env/`.env` → `[messaging.feishu]` 配置，租户令牌按 app id 缓存并提前刷新（约 110 分钟）—— hermes
  依赖注入线程本地 lark 客户端、仅在飞书评论上下文可用，ulnclaw 在任意配置了凭据的会话中均可用（超集）

<a id="spotify-7-工具"></a>

#### `spotify_*`（7 工具） — ✅ 核心

- P247 完整移植 hermes `plugins/spotify`（`src/spotify_tool.rs` +
  `src/spotify_auth.rs`）：`spotify_playback`（get_state/currently-playing/play/pause/next/previous/seek/set_repeat/set_shuffle/set_volume/recently_played，含
  204 空态描述）、`spotify_devices`（list/transfer）、`spotify_queue`（get/add）、`spotify_search`（7
  种条目类型）、`spotify_playlists`（list/get/create/add_items/remove_items/update_details）、`spotify_albums`（get/tracks）、`spotify_library`（tracks/albums
  × list/save/remove）—— Spotify Web API 客户端：Bearer 鉴权、401 强制刷新令牌后重试一次、401/403-Premium/404/429 友好错误映射
- id/URI/open.spotify.com-URL 归一化
- 授权经 `ulnclaw spotify-auth login` —— PKCE S256 环回流（与 hermes `login_spotify_command` 一致：相同
  scope、默认回调 `http://127.0.0.1:43827/spotify/callback`、state 随机数校验、RFC 7636 challenge），令牌存于
  `auth.json` 的 `providers.spotify`，提前 120 秒刷新、刷新彻底失败时隔离死令牌（与 hermes
  `resolve_spotify_runtime_credentials` 一致）
- 工具按存储的授权状态（`logged_in`）门控

<a id="yb-5-个元宝工具"></a>

#### `yb_*`（5 个元宝工具） — ✅ 核心

- P248 完整移植 hermes `tools/yuanbao_tools.py`（`src/yuanbao_tool.rs`）：`yb_query_group_info` /
  `yb_query_group_members`（find/list_bots/list_all + @提及提示，角色标签 user/yuanbao_ai/bot）经存活适配器的 WS 业务 RPC
  执行（`src/yuanbao.rs` `YuanbaoHandle` —— 与 hermes `get_active_adapter` 一致，每会话重新注册），新增 proto
  编解码（`encode_query_group_info` / `encode_get_group_member_list` + 嵌套 GroupInfo / repeated MemberInfo
  响应解码）
- `yb_send_dm` 按昵称模糊匹配解析收件人，发送分块 C2C 文本（携带群上下文）并经 COS 上传路径发送媒体（消息中的 `MEDIA:` 标签与 hermes `extract_media` 一致地被提取）
- `yb_search_sticker` / `yb_send_sticker` 使用已移植的贴纸目录（id/名称/随机查找、TIMFaceElem、可选群引用回复 ref）
- 工具按适配器存活状态门控（hermes `_check_yuanbao`）
- 差异：ulnclaw 无逐轮会话环境变量，`chat_id`/`group_code` 为显式参数

<a id="send-message"></a>

#### `send_message` — ✅ 核心

- P259 完整移植 hermes `tools/send_message_tool.py` +
  `gateway/channel_directory.py`（`src/send_message_tool.rs` +
  `src/channel_directory.rs`）：`send`/`list`/`react`/`unreact` 四个动作，忠实的 target 语法（裸平台名 →
  `<PLATFORM>_HOME_CHANNEL` / `EMAIL_HOME_ADDRESS` 主频道，`platform:chat_id`、`platform:chat_id:thread_id`
  Telegram 话题 / Discord 线程、`@username`、Slack `C/G/D` + `U…` 用户 + `@handle` + 线程 ts 形式、Matrix
  `!room`/`@user`/`:$thread`、微信/WhatsApp-JID/E.164/photon-GUID/ntfy/email/元宝守卫），人类友好频道名解析（精确 id →
  精确名称/展示标签 → 唯一前缀 →
  唯一子串），基于持久化频道目录（`<home>/channel_directory.json`，分发器每条入站事件刷新，`channel_aliases.json` 用户别名覆层）
- `MEDIA:<path>` 标签在 Telegram（照片/文档 + hermes `_media_caption_split` 1024 字符题注语义）、Discord（单条
  multipart 消息 `payload_json` + files）、Slack（`chat.postMessage` + 现代三步文件上传）原生投递，其余平台回退为诚实的文本描述
- 表情回应经 Telegram `setMessageReaction`、Discord `PUT/DELETE …/reactions/…/@me`、Slack
  `reactions.add/remove`，省略 `message_id` 时自动取目录记录的最近入站消息
- 以存活平台适配器为门控（hermes `_check_send_message`）

<a id="x-search"></a>

#### `x_search` — 🟡 门控

- hermes `x_search_tool.py` 完整移植：xAI Responses-API `x_search` 服务端工具，支持账号白/黑名单（最多 10 个、去
  `@`）、严格客户端日期范围校验（YYYY-MM-DD、禁止倒置/纯未来窗口）、`enable_image_understanding` /
  `enable_video_understanding`、5xx/瞬时错误退避重试、过滤无引文时的 `degraded`/`degraded_reason` 标记、`[x_search]`
  配置（model / reasoning_effort / timeout_seconds / retries）
- 仅在 `XAI_API_KEY` 存在**且**启用可选 `x_search` 工具集时注册（hermes 对齐——SuperGrok OAuth 路径未移植）

<a id="video-generate-bfl-flux3"></a>

#### `video_generate`、`bfl_flux3_*` — ✅ 核心

- `video_gen.rs` provider 注册表（hermes 插件设计：单一可用后端自动选中、配置名
  fail-closed、`success_response`/`error_response` 契约）+ 统一 `video_generate`
  工具（文生视频/图生视频/参考图生成、软校验、模型解析顺序 参数 > `[video_gen]` 配置 > provider 默认）
- `managed_gateway.rs` Nous 工具网关传输（auth.json bearer + `TOOL_GATEWAY_USER_TOKEN`、`{vendor}-gateway`
  URL 构造、预签名 `nous-upload:` 媒体上传）
- 6 个 `bfl_flux3_*` 工具带固定 schema、本地路径上传预处理、轮询至完成（限流/传输错误处理、240s 兜底）、签名 URL 下载到 `~/Downloads`（`.part` 暂存 + 冲突后缀）+ 提示词指南
- `video_gen_xai.rs` xAI Imagine 后端（auth.json OAuth access-token 复用 → `XAI_API_KEY` 回退、文生/图生视频模型路由含
  1.5 模型、edit/extend 提交+轮询流程）+ `xai_video_edit`/`xai_video_extend` 工具（公网 HTTPS URL
  校验、`provider_not_configured` 门控）
- `video_gen_backends.rs` FAL 后端（六大模型家族 —— LTX 2.3、Pixverse v6、Veo 3.1、Seedance 2.0、Kling v3
  4K、Happy Horse —— 能力驱动载荷、`FAL_KEY` 直连队列 REST 或 Nous `fal-queue` 托管网关）与 DeepInfra 后端（OpenAI 兼容
  `/videos` 创建→轮询→下载到 `~/videos`）
- 不做 OAuth 刷新 —— 缓存的 Nous token 原样使用

<a id="project-list-project-create-project-switch"></a>

#### `project_list`、`project_create`、`project_switch` — ✅ 核心

- hermes `tools/project_tools.py` + `hermes_cli/projects_db.py` 完整移植：每 profile
  `projects.db`（projects / project_folders / project_meta / discovered_repos，WAL + DELETE 回退 +
  增量列迁移）、slug 校验 + `-2` 冲突后缀、多文件夹工作区与主目录指针（首个文件夹隐式为主、删除时降级/重指）、归档/恢复/硬删除（文件夹级联）、活动项目指针、最长前缀
  `project_for_path` 解析、确定性 kanban 分支名（`<slug>/<task-id>[-<title-slug>]`）、带策略协调的仓库发现缓存
- 工具置于可选 `project` 工具集（仅 GUI 会话 —— 与 hermes 一致不进核心集），宿主应用可安装工作区重锚回调（`projects_db::set_project_workspace_callback`）

<a id="技能使用遥测-学习图谱-skill-usage-learning-graph-learning-mutations"></a>

#### 技能使用遥测 + 学习图谱（`skill_usage`、`learning_graph`、`learning_mutations`） — ✅ 核心

- hermes `tools/skill_usage.py` + `agent/learning_graph.py` + `agent/learning_mutations.py`
  移植：`<home>/skills/.usage.json` 旁路记录（view/use/patch 计数、生命周期状态、固定、agent 创建溯源、原子写入），遥测接入
  `skill_view`/`skill_manage`（bump view/patch、标记 agent 创建、删除时 forget），经 `skills/.archive`
  的技能归档/恢复（冲突时间戳后缀、固定技能拒绝）
- 学习图谱载荷 —— 已学技能过滤（agent 创建或使用过）、`related_skills` 边、`MEMORY.md`/`USER.md` 条目记忆卡片、词法 记忆→技能 边（每卡 top-4）、聚类 + 密度统计
- journey 节点变更（`node_detail`/`delete_node`/`edit_node`）与 memory 工具的条目格式对齐

<a id="学习时间线-journey-cli-learning-graph-render-journey"></a>

#### 学习时间线 / `journey` CLI（`learning_graph_render`、`journey`） — ✅ 核心

- hermes `agent/learning_graph_render.py` + `hermes_cli/journey.py` 移植：`learning_graph_render.rs` ——
  桌面同源色彩数学（调色板推导、互补记忆色、smoothstep 年龄渐变）、新旧度计算（带时间 + 序号回退）、日/月/年分桶时间线（按主导类别着色的技能/记忆比例条 —— 学习热图）、编号
  charted-signal 标记、累计轨迹 sparkline、图例/坐标轴/摘要装饰
- `ulnclaw journey` CLI —— 时间线帧（`--reveal`、`--width/--height`、`--no-color`）、`--play` 动画、`--json`
  载荷导出、`journey list`、`journey delete <node> [-y]`（技能归档、记忆重写）、`journey edit <node>` 经 `$EDITOR`
- TUI 预渲染（`render_frames`）与 GUI 星图仍为桌面专属

<a id="技能策展-cli-curator"></a>

#### 技能策展 CLI（`curator`） — ✅ 核心

- hermes `hermes_cli/curator.py` 本地半区（LLM 整合运行留在桌面侧）：`curator.rs` —— 空闲天数计算（活动优先、created_at
  回退）、裁剪候选选择（agent 创建、未固定、未归档、空闲 ≥ N 天、最空闲优先）、状态汇总、相对时间渲染
- `skill_usage.rs` 报表 —— `usage_report`（磁盘上全部技能含溯源/计数/最近活动）、`unmanaged_report` /
  `list_unmanaged_skill_names` / `adopt_skill`（溯源标记）、`list_archived_skill_names`
- CLI `ulnclaw curator status\|pin\|unpin\|archive\|restore\|list-archived\|usage [--sort
  activity\|name\|recent] [--json]\|prune [--days N] [--dry-run] [-y]\|adopt [names \|
  --all-unmanaged] [--dry-run] [-y]\|list-unmanaged`
- 同时以进程级 env 锁加固网关 env 覆盖测试
- P316 将策展经 HTTP 暴露——`GET /api/curator`（状态汇总 + 已归档清单 + 按活动排序的使用表）+ `POST
  /api/curator/pin|unpin|archive|restore`（固定技能拒绝归档），在桌面技能视图的策展分区渲染

<a id="持久化目标-ralph-循环-goals"></a>

#### 持久化目标 / Ralph 循环（`goals`） — ✅ 核心

- hermes `hermes_cli/goals.py` 移植：`goals.rs` ——
  `GoalContract`（outcome/verification/constraints/boundaries/stop_when，别名表 `parse_contract`
  使无关冒号不被误解析，空字段省略，带标签 `render_block`）、`GoalState` serde
  往返（状态、轮次预算、子目标、解析/传输失败计数、pid/会话/时间等待屏障）、`parse_judge_response`（verdict + 旧式 `done` 布尔、去代码围栏、内嵌 JSON
  提取、无目标时 wait 指令降级）、面向裁判的背景进程块渲染
- `GoalManager` 按会话编排并持久化于 `state_meta`（键
  `goal:<session_id>`，set/set_contract/pause/resume/clear/mark_done，子目标增删清，wait_on/wait_on_session/wait_for_seconds/stop_waiting
  惰性自动清除，status_line，contract>subgoals>plain 优先级的 next_continuation_prompt，render_contract）
- fail-open `judge_goal` 经 `goal_judge` 辅助任务（contract>subgoals>plain 提示、背景进程、传输/解析失败追踪）+ `draft_contract`
- `evaluate_after_turn` 状态机拆分为可纯测的 `apply_verdict`（等待屏障短路不耗轮次、WAIT 停泊、DONE、传输连续 5 次自动暂停、解析连续 3
  次自动暂停、轮次预算耗尽、continue）+ 异步裁判包装
- `migrate_goal_to_session`
- terminal.rs 新增后台进程 pid 捕获 +
  `background_process_running`/`background_process_exists`/`list_background_processes` 支撑会话等待屏障
- REPL `/goal`（status/show/draft/pause/resume/clear/wait/unwait、内联契约、自动启动）+ `/subgoal`（list/add/remove/clear）
- `AuxiliaryTaskConfig.max_tokens` 配置项

<a id="网关-profile-多路复用-p-profile-cdp-会话存活"></a>

#### 网关 profile 多路复用（`/p/<profile>`）+ CDP 会话存活 — ✅ 核心

- hermes api_server profile 前缀中间件移植：所有网关路由镜像到 `/p/<profile>/...`
- `[gateway] multiplex_profiles = true` 时每个镜像由独立栈支撑（agent 取自 `[profiles.<name>]` 覆盖，home 按 profile
  隔离 `<home>/profiles/<name>` —— state.db/approvals.json/cron/skills），惰性构建并缓存（`ProfileHub`），未知 profile
  → 404 `Unknown or unconfigured profile`
- 多路复用关闭时前缀被接受但由默认 profile 服务（对齐 hermes `_resolve_request_profile`）
- 镜像同样经 bearer 鉴权。CDP 客户端加固：`CdpClient.is_connected`（读/写循环在套接字断开时翻转 closed 标志并让在途调用快速失败 —— 不再空等 30
  秒超时），`with_session` 透明丢弃已死的缓存会话并重建。Profile 密钥作用域（P222，hermes `agent/secret_scope.py` 移植）：多路复用开启时，每个
  `/p/<profile>/...` 请求都运行在该 profile 的失败关闭（fail-closed）密钥作用域内（`<home>/profiles/<name>/.env` +
  按需水合的外部秘密源，剔除真正的全局变量）
- 作用域内读取只解析作用域本身——不再回退到可能残留其他 profile 值的进程环境——无作用域的凭据读取会以 `UnscopedSecretError` 大声失败，而不是泄漏其他 profile 的值
- cron 调度器为每次任务运行安装作用域，`spawn_scoped` 在派生的运行任务内重装捕获的作用域（对齐 hermes `copy_context()`），多路复用关闭时处处恢复普通 env 行为

<a id="repl-显示与输入体验-hermes-cli-focus-view-py-prompt-stash-py-clipboard-py"></a>

#### REPL 显示与输入体验（`hermes_cli/focus_view.py`、`prompt_stash.py`、`clipboard.py`） — ✅ 核心

- `src/focus_view.rs`、`src/prompt_stash.rs`、`src/clipboard.rs` —— 三个 hermes CLI
  体验模块。**专注视图**（`/focus [on\|off\|status]`）：纯显示层的精简输出模式 —— 开启时把工具进度吸附为 `off` 并记住用户原模式（`/focus off`
  原样恢复），按轮诚实统计被隐藏的工具行（只计配置模式本会显示的行），轮末打印 `⋯ N tool lines hidden · /focus off to show` 恢复提示，另提供 `◉
  focus` 状态栏段
- 纯显示不变式：绝不改变发往模型的任何字节。**工具进度**（`/verbose [off\|new\|all\|verbose]`）：REPL 工具回调滚动行（`⚙ <tool>` 行
- `new` 去重连续同名）上的 hermes tool_progress_mode 档位循环。**草稿暂存**（`/stash [text\|list\|pop [n]\|drop
  <n>\|clear]`）：会话级纯内存草稿栈（hermes Ctrl+S 手势：有内容→暂存、空输入+1 条→弹回、空输入+多条→浏览
- 新者在前、上限 20 条、60 字符预览、`📌 n` 提示符指示、绝不落盘）。**剪贴板**（`/paste`）：跨平台剪贴板图片提取，以 PNG 存至
  `<home>/clipboard/`（macOS pngpaste/osascript、Windows/WSL2 PowerShell WinForms + Get-Clipboard +
  FileDropList 回退、Linux Wayland wl-paste 非 PNG 经 ImageMagick 归一化、X11 xclip）+
  `write_clipboard_text`（pbcopy → Set-Clipboard base64 → wl-copy → xclip → xsel，CJK 安全）+ SSH 会话检测（OSC
  52 提示）
- 桌面端 Ctrl+S 键绑定保留在桌面壳层

<a id="会话裁剪-归档-统计-session-filters-py"></a>

#### 会话裁剪/归档/统计（`session_filters.py`） — ✅ 核心

- `session/filters.rs` —— 时长解析（`5h`/`30m`/`2d`/`1w`，裸数字 = 天）、时间点解析（时长 = 距今多久前
- ISO 时间戳 naive=本地时区）、epoch 格式化、`PruneFilters` 类型化 WHERE 子句构建器（仅限已结束、last_active 取
  COALESCE(MAX(消息时间), started_at)、source/end_reason 精确匹配、title/model 大小写不敏感子串、cwd 前缀、消息/令牌/工具调用上下界、三态
  archived）+ 可读 `describe()`
- 存储层 `list_prune_candidates`（按最旧活动排序）、`prune_sessions`（先删消息 +
  FTS）、`archive_sessions`（软隐藏、幂等）、`set_session_archived`、`session_count_by_source`
- CLI `ulnclaw sessions prune|archive`（hermes 语义：裸 prune = 90 天以上，任一过滤器抑制隐式截断，裸 archive 拒绝执行，预览 +
  y/N 确认 + `--dry-run`、`--include-archived`）与 `sessions stats`（总量、按源计数、库大小）
- hermes 的计费/聊天/分支/成本过滤器对应 ulnclaw 未跟踪的列，不移植

<a id="皮肤-主题引擎-skin-engine-py"></a>

#### 皮肤/主题引擎（`skin_engine.py`） — ✅ 核心

- `skin.rs`：hermes 全部 9
  个内置皮肤以数据形式内置（default、ares、mono、slate、daylight、warm-lightmode、poseidon、sisyphus、charizard —— 258
  个颜色条目 + 品牌文案 + spinner 表情），局部调色板向 default 皮肤继承（`build_skin_config`），`list_skins`/`load_skin`（未知 →
  default），进程级活动皮肤（`init_skin_from_config` 读取 `[display]
  skin`，`get/set_active_skin`），`get_color`/`get_branding` 访问器，真彩 ANSI `colorize`（遵循 NO_COLOR）
- `ulnclaw skins` CLI 列出主题并标记当前激活
- REPL 提示行以活动皮肤的 `banner_dim` 着色。延后：`<home>/skins/` 用户 YAML 皮肤（无 YAML 依赖）、TUI 状态栏/prompt-toolkit 表面

<a id="欢迎横幅与更新检查-banner-py"></a>

#### 欢迎横幅与更新检查（`banner.py`） — ✅ 核心

- `banner.rs`：皮肤着色的欢迎面板（盒线绘制）—— 盲文爪痕主视觉 + 模型行（缩短名称、去 `.gguf`、28 字符上限、models.dev 上下文经
  `spawn_blocking` + 2 秒上限查询）、`approvals.mode = "off"` 警告（hermes YOLO 行）、cwd + 会话 id、"Available Tools"
  按启用工具集分组（显示 8 个，`+N more toolsets`）、技能按类别 + `+N more` 溢出、`N tools · N skills · /help for commands`
  汇总行
- ≥95 列终端额外显示 ULNCLAW 块字标（与 hermes 门限一致）
- git 更新检查 6 小时缓存于 `$ULNCLAW_HOME/.update_check`（版本变化即失效）—— 作用域 `git fetch` 落后计数 + 浅克隆 SHA 对比路径，官方
  SSH 远端走 `git ls-remote`（计数未知 → `-1` 哨兵），仓库目录 = `$ULNCLAW_REPO` → 构建期 `CARGO_MANIFEST_DIR` →
  `$ULNCLAW_HOME/ulnclaw`
- agent 构建期间后台线程 `prefetch_update_check` + `get_update_result(500ms)`
- 面板标题版本标签 `ulnclaw vX · upstream <sha8>`（+carried commits），最新 tag 查询 + gitee 发布
  URL（进程级缓存）。延后：标题富链接、皮肤 `banner_hero`/`banner_logo` 覆盖

<a id="浏览器-cdp-接入层-browser-connect-py"></a>

#### 浏览器 CDP 接入层（`browser_connect.py`） — ✅ 核心

- `browser/connect.rs`：Chromium 系候选发现（macOS/Windows/Linux，含 WSL `/mnt/c` 安装路径），覆盖 Chrome/Chromium/Brave/Edge
- 双栈回环 CDP 探测 —— `is_browser_debug_ready`（`/json/version` → `/json`，`ws://…/devtools/browser/…` 走
  TCP 连通）、`discover_local_cdp_url`（先 IPv4 后 `[::1]`，捕获被 IPv4 占用者挤到纯 IPv6 的浏览器）
- 端口仲裁 —— `local_port_in_use` 区分空闲与被占用，`find_free_debug_port` 要求双栈回环均可绑定
- 带诊断的可视调试浏览器启动 `launch_chrome_debug`（逐候选 `LaunchAttempt`：ready/starting/exited/spawn-failed，stderr
  尾部写入 `<home>/chrome-debug/launch-stderr.log`，退出码 0 的单实例吸收提示，`manual_chrome_debug_command` 兜底含 macOS
  `open -a` 形式）
- `connect_local_default` 组合出 hermes `/browser connect` 完整默认流程。REPL 裸 `/browser connect` 执行该流程，成功后设置实时覆盖并向会话注入 hermes 系统备注
- `/browser disconnect` 注入回退备注。托管启动候选表同步补齐 Brave/Edge。网关 `/v1/browser/*` 保持不变（已对齐）

<a id="doctor-doctor-py"></a>

#### Doctor（`doctor.py`） — ✅ 核心

- `doctor.rs` + `ulnclaw doctor` CLI：hermes 盒线横幅报告，✓/⚠/✗/ℹ 分级检查按段组织 —— Version & Updates（P61 的 git
  状态 + 6 小时缓存落后计数）、Configuration Files（config.toml 存在性/TOML 合法性/模型已配置、`.env` 密钥扫描）、Directory
  Structure（home + sessions/skills/memory/cron/checkpoints/logs、state.db）、Auth
  Providers（`resolve_api_key` 链：config → ULNCLAW_API_KEY → OPENAI_API_KEY → ANTHROPIC_API_KEY
- 本地免密钥提供商单独提示）、External Tools（git、P62 的 Chromium 系候选、内置 SQLite）、Toolsets（启用/禁用 + 经
  `resolve_toolset` 检测未知名称）、Skills（安装数量 + frontmatter 健全性）、Profiles（逐 profile 模型/工具集覆盖 + profile home）
- `--fix` 创建缺失的 home/子目录与默认 config.toml（hermes `--fix` 快速路径），`--online` 以阻塞 reqwest 探测提供商端点（bearer 密钥访问 `/v1/models`
- ollama 类本地走 `/api/tags`），`--json` 输出序列化报告
- 问题汇总编号列出 + `--fix` 提示，与 hermes 一致恒以退出码 0 结束

<a id="会话洞察-agent-insights-py"></a>

#### 会话洞察（`agent/insights.py`） — ✅ 核心

- `insights.rs` + `ulnclaw insights [--days N] [--source S] [--json]` CLI + REPL `/insights [days]`
  + 网关聊天 `/insights [N] [--days N] [--source S]` 斜杠命令：InsightsEngine 以第二个 WAL 读取连接分析 state.db ——
  总览（会话/消息/工具调用数、输入/输出/总令牌、平均会话时长、活跃天数）、基于 models.dev 定价的美元成本估算（`get_model_info`，provider
  取自当前配置作为提示，无定价显示 "cost unknown"）、按令牌排序的模型分解、来源分解（hermes 平台分解）、`role='tool'` 行的工具调用分解（前 30）、活动模式（按小时
  + 周一起始的星期桶、峰值检测）、按令牌排行的前 5 会话（含标题/日期）
- 排除已归档会话，`--source` 过滤对齐 hermes，终端渲染带 █ 条形图（hermes `_bar_chart`）、`format_duration_compact` + K/M
  令牌格式化，serde JSON 报告、技能使用分解（扫描 assistant `tool_calls` JSON 中的 `skill_view`/`skill_manage` 调用 ——
  每技能加载/编辑计数 + 最后使用日期、汇总总数、排行 `top_skills`，hermes `_get_skill_usage`/`_compute_skill_breakdown`
  语义）、`get_usage_breakdown` 工具+技能载荷（hermes 仪表盘路由形状）与紧凑 markdown `format_gateway` 渲染器（支撑网关 `/insights`
  斜杠回复）
- P328 新增 `GET /api/analytics/models`（1-365 天窗口内逐模型会话/消息/令牌/最近使用，存储侧 GROUP BY——对应 hermes `/api/analytics/models`
- 费用/能力列保留在模型视图目录表），模型视图呈现为用量表

<a id="宠物-agent-pet-hermes-cli-pets-py"></a>

#### 宠物（`agent/pet/` + `hermes_cli/pets.py`） — ✅ 核心

- `src/pets.rs` + `ulnclaw pets list|install|select|show|off|scale|remove|doctor|hatch`：petdex 吉祥物引擎
  —— 公共清单抓取（petdex.dev、300 秒进程内缓存 + 后台预热、资产下载锁定 petdex 主机）、按 profile 的 `<home>/pets/<slug>/`
  存储（pet.json + 精灵图：安装/加载/列表/解析/重命名/删除/zip 导出/空闲帧缩略图，防穿越 slug）、图集行分类推断（8 行旧版 vs 9 行 Codex 图集）+
  状态别名（waving/jumping/running）、`derive_pet_state`
  活动→动画映射（error→failed、celebrate→jump、completed→wave、awaiting-input→waiting、tool-running→run、reasoning→review）、四种终端渲染模式
  —— kitty 图形协议（分块 APC 传输 + Unicode 占位符虚拟放置载荷与行/列变音符）、iTerm2 内联图像、手写 DEC sixel（中值切割 ≤255
  色调色板量化）、带可读性下限的真彩 Unicode 半块回退 —— 由 `[display.pet]` 配置驱动（enabled/slug/scale
  0.1–3.0/render_mode/unicode_cols），select/off/scale 持久化写入
- LLM 宠物孵化流水线（`agent/pet/generate/` → `src/pets_atlas.rs` + `src/pets_generate.rs` + `ulnclaw pets
  hatch`）：基础草稿 → 锚定行条带生成 → 帧提取 → 图集合成/校验 → 商店注册，提示词与 hermes 逐字一致，色键背景移除（边缘泛洪填充 + 饱和键快速路径 +
  空洞修补）、互相关单元配准/归一化、running-left 镜像、idle 兜底、每行 3 次尝试的 4 路并发列生成，`[pets]` 配置 OpenAI
  兼容图像端点（image_base_url/image_api_key/image_model
- 密钥回退 OPENAI_API_KEY/ULNCLAW_API_KEY，模型回退 gpt-image-2），`--style`
  风格提示（pixel/plush/clay/sticker/flat-vector/3d-toy/painterly/auto）、`--drafts N` 仅出草稿模式与 `--base
  <path>` 从图片孵化
- REPL `/pet`（toggle/list/scale/off/<slug> 领养）+ `/hatch <description>` 斜杠命令（hermes cli_commands_mixin 语义，含进度打印）
- P126 移植了桌面生成悬浮层：桌面外壳的孵化对话框（提示词 + 风格 + 草稿数 → 基础草稿网格挑选 → 实时行进度 → 精灵图预览 + 自动领养）基于新的网关孵化任务 API（`POST
  /api/pets/hatch`、`GET /api/pets/hatch/:id`、`POST /api/pets/hatch/:id/pick|cancel`、`GET
  /api/pets/hatch/:id/draft/:index`）。已知差异：单一 OpenAI 兼容端点取代 hermes 的 Nous/OpenRouter/Krea 供应商注册表
- 精灵图以 PNG 编码（`image` crate 无 WebP 编码器），解码两种格式均可

<a id="建议自动化-cron-suggestions-py-suggestions-cmd-py"></a>

#### 建议自动化（`cron/suggestions.py` + `suggestions_cmd.py`） — ✅ 核心

- `cron/suggestions.rs`：`<home>/cron/suggestions.json` JSON 存储（tmp+rename 属主权限写入），完整 hermes 语义 ——
  pending/accepted/dismissed 状态、dedup-key 锁存（已决策的键不再重复建议）、MAX_PENDING=5
  积压上限、来源校验（catalog/blueprint/usage/integration）、按 id/1 基待处理序号/精确标题解析
- `accept` 将存储的 job_spec 经 `CronStore` 落地为真实 cron 任务并置 accepted
- `clear_resolved` 仅清除 accepted 记录（dismissed 保留作去重记忆）
- 4 条精选起始目录（每日简报、重要邮件监控、每周回顾、工作日开始提醒 —— 提示词改写为自包含，日程经 `parse_schedule` 验证），`seed_catalog_suggestions` 幂等
- REPL `/suggestions [accept N|dismiss N|catalog|clear]` 与 `ulnclaw suggestions` CLI 共用
  `handle_suggestions_command` 分发（accept/add/schedule 与 dismiss/no/reject 别名对齐、用法文本）

<a id="状态报告-hermes-cli-status-py-hermes-cli-subcommands-status-py-timefmt-py"></a>

#### 状态报告（`hermes_cli/status.py` + `hermes_cli/subcommands/status.py` + `timefmt.py`） — ✅ 核心

- `status.rs`：`show_status` 移植 —— 面板头 + Environment（版本 / home / config.toml /
  .env）、Model+Provider+Base URL、API Keys（config.toml `model.api_key` 行 + 20 项供应商环境变量表，备选变量依次回退，所有值经
  `redact_key` 脱敏）、Terminal Backend、Browser（endpoint + 浏览器发现）、Gateway（监听 / 鉴权密钥 / multiplex
- `--deep` 追加网关端口 TCP 探测）、Scheduled Jobs（启用/总数 + 下次运行）、Sessions（总数 + 最近会话）、Skills（已安装 +
  待处理建议）、Updates（git 上游检查，6h 缓存）、页脚指向 doctor/init
- `relative_time()` 为 timefmt.py 移植（just now / Nm / Nh / yesterday / Nd / 日期）
- CLI `ulnclaw status [--all] [--deep]`（`--all` 与默认渲染同为脱敏输出）

<a id="日志查看-文件日志-hermes-cli-logs-py-hermes-cli-subcommands-logs-py-hermes-logging-py-滚动手柄"></a>

#### 日志查看 + 文件日志（`hermes_cli/logs.py` + `hermes_cli/subcommands/logs.py` + `hermes_logging.py` 滚动手柄） — ✅ 核心

- `logs.rs`：查看器移植 —— `LOG_FILES` 注册表（agent/errors/gateway）、`_parse_since`（Ns/m/h/d
  截止时刻）、时间戳/级别/记录器名正则（记录器正则扩展以支持 Rust `::` 目标）、`_matches_filters`（level>= / session 子串 / since /
  组件前缀）、`_read_last_n_lines`（<=1MiB 整读、大文件自尾部倍增分块）、`_read_tail`（带过滤时 20 倍窗口）、`list_logs`（大小 +
  时龄表）、`tail_log` 头部/过滤描述对齐、`_follow_log` 300ms 轮询
- 写入端移植 —— `RotatingFile`（max_bytes x backup_count 移位轮转：agent.log 5MBx3 INFO+、errors.log 2MBx2
  WARNING+、gateway.log 5MBx3 目标过滤）+ `HermesLogFormat`（`YYYY-MM-DD HH:MM:SS,mmm LEVEL [session] target:
  message`）经按文件 layer 接入 tracing
- `COMPONENT_PREFIXES` 适配 ulnclaw 模块路径
- CLI `ulnclaw logs [agent|errors|gateway|list] [-n] [-f] [--level] [--session] [--since] [--component]`
- P325 新增 `GET /api/logs`（不带 `file` 返回清单，带 `file` 返回过滤后的逐文件尾部，支持
  level/component/search/session/since），桌面诊断日志面板新增文件选择器与搜索（对应 hermes `/api/logs`）

<a id="自更新器-hermes-cli-subcommands-update-py-update-cmd-py-git-核心"></a>

#### 自更新器（`hermes_cli/subcommands/update.py` + `update_cmd.py` git 核心） — ✅ 核心

- `update.rs`：`--check` 为 `_cmd_update_check` 移植 —— 分支解析（`--branch` > 当前分支 > master，hermes
  `_resolve_update_branch`）、浅克隆感知（`--depth 1` fetch + 仅比对 SHA 存在性）、默认分支优先 upstream fetch 并回退
  origin、fetch 错误分类（网络 / 认证 / 通用）、比较引用校验、rev-list 落后计数
- 应用路径为 `_cmd_update_impl` git 核心移植 —— 自动
  stash（`--include-untracked`、清理未合并索引、`ulnclaw-update-autostash-<ts>` 命名）、按 origin URL 检测 fork 并自动添加
  `upstream` 远端（`_is_fork` / `_add_upstream_remote`，本地路径 origin 跳过）、`git merge
  --ff-only`（历史分叉只报告不强推）、stash 恢复与冲突指引、old..new 提交日志，随后 `cargo build --release` 作为 Rust 的依赖刷新等价物
- Python 专属机制（venv/pip/npm、Windows 锁、桌面交接、docker/nix、systemd 重启）对编译型 Rust 二进制不适用
- CLI `ulnclaw update [--check] [--branch N] [-y]`
- P324 新增 `GET /api/update/check`（不应用的差距报告）+ `POST /api/update`（就地应用），桌面诊断视图提供更新面板（对应 hermes `/api/hermes/update*`）

<a id="备份与恢复-hermes-cli-backup-py-hermes-cli-subcommands-backup-py"></a>

#### 备份与恢复（`hermes_cli/backup.py` + `hermes_cli/subcommands/backup.py`） — ✅ 核心

- `backup.rs`：完整 zip 备份（hermes `run_backup` —— 适配后的 `_EXCLUDED_DIRS/_SUFFIXES/_NAMES` 排除集、输出 zip
  自排除、进度/错误摘要、`ulnclaw-backup-<ts>.zip` 命名、目录型输出处理），SQLite 经 `sqlite backup()` WAL
  安全快照（`safe_copy_db`，hermes `_safe_copy_db`）+
  `verify_sqlite_integrity`/`is_zeroed_sqlite_file`/`copy_db_and_verify`
- 导入（hermes `run_import` —— `validate_backup_zip` 标记文件校验、`detect_prefix` 含 `.ulnclaw`/`ulnclaw`、防
  zip-slip 的暂存覆盖、`_IMPORT_SKIP_NAMES` 运行时状态保护、`_SECRET_FILE_NAMES` 0600 权限收紧）
- 快速快照（hermes `create/list/restore_quick_snapshot` + `_prune_quick_snapshots` —— manifest.json、防穿越
  id、.db 原子替换、keep=20 修剪、pre-update 用 max_file_size 跳过）
- cron 安全网 `restore_cron_jobs_if_emptied`（统计 state.db `cron_jobs` 表而非 jobs.json）
- pre-update 钩子接入 `ulnclaw update`，`ulnclaw import` 前置 pre-import 快照 + 导入后安全网
- CLI `ulnclaw backup [-o] [-q] [-l]` / `backup list|restore <id>|prune [keep]` / `ulnclaw import <zip>`

<a id="回退链-cli-hermes-cli-fallback-cmd-py-fallback-config-py"></a>

#### 回退链 CLI（`hermes_cli/fallback_cmd.py` + `fallback_config.py`） — ✅ 核心

- `fallback.rs`：运行时链已存在（`[model] fallbacks` 规格 + `agent::with_fallback_specs` / `parse_fallback_spec`）
- 本次补齐管理 CLI —— `list`（主模型 + 编号链，hermes `cmd_fallback_list` 文案）、`add
  <provider:model>`（经同部署比较拒绝主模型自身、拒绝完全重复，供应商大小写不敏感）、`remove <N|provider:model>`、`clear`（TTY 确认，`-y`
  跳过）
- 存储经行级 config.toml 编辑写回（`save_chain`：在 `[model]` 段内替换/插入 `fallbacks = [...]`，保留注释与顺序，文件缺失时创建）
- hermes 交互式选择器以显式规格参数替代（ulnclaw 无 curses 选择器）
- CLI `ulnclaw fallback [list|add|remove|clear] [-y]`

<a id="活跃会话租约-hermes-cli-active-sessions-py"></a>

#### 活跃会话租约（`hermes_cli/active_sessions.py`） — ✅ 核心

- `active_sessions.rs`：跨进程租约注册表 `<home>/runtime/active_sessions.json`，由 `active_sessions.lock` 上的
  flock 保护（hermes `_FileLock`）
- 条目携带 lease_id/session_id/surface/pid + `/proc/<pid>/stat` 启动时刻，PID 复用无法伪造存活（hermes psutil create_time 配对）
- `prune_dead` 在每次变更时回收死进程租约
-
  `try_acquire/release/transfer_active_session`、`release_orphaned_leases`、`active_session_registry_snapshot`、`summarize_holders`（"desktop
  x4, cli, oldest Nh ago"）+ `active_session_limit_message` 文案对齐
- 上限经 `[gateway] max_concurrent_sessions` 配置（0/未设禁用
- hermes 顶层/gateway.* 解析），在 chat REPL 启动时强制执行，租约随 Drop 释放（网关请求路径无状态、不按槽位限制）

<a id="配置管理-cli-hermes-cli-config-py-config-command"></a>

#### 配置管理 CLI（`hermes_cli/config.py` config_command） — ✅ 核心

- `config_cmd.rs`：`show`（面板头 + 路径 + 全量配置，机密键经 `status::redact_key` 脱敏）、`get <key> [--json]`（config.toml 点路径
- 全大写键按 hermes `_is_env_config_key` 经进程环境变量 + `.env` 解析）、`set <key> <value> [--force]`（标量类型推断
  bool/int/float/数组/表/字符串、嵌套表创建、未知段提示与 hermes 对齐
- env 风格键写入 `.env`）、`unset <key>`（config.toml 或 `.env` 行移除）、`path` / `env-path`、`edit`（$EDITOR）
- 存储经 TOML 往返重写（TOML 取代 hermes YAML，注释丢失为已记录的取舍）
- P318 新增原始逃生通道——`GET/PUT /api/config/raw` 逐字读取并原子替换 config.toml（解析校验、保留注释），桌面配置视图提供原始 TOML 对话框（对应
  hermes `/api/config/raw`）
- P320 新增 `/api/env`（仅列文件/进程状态不列值、增删 env 风格键），配置视图提供环境变量管理器（对应 hermes `/api/env`）
- P336 新增 `POST /api/env/reveal`（按键揭示 `.env`/进程环境中的未脱敏值，30 秒窗口限 5 次——对应 hermes env-reveal）与 `GET
  /api/config/defaults`、`GET /api/config/schema`（点路径叶子 + 类型 + 默认值的扁平化模式——精简对应 hermes config
  defaults/schema），桌面 Config 视图提供逐键 👁 揭示按钮与模式参考区块

<a id="环境转储与版本-hermes-cli-dump-py-build-info-py"></a>

#### 环境转储与版本（`hermes_cli/dump.py`、`build_info.py`） — ✅ 核心

- `ulnclaw dump [--show-keys]`：纯文本、可直接粘贴的装机摘要——版本 + git SHA/提交日期、系统、profile、home、模型/provider、含
  `TERMINAL_ENV` 覆盖提示的实际终端 backend、`api_keys:` set/not set/脱敏值并带"仅 shell 存在、`.env` 缺失"告警（托管后端只读 `.env`
  不读登录 shell）、`features:` toolsets / MCP 服务器 / 记忆 provider / gateway 监听+鉴权 / cron 激活-总数 / 技能 /
  检查点，以及非默认 `config_overrides:`
- `ulnclaw version [--no-update-check]`：版本行 + 安装目录/方式 + 复用 `update --check` 机制的实时升级状态
- 无 git 安装回退读取内置 `.ulnclaw_build_sha` 标记（对标 hermes `.hermes_build_sha`）
- P321 新增 `GET /api/ops/dump`（始终脱敏，桌面 Doctor 运维面板——对应 hermes `/api/ops/dump`）

<a id="提示词体积诊断-hermes-cli-prompt-size-py"></a>

#### 提示词体积诊断（`hermes_cli/prompt_size.py`） — ✅ 核心

- `ulnclaw prompt-size [--json]`：测量每次调用的固定负载——系统提示词按四层拆分（基础身份 / 持久记忆 / 环境 /
  易变日期+模型）并给出字符数与字节数、记忆文件体积、工具数 + JSON schema KB、按 schema 从大到小排序的 toolset 清单（回答"想省 token 该关哪个"）、按
  SKILL.md 从大到小排序的已装技能（技能按需加载、不在基础提示词内）
- 与 `Agent::effective_system_prompt` 共用 `agent::DEFAULT_SYSTEM_PROMPT` 及相同构件，数字与实际注入的提示词一致
- P321 新增 `GET /api/ops/prompt-size`（桌面 Doctor 运维面板——对应 hermes `/api/ops/prompt-size`）

<a id="导入其他-agent-配置-hermes-cli-agent-import-py"></a>

#### 导入其他 Agent 配置（`hermes_cli/agent_import.py`） — ✅ 核心

- `ulnclaw import-agent [claude-code|codex] [--source DIR] [--dry-run]
  [--overwrite]`：detect→parse→map→apply，逐项记录 imported/skipped/conflict/error
- claude-code：`CLAUDE.md` → `memory/MEMORY.md` 条目（标题成为上下文前缀、跳过代码块/表格、去重），`.claude.json` +
  `settings.json` 的 `mcpServers` → config.toml `[[mcp.servers]]`（同名冲突保留原配置、机密风格 env 变量剥离并报告），`skills/`
  → `skills/claude-code-imports/`，权限规则以转换后的命令模式报告（ulnclaw 无 allowlist 配置面）
- codex：`AGENTS.md` + `memories/*.md` → 记忆条目，`config.toml [mcp_servers.*]` →
  `[[mcp.servers]]`，`skills/` → `skills/codex-imports/`
- 记忆合并前先备份（`.bak.<ts>`）、迁移预算 2 万字符
- 凭据文件绝不读取，dry-run 不写任何文件

<a id="会话技能标题修复-hermes-cli-sessions-cmd-py-retitle-skills"></a>

#### 会话技能标题修复（`hermes_cli/sessions_cmd.py retitle-skills`） — ✅ 核心

- `ulnclaw sessions retitle-skills [--limit N]
  [--apply]`（默认干跑）：`list_skill_scaffolded_sessions`（首个用户回合匹配 `[IMPORTANT: The user has invoked the`
  脚手架且已有标题的会话）、`describe_skill_invocation` 从捆绑与单技能格式还原用户键入的调用（引号名称、`User instruction:` / `alongside
  the skill invocation:` 提取、摘录接缝切分、空白折叠）、`generate_title_forced` 绕过自动标题开关、`_is_titlelike`
  拒绝命令输出型候选、唯一标题冲突经 `get_next_title_in_lineage` 去重（`base #2`、`#3`……）
- P223 输出对齐：hermes 的 `every title already reflects the user's request.` / `✓ Re-titled N session(s).` 汇总行与完整子命令长说明

<a id="secrets-保险库-agent-secret-sources"></a>

#### Secrets 保险库（`agent/secret_sources/`） — ✅ 核心

- `src/secrets.rs` + `ulnclaw secrets status|sync [--apply]`：外部秘密源在启动时、任何 provider 读取 env
  之前应用（hermes env-loader 钩子）。三个来源，完整复刻 hermes 优先级语义 —— mapped 优先于 bulk、首个声明者胜出、`preserve_existing`
  胜过一切、`override_existing` 可覆盖已有 `.env`/shell 值但绝不覆盖其他来源、引导令牌变量写入保护。`command`：经 `/bin/sh -c` 的任意
  KEY=VALUE 助手（keepassxc-cli / secret-tool / tmpfs cat），硬超时降级为“无值”，stderr 丢弃，1 MiB
  输出上限，支持引号/注释解析。`bitwarden`：Bitwarden Secrets Manager，经 `bws secret list <project> --output json`（托管
  `<home>/bin/bws` 优先于 PATH、`BWS_SERVER_URL` 透传、固定 v2.0.0 自动安装 —— 来自 bitwarden/sdk-sm releases 的
  sha256 校验 zip、zip-slip 防护解包、0755 分阶段安装）。`onepassword`：映射式 `op://vault/item/field` 绑定，经 `op read --
  <ref>` 解析，子进程仅继承最小允许清单 env，空值拒绝写入，单引用失败降级为警告。拉取错误只产生单行警告、绝不致命。TTL
  拉取缓存（`agent/secret_sources/_cache.py` 移植，`src/secrets_cache.rs`）：`<home>/cache/` 下原子 0600 写入（目录
  0700），TTL 为 0 时两层缓存对称关闭，仅完整无错拉取入缓存
- Bitwarden 缓存落盘即 AES-256-GCM **加密**（HKDF-SHA256 密钥由引导令牌派生、缓存键绑定为 AAD、迁移成功后删除旧明文缓存）。交互式安装向导：`secrets
  bitwarden setup|install|status|token|disable`（hermes 五步流程 —— 安装二进制 → 令牌 → 区域 → `bws project list`
  项目选择 → 测试拉取 → 保存配置
- 非 TTY 快速路径要求 `--access-token`/`--server-url`/`--project-id`）与 `secrets onepassword
  setup|status|set|remove|disable`。`secrets bitwarden token` 无需重跑向导即可轮换访问令牌（hermes `cmd_token`：掩码提示或
  `--access-token`、`0.` 形状警告、以新凭据 `bws project list` 先验证后落盘（除非 `--no-verify`）、已配置项目可见性警告、写入 .env
  并清除两层缓存）。未移植：Windows bws 资产路径未测试

<a id="凭证池-agent-credential-pool-py-hermes-cli-auth-py-仪表盘-api-credentials-pool"></a>

#### 凭证池（`agent/credential_pool.py` + `hermes_cli/auth.py` + 仪表盘 `/api/credentials/pool`） — ✅ 核心

- P330 精简移植：`src/credential_pool.rs` —— `<home>/credentials-pool.json` 存储手工登记的按 provider API 密钥条目。网关
  `GET /api/credentials/pool`（按 provider 行，1 基序号、脱敏令牌预览、请求计数）、`POST /api/credentials/pool`（provider
  归一化 + 默认 `key #N` 标签）、`DELETE
  /api/credentials/pool/:provider/:index`（手工条目无再播种源，删除即彻底）。轮换：优先级最高档优先，同档取使用最少者
- 请求计数尽力持久化（原子写盘，无跨进程锁）。`UlncLawConfig::resolve_api_key` 按 配置字面值 > 池条目 > 环境变量 解析，池成员身份即精选信号
- 桌面 Config 视图新增凭证池区块（条目行 + 添加/移除）。已知差异：不从 env/OAuth/配置源自动播种、无抑制/移除步骤注册表、无 OAuth 单例条目、无 `hermes auth` CLI 面，xAI 池代理适配器仍未移植

<a id="iron-proxy-出站防火墙-hermes-egress-agent-proxy-sources-iron-proxy-py"></a>

#### iron-proxy 出站防火墙（`hermes egress`、`agent/proxy_sources/iron_proxy.py`） — ✅ 核心

- `src/iron_proxy.rs` + `src/egress_cmd.rs` + `ulnclaw egress
  install\|setup\|start\|stop\|restart\|reload\|status\|disable\|config` + `/egress` 斜杠命令：托管
  iron-proxy v0.39.0（github.com/ironsh/iron-proxy，Apache-2.0）TLS 拦截式 Docker 沙箱出站代理 —— 沙箱只拿到按 provider
  铸造的代理令牌，守护进程在白名单主机上将其换成真实上游凭据（从守护进程自身环境读取），其余一律拒绝。`install [--force]`：固定版本下载，SHA-256 校验 + 尽力而为的系统
  `gpg` 分离签名验证（临时 keyring
- 签名存在但校验失败则硬失败）。`setup [--tunnel-port N] [--from-bitwarden\|--no-bitwarden] [--rotate-tokens]
  [--restart\|--no-restart]`：openssl CA（4096 RSA、10 年、0600 原子写密钥），为 env/`<home>/.env`/Bitwarden 中的每个已知
  provider 铸造令牌（8 个 bearer + 3 个头部认证 provider —— Anthropic `x-api-key`、Azure `api-key`、Gemini
  `x-goog-api-key` 含 `GOOGLE_API_KEY` 别名归并），重跑 setup 默认保留既有令牌除非交互确认轮换（先备份 mappings），SigV4/GCP 未覆盖
  provider 警告，写 `proxy.yaml`（v0.39 schema：allowlist + secrets 变换，失败关闭 `require: true` + 按 provider
  `match_headers` + 查询匹配，Linux 绑定 docker 网桥（RFC1918 校验、绝不 0.0.0.0）/ Docker Desktop 绑 loopback，默认 SSRF
  拒绝 CIDR（IMDS/环回/RFC1918/CGNAT/v4 映射 v6），loopback bearer 管理 API）+ 0600 `mappings.json`，启用 `[proxy]`
  配置键（绝不静默将 bitwarden 降级为 env），可停止并重启运行中的守护进程。`start`：以最小允许清单 env
  分离式拉起（仅映射的真实密钥、别名镜像、剥离代理链变量、NO_COLOR），每次启动的 nonce + pidfile/nonce 文件（O_EXCL/O_NOFOLLOW/属主校验），PID
  回收防御：/proc environ → cmdline 基名 → ps 回退 + SIGKILL 前 starttime 复核，可选启动时 Bitwarden BSM 刷新（未设
  `proxy.allow_env_fallback` 时失败关闭），5 秒端口监听等待 + 日志尾部诊断。`reload`：经管理 API `POST /v1/reload` 热替换规则集（422
  校验/401 旧密钥处理）。`status [--show-tokens]`/`disable`/`config` 与 `/egress`
  斜杠共用只读快照（二进制/版本/配置/CA/pid/监听/映射/未覆盖）。差异：品牌化 `ULNCLAW_IRON_PROXY_*` 环境变量（hermes 为
  `HERMES_IRON_PROXY_*`）
- 归档解包走系统 `tar`
- CA 主题品牌化
- 仅 Docker 作用域强制（hermes v2026.8.3）

<a id="computer-use-tools-computer-use"></a>

#### Computer Use（`tools/computer_use/`） — ✅ 核心

- `src/computer_use.rs` + `ulnclaw computer-use status|doctor|install`：经 cua-driver 守护进程的后台桌面控制（MCP
  over stdio，hermes `cua_backend.py`）。完整复刻 hermes 工具 schema（capture som/vision/ax、按 SOM 元素索引或坐标的 click
  族、drag、scroll、type、组合键、set_value、wait、list_apps/list_windows/focus_app、cua_browser_* 类型化浏览器透传）。复刻
  hermes 审批语义：capture 与列表类免费，其余动作一律走审批回调，无人值守时失败关闭。惰性共享 MCP
  会话（`start_session`/`end_session`、`set_config` max_image_dimension、光标覆盖层策略含 `--no-overlay` 自动探测 + 默认
  `CUA_DRIVER_RS_TELEMETRY_ENABLED=0`）。`doctor` 驱动 cua-driver 的 `health_report`。P228
  补齐客户端视觉后处理层：每个动作结果的 `screenshot_png_b64` 都会被解码，最长边超过 `[computer_use] max_image_dimension`
  时在客户端降采样（保持宽高比、刷新 `width`/`height`、失败开放）——作为驱动侧 `set_config` 上限的双保险，即使驱动忽略该配置，SOM/视觉载荷也有界。未移植：macOS
  TCC `permissions` 授权流程、嵌入式守护进程/socket 模式、上下文级截图驱逐（v2026.8.3 检出中无参考实现——`tools/` 目录缺失
- 不凭空发明语义）

<a id="插件系统-hermes-cli-plugins-py-agent-shell-hooks-py"></a>

#### 插件系统（`hermes_cli/plugins.py`、`agent/shell_hooks.py`） — ✅ 核心

- `src/plugins.rs` + `ulnclaw plugins list|install|update|remove|enable|disable|accept-hooks`：以
  shell-hook 线协议实现的 hermes 插件架构 Rust 原生化移植（静态二进制无法导入 Python 插件）。目录插件位于
  `<home>/plugins/<name>/plugin.toml`（manifest：hooks + `[[tools]]`）
- 工具以 `plugin__<name>__<tool>` 注册，作为子进程运行、stdin 收 `{"tool", "arguments"}` JSON。配置式 shell 钩子 `[hooks]
  <event> = ["cmd"]`，复刻 hermes 首次使用同意机制（`shell-hooks-allowlist.json`、`auto_accept` /
  `ULNCLAW_ACCEPT_HOOKS`）。完整复刻 hermes `VALID_HOOKS` 目录（23 个事件）
- 核心触发 hermes 运行期实际发出的全部 13 个：`pre_tool_call`（block
  决定在审批前否决）、`post_tool_call`、`transform_llm_output`、`on_session_start`/`on_session_end`/`on_session_reset`（`/new`）/`on_session_finalize`（REPL
  退出）、`pre_llm_call`（context 响应追加进当轮用户消息，hermes turn-context
  语义）、`post_llm_call`、`pre_api_request`/`post_api_request`/`api_request_error`（包裹每次 provider
  调用）、`pre_gateway_dispatch`（在白名单门控之前 skip/rewrite 平台消息）
- 其余 10 个在 hermes v2026.8.3 中也仅存于目录。`ulnclaw hooks list|test|revoke|doctor`（hermes `hooks`
  CLI）检查同意状态、以默认载荷触发、逐个探测已同意的钩子。P326 将同意面暴露为 `GET /api/ops/hooks`（逐命令 consented/pending/unknown-event
  状态 + 有效事件 + 白名单概览）、`POST /api/ops/hooks/accept-all` 与 `POST /api/ops/hooks/revoke`，桌面插件视图呈现（对应
  hermes `/api/ops/hooks`）。P178 增加 hermes git 安装生命周期：`plugins install <url|owner/repo[/subdir]>
  [--force] [--enable|--no-enable]`（非交互 git 浅克隆 + 60 秒上限，GitHub 浏览器 URL/`#subdir`/`.git/`
  边界标识符解析，子目录防穿越，从 plugin.toml 或 plugin.yaml 发现名称，目标净化后落 `<home>/plugins/`，交互式启用提问）、`plugins update
  <name>`（在已安装克隆内 git pull）与 `plugins remove <name>`（删除目录）。P234
  移植钩子输出溢出防护（`tools/hook_output_spill.py`）：钩子注入的 `context` 会附加到后续每次 API 调用，超过 `[hooks.output_spill]
  max_chars`（默认 1 万字符）的块会溢出落盘到 `<home>/hook_outputs/<session>/<uuid>.txt`，提示词内只留头/尾预览 + 路径
- I/O 失败降级为带 "spill write failed" 的有界预览，绝不拖垮本轮（enabled/max_chars/preview_head/preview_tail/directory
  可配，会话 id 做路径净化）。P351 将市场管线暴露为 `/api/dashboard/plugins*` + `/api/dashboard/agent-plugins*` +
  `/api/dashboard/plugin-providers`（活动仪表盘插件清单 + 重新扫描、基于精选 `plugin-hub/index.json` +
  本地目录候选的合并市场载荷（安装/更新/移除）、持久化到 `[plugins]` 的记忆/上下文 provider 选择、`dashboard.hidden_plugins` 可见性开关——对应
  hermes 插件市场）
- 逐插件静态资源托管（`/dashboard-plugins/*`）仍未移植。hermes 的 Python 插件导入、entry-point 包与 provider 注册未移植

<a id="消息平台网关-gateway-platforms"></a>

#### 消息平台网关（`gateway/platforms/`） — ✅ 核心

`src/messaging.rs` —— hermes 平台网关架构运行于 `ulnclaw gateway` 内：适配器将入站聊天消息归一化为
`MessageEvent`，每个聊天一个会话（`platform-<name>-<chat>`，经 `create_named_session`）承载对话连续性，回复经平台送回并按 hermes
风格分块。三十个自包含适配器——十九个长驻循环：

**Telegram** —— Bot API 长轮询 getUpdates/sendMessage、clarify 内联键盘 + callback_query 点按路由

**Discord** —— Gateway v10 websocket IDENTIFY/心跳/MESSAGE_CREATE/INTERACTION_CREATE + REST 发送、clarify
  嵌入+按钮 + 交互回调路由

**Slack** —— Socket Mode events_api 信封 + chat.postMessage、assistant.threads.setStatus assistant
  线程输入状态 —— 默认 "is thinking..."、30 秒后切换 "still working… (Xm YYs)" 耗时心跳、`typing_status_text` 覆盖、Block
  Kit clarify 按钮 + interactive 信封路由

**Signal**
- signal-cli HTTP 守护进程：SSE 入站 + keepalive/闲置健康重连、JSON-RPC 2.0 出站 + 限流重试、Note-to-Self
  提升与出站回声抑制、`group_allowed_users` 群组门控（`*` 通配）+ require-mention 过滤、附件经 `getAttachment` base64 + mime
  嗅探 + ADTS→m4a ffmpeg 重封装、`MEDIA:` 回复以 `base64Attachments` 发送
- `[messaging.signal]` 或 SIGNAL_HTTP_URL/SIGNAL_ACCOUNT

- 微信（经腾讯 iLink Bot API 的微信个人号：长轮询 getupdates + 持久化 sync-buf 断点续传、消息 id +
  内容指纹双重去重、DM/群组准入策略（pairing/allowlist/open/disabled）映射到 ulnclaw 白名单∪配对门控、磁盘持久化的按对端 context_token 回显存储
  + 会话过期去令牌回退发送、双向 AES-128-ECB 加密 CDN 媒体（图片/视频/文件/语音，SSRF 主机白名单）、2000 字符 markdown 感知分块 + 易复制行折行 +
  文本防抖批处理、getconfig 输入指示票据、`ulnclaw weixin login` 二维码登录
- `[messaging.weixin]` 或 WEIXIN_ACCOUNT_ID/WEIXIN_TOKEN

**QQ**
- 官方 QQ Bot API v2：WebSocket 网关 Hello/Identify/Resume/心跳 + hermes 关闭码语义（4004 刷新令牌、4006/4007/4009
  重置会话、4008 限流退避、4914/4915 停止重连），C2C/群 @/频道/频道私信事件 + 300 秒消息去重，markdown（msg_type 2）或去格式纯文本回复、被动回复
  msg_id 挂载 + `msg_seq` 生成，出站媒体 8 MB 以下走 base64 内联、更大走三步分块上传（upload_prepare → 预签名 COS PUT +
  upload_part_finish → complete，含日配额 40093002 与分片重试 40093001 处理），语音优先取 `asr_refer_text`、否则原音频入 `[stt]`
  管道，引用消息（message_type 103）上下文合并，INTERACTION_CREATE 确认 + 内联键盘（执行审批 ✅ 允许一次 / ⭐ 始终允许 / ❌ 拒绝 回调按钮，走
  `send_exec_approval` 契约，按钮点按经操作者授权后回流审批网关——私聊须与聊天用户一致、群/频道点按须通过白名单 ∪ 配对准入门控
- 更新确认 ✓/✗ 键盘将 `y`/`n` 应答原子写入 `.update_response` 文件
- 扫码配置引导已移植（`ulnclaw qq login` —— hermes `qr_register`：`q.qq.com` 门户 `create_bind_task` /
  `poll_bind_result` API（`QQ_PORTAL_HOST` 可覆盖）、终端二维码 + 链接展示、600 秒默认时限内 2 秒轮询、二维码过期最多刷新 3
  次、`client_secret` 本地 AES-256-GCM 解密（IV ‖ 密文 ‖ tag 布局，密钥不出 CLI）
- 凭证持久化于 `<home>/qq/credentials.json` 并作为配置解析的末级回退
- P220 已移植完整设置向导（`ulnclaw qq setup` —— hermes `_setup_qqbot`）：扫码/手动凭证二选一、DM
  安全策略选择（pairing/open/allowlist，含自助添加与逗号列表输入）并持久化到 `[messaging.qq]`（`dm_policy` + `allow_from`，合并式
  TOML 编辑保留其余配置）、home 频道选择持久化为 `<home>/.env` 的 `QQBOT_HOME_CHANNEL`
- 密钥输入经 crossterm raw 模式隐藏回显）
- `[messaging.qq]` 或 QQ_APP_ID/QQ_CLIENT_SECRET

**元宝**
- 腾讯元宝 App 机器人：WebSocket 网关会话经 HMAC-SHA256 `sign-token` HTTP 握手引导（北京时间 +08:00 时间戳、签名令牌缓存）、手写
  protobuf 线格式编解码（`src/yuanbao_proto.rs`：ConnMsg 信封、AUTH_BIND/BIND_ACK、ping、push 回执、30
  秒私聊/群组心跳）、入站推送解码 + 每发送者 1.5 秒防抖、DM/群组准入策略（pairing/allowlist/open/disabled）映射到白名单∪配对门控、markdown 感知
  4000 字符分块回复经 WS 发送（send-c2c/send-group）、复刻 hermes
  不重连关闭码（4012/4013/4014/4018/4019/4021）、出站媒体已移植（`MEDIA:` 回复路径经 `genUploadInfo` 临时 COS 凭证 + 签名全球加速 PUT
  上传——hermes `yuanbao_media.py`——再以 TIMImageElem（解析 PNG/JPEG/GIF/WebP 尺寸、md5 uuid、TIM `image_format`）或
  TIMFileElem 分发，说明文字以 TIMTextElem 追加）
- 表情包已移植（hermes `yuanbao_sticker.py`：59 条内置贴纸表，精确/包含/描述/模糊四级查找，入站 TIMFaceElem 渲染为 `[emoji:
  <名称>]`，`STICKER:<名称>` 回复标签以 TIMFaceElem（`data` JSON 载荷）发送目录贴纸，proto `index` 字段 9）
- 入站媒体解析/下载已移植（hermes ExtractContentMiddleware + MediaResolveMiddleware：图片/文件引用渲染为
  `[image|ybres:RID]` / `[file:name|ybres:RID]` 锚点，经 `/api/resource/v1/download` resourceId 交换（401
  强制刷新令牌重试一次）、限大小下载入媒体缓存成为附件，锚点替换为本地路径，24 小时 resourceId 缓存 + 满载淘汰最旧 25%，复刻 hermes 占位符过滤
- 引用/观察媒体回填已移植（适配器侧消息内容缓存 + 群组观察缓冲区替代 hermes 转写库 —— ulnclaw 转写不保存平台消息 id：引用消息中的已本地锚点原样注入、残留
  `[kind|ybres:RID]` 锚点重新解析，无引用的群聊回合按最新优先从最近 50 条消息回填至多 12 个引用并按 rid 去重，另注入 `[Replying to: "..."]`
  引用提示（500 字符截断））
- 微信转发聊天记录（elem_type 1009）深解析入提示 —— TIMCustomElem ext_map 中的 ForwardMsgData protobuf、单条 1000
  字符上限、`[kind|ybres:RID]` 媒体锚点走同一下载管线、`用户附言：` 附言尾注，锚点替换按 resourceId 匹配）
- `[messaging.yuanbao]` 或 YUANBAO_APP_ID/YUANBAO_APP_SECRET/YUANBAO_BOT_ID

**邮件**
- hermes `platforms.email` 插件：IMAP 隐式 TLS INBOX 轮询（UID SEARCH UNSEEN + UID FETCH RFC822）+ 启动时以全量 UID
  种子的有界已见 UID 集合，SMTP 回复带 `Re:` 主题线程（`In-Reply-To`/`References`、生成的 Message-ID）与 multipart `MEDIA:`
  附件，RFC 2047 头部解码，text/plain 优先 + HTML 去标签回退的正文提取，noreply/批量邮件过滤，白名单门控生效时对 `Authentication-Results` 做
  SPF/DKIM/DMARC From 域认证且失败关闭（GHSA-rxqh-5572-8m77 对齐
- `require_authenticated_sender = false` / EMAIL_TRUST_FROM_HEADER 可退出）
- `[messaging.email]` 或 EMAIL_ADDRESS/EMAIL_PASSWORD/EMAIL_IMAP_HOST/EMAIL_SMTP_HOST

**Mattermost**
- hermes `platforms.mattermost` 插件：REST v4 + WebSocket 事件流——`authentication_challenge` WS 认证 + 30 秒
  ping + 指数退避重连（401/403 永久错误停止重连），`posted` 事件 + 帖子 id 去重 + 频道类型映射，`allowed_channels` 白名单 +
  `require_mention` @机器人 门控（`free_response_channels` 豁免）+ 提及剥离，线程根回复（`reply_mode =
  "thread"`），鉴权文件下载入媒体缓存、出站 `MEDIA:` 标签 multipart 上传，4000 字符帖子 + `disable_mentions` props，REST 输入指示
- `[messaging.mattermost]` 或 MATTERMOST_URL/MATTERMOST_TOKEN

**Matrix**
- hermes `platforms.matrix` 插件核心，直连 Client-Server API 无 SDK：密码登录或访问令牌（`whoami` 发现），长轮询 `/sync` +
  首轮同步回灌跳过，`m.room.message` 文本/notice 处理 + 引用回退剥离，`allowed_users`/`allowed_rooms` 门控 + 群组房间
  @提及要求（显示名/@user-id 模式）+ `free_response_rooms` 豁免，mxc:// 媒体下载（优先鉴权）与 `MEDIA:` 回复上传，事务性 `m.text` 发送按
  `max_message_length` 分块（默认 16000，钳制 500..65535）
- 不支持端到端加密——`m.room.encrypted` 房间跳过并告警
- `[messaging.matrix]` 或 MATRIX_HOMESERVER + MATRIX_ACCESS_TOKEN 或 MATRIX_USER_ID/MATRIX_PASSWORD

**钉钉**
- hermes `platforms.dingtalk` 插件，无 Python SDK 手写 Stream Mode：`POST /v1.0/gateway/connections/open`
  网关握手 + `/v1.0/im/bot/messages/get` WebSocket CALLBACK 帧 + ACK
  回复，文本/富文本/语音识别/文件名/文档卡片提取，`allowed_users`/`allowed_chats` 门控 + `require_mention`（hermes 钉钉默认 false）+
  `isInAtList` + 正则 `mention_patterns` 唤醒词 + `free_response_chats`，`downloadCode` 媒体经
  `/v1.0/robot/messageFiles/download` + 缓存 OAuth2 令牌解析，sessionWebhook markdown 回复（20000 字符分块、hermes
  markdown 归一化、5 分钟过期余量、LRU-500 缓存），表情回应经 `POST /v1.0/robot/emotion/reply|recall`（入站即发
  🤔Thinking、最终回复后一次性换成 🥳Done、按聊天幂等——hermes `_send_emotion`/`_fire_done_reaction`）
- 配置 `card_template_id` 时回复经 card_1_0 AI 流式卡片送达（创建 STREAM 卡片实例 → 投递至
  `dtv1.card//IM_GROUP.<conversationId>` / `IM_ROBOT.<senderStaffId>` 空间 → 一次性全量 `streaming` 更新 +
  `isFinalize`，任何失败回退 webhook markdown，`robot_code` 默认回退 `client_id`，缺 `senderStaffId` 的 DM
  卡片跳过——hermes `_create_and_stream_card`）
- `[messaging.dingtalk]` 或 DINGTALK_CLIENT_ID/DINGTALK_CLIENT_SECRET/DINGTALK_CARD_TEMPLATE_ID/DINGTALK_ROBOT_CODE

**企业微信**
- hermes `platforms.wecom` 插件：企微 AI Bot WebSocket 网关——`aibot_subscribe` 握手、30 秒应用层
  ping、`aibot_msg_callback` 事件 + msgid 去重、mixed/text/voice/appmsg/quote 提取 + 首部
  @提及剥离、`dm_policy`/`group_policy`（pairing/allowlist/open/disabled）门控、base64 或 URL 入站媒体 + 企微
  AES-256-CBC `aeskey` 文件方案（key 即 IV、PKCS#7）、markdown 回复经绑定入站 req_id 的 `aibot_respond_msg`（群聊必需）+
  `aibot_send_msg` 主动私聊回退、三步 `aibot_upload_media_*` 分块上传（512 KiB 分片、md5、100 片上限）原生附件
- 入站文本批处理合并 WeCom 客户端约 4000 字符切块——按会话作用域去抖（0.6 秒静默期，最新块达 3900 字符切分阈值时延长至 2.0 秒
- `WECOM_TEXT_BATCH_DELAY_SECONDS`/`WECOM_TEXT_BATCH_SPLIT_DELAY_SECONDS`，0 关闭），语音与媒体立即分发（hermes
  `_enqueue_text_event`/`_flush_text_batch`）
- `[messaging.wecom]` 或 WECOM_BOT_ID/WECOM_SECRET

**Home Assistant**
- hermes `platforms.homeassistant` 插件：WebSocket API `/api/websocket` 认证握手（`auth_required` → `auth` →
  `auth_ok`）+ `subscribe_events` state_changed 事件流、ping/pong 保活与 hermes 5/10/30/60
  秒重连调度，默认关闭的事件过滤（`watch_domains`/`watch_entities`/`watch_all` + `ignore_entities`）+ 按实体冷却（默认 30
  秒），按域的人类可读格式化（climate/sensor/binary_sensor/light/switch/fan/alarm_control_panel，通用回退），事件投递到合成频道
  `ha_events`，回复经 REST API 以持久通知送达（`persistent_notification/create`、4096 字符分块、REST 独立发送以避免与事件监听竞争同一
  WS）
- `notify/notify` 进程外独立发送器已移植：只要配置 HASS_URL + HASS_TOKEN（即便未启用实时适配器）即注册仅凭凭证的发送器，向
  `/api/services/notify/notify` 投递 `{message, target}`（hermes `_standalone_send`）
- 实时适配器启动后以其持久通知发送器覆盖该槽位
- `[messaging.homeassistant]` 或 HASS_TOKEN/HASS_URL

**WhatsApp** —— hermes `platforms.whatsapp` 插件，Baileys HTTP 桥传输 + 完整桥进程监督

- ：Baileys 桥（`scripts/whatsapp-bridge/bridge.js` + `package.json`）编译期内嵌于二进制，运行时同步到 `<home>/scripts/whatsapp-bridge/`
- `src/whatsapp_bridge.rs` 复刻 hermes adapter 的 connect 生命周期——PATH 解析 `node`/`npm`、以 `package.json`
  哈希印章（`node_modules/.hermes-pkg-hash`）门控的 `npm install`、pidfile 复核（`bridge.pid` 记录 PID + 内核启动时间，回收的
  PID 永不被误杀）与仅 LISTEN 状态端口占用者的陈旧桥清理、以 `node bridge.js --port N --session DIR --mode MODE` 拉起且
  stdout/stderr 追加写入 `bridge.log`、两阶段就绪等待（HTTP 起来 ≤15 秒，其后 `status == "connected"` 再 ≤15
  秒，否则告警继续）、以及接管已运行桥时的 `scriptHash` 陈旧握手（`/health` 上报桥自身源码哈希）。自动拉起（`auto_spawn`，默认 true
- `WHATSAPP_AUTO_SPAWN`）仅在 `bridge_url` 指向 `127.0.0.1:<bridge_port>` 时生效——其他 URL 保持外部桥客户端行为。入站：等待
  `/health` `connected`（QR 配对状态持续提示并指向 `bridge.log`、捕获 `botJid` 用于提及检测），每秒轮询
  `/messages`，丢弃广播伪聊天与自身发送回声，而 self-chat 模式的 owner 消息（桥 `fromOwner`
  标记，`WHATSAPP_FORWARD_OWNER_MESSAGES`）以 `[owner reply]` 前缀（hermes `_OWNER_REPLY_PREFIX`）通过准入，DM
  白名单∪配对门控 + 群组 `allowed_channels`/require-mention 门控（`free_response_channels` 豁免），入站媒体（桥缓存的绝对路径或
  URL）下载入内容寻址媒体缓存（桥 mime 回退），即发即忘 `/read` 已读回执，回复经 `/send`（4000 字符分块）、`MEDIA:` 标签经
  `/send-media`（按扩展名推导 mediaType）
- 原生投票经 `/send-poll`（hermes `send_poll`——2–12 个有效选项的 clarify
  提问渲染为单选投票，所选选项以纯文本入站并解析待决提问，编号文本回退）、位置图钉经 `/send-location`（hermes `send_location`）、消息编辑经
  `/edit`（hermes `edit_message`，传输原语）
- 入站投票/位置以文本事件呈现
- `[messaging.whatsapp]` 或
  WHATSAPP_BRIDGE_URL/WHATSAPP_BRIDGE_PORT/WHATSAPP_SESSION_PATH/WHATSAPP_AUTO_SPAWN/WHATSAPP_ALLOWED_USERS

**IRC**
- hermes `platforms.irc` 插件：rustls 零依赖 TCP 客户端——复刻 hermes 注册序列（PASS/NICK/USER、30 秒等待 RPL_WELCOME、可选
  NickServ IDENTIFY、JOIN），433 昵称冲突按递增后缀重试，PING/PONG 保活，CTCP ACTION 折叠（`/me` → `* nick 文本`），频道入站需
  `nick:`/`nick,`/`nick ` 寻址（派发前剥离），白名单为可选——留空即允许所有人（hermes IRC 语义，昵称无身份可言），回复去 markdown（IRC 变体：图片 →
  url、链接 → `文本 (url)`）并按 510 字节行限制分块 + 0.3 秒防洪停顿
- `[messaging.irc]` 或 IRC_SERVER/IRC_PORT/IRC_NICKNAME/IRC_CHANNEL

**ntfy**
- hermes `platforms.ntfy` 插件：HTTP 流式订阅 `GET /<topic>/json?poll=false` NDJSON，2/5/10/30/60 秒重连退避（流存活
  ≥ 60 秒后重置）、401/404 致命即停，Bearer 或 Basic 令牌认证，消息 id 去重（300 秒窗口、1000 条），`hermes-agent` 回声标签防循环，复刻
  hermes 可信频道身份模型（user_id = 主题
- 发布者可控的 title 永不作为授权依据），回复 POST 至发布主题（带回声标签 + 可选 `X-Markdown`），4096 字符上限
- `[messaging.ntfy]` 或 NTFY_TOPIC/NTFY_SERVER_URL/NTFY_TOKEN

**SimpleX**
- hermes `platforms.simplex` 插件：`simplex-chat` 守护进程 WebSocket 客户端——corrId 关联命令帧（`hermes-`
  前缀回声过滤）、联系人请求自动接受（`/accept`）、`rcvFileDescrReady` → 即发即忘 `/freceive`、`newChatItems` 入站（过滤自发消息 + 仅
  `rcvMsgContent`），DM 白名单按联系人 id 或显示名 ∪ 配对，`SIMPLEX_GROUP_ALLOWED` 群组门控（`*` 通配），守护进程本地媒体入缓存 +
  `rcvFileComplete` 延迟投递语音，0.8 秒静默期文本批处理，回复经 `@<id>` DM 与结构化 `/_send #<id> json`
  群组格式（按扩展名区分语音/文档），8000 字符分块
- `[messaging.simplex]` 或 SIMPLEX_WS_URL/SIMPLEX_ALLOWED_USERS

- 外加十一个挂载于网关的 webhook 平台（WhatsApp Cloud、Microsoft Graph 变更通知、通用 webhook 平台、BlueBubbles、飞书、Twilio
  SMS、Microsoft Teams、LINE、Google Chat、Raft wake 端点与 A2A agent 服务，详见下文）。复刻 hermes
  配对语义：每个平台均受白名单门控，空白名单失败关闭并记录待添加的 id。交互式配对码（hermes `gateway/pairing.py` 移植，`src/pairing.rs`）：未授权发送者收到
  8 位 CSPRNG 配对码（存储为加盐 SHA-256、1 小时过期、每平台至多 3 个待审、每用户每 10 分钟一次请求、连续 5 次审批失败锁定平台 1 小时）
- `ulnclaw pairing list|approve|revoke|clear-pending` 管理授权，获批用户与白名单在认证门控处取并集（`[messaging] pairing =
  true` 默认开启）。媒体附件（hermes media-cache 管道移植，`src/media_cache.rs`）：入站 Telegram
  photo/document/video/audio/voice（getFile 下载、照片取最大尺寸）、Discord `attachments`、Slack `files`（bot bearer
  下载）按内容寻址缓存于 `<home>/media-cache/`（SHA-256 命名、hermes mime→ext 表、25 MB 上限），以路径引用 +
  vision_analyze/video_analyze/read_file 提示交付 agent（hermes 文本回退语义）
- 出站回复中的 `MEDIA:<路径>` 标签在 Telegram（sendPhoto/sendDocument）、Discord（multipart）与 Slack（现代
  `files.getUploadURLExternal` → PUT → `files.completeUploadExternal` 流程）转为原生上传，纯媒体入站消息无需文本即可流转

**WhatsApp Cloud** —— hermes `whatsapp_cloud.py` 移植，`src/webhook_platforms.rs`

：网关挂载 `/webhooks/whatsapp`，Meta 验证握手（hub.challenge 回显）、原始请求体 `X-Hub-Signature-256` HMAC 校验，文本 +
image/document/audio/video/sticker 入站走同一白名单∪配对 + 插件门控（入站媒体经 Graph `/media` 对象下载、按 Meta
分型大小上限入内容寻址缓存、caption 作为消息文本），回复经 Graph API 分块发送，另支持两步式原生媒体发送（`/media` multipart 上传 → media-id 消息

**Microsoft Graph 变更通知接入** —— hermes `msgraph_webhook.py` 移植

- ：`/webhooks/msgraph` validationToken 回显 + 必填 clientState 校验，通知以内部事件呈现并复刻 hermes 语义——回执去重（`id:<通知
  id>` 台账、默认 5000、FIFO 淘汰）、`accepted_resources` 资源过滤（精确/子路径/`prefix*` 通配）、提示词渲染（`prompt` 自定义
  `{dotted.path}` 模板，否则排序 JSON 美化转储截断 4000 字符）、`msgraph:<subscriptionId>` 聊天作用域 + 回执键或 sha1 消息
  id、hermes 状态码（202 接受/去重、403 整批鉴权失败、400 格式错误/资源未接受、413 超限）
- 未移植：`notification_scheduler` 钩子（hermes 侧同样无消费者）、源 CIDR 白名单 + 独立健康端点（hermes 独立 aiohttp 服务器，ulnclaw
  挂载共享网关）。通用 webhook 平台（hermes `webhook.py` 移植）：`[messaging.webhook]` 路由挂载于
  `/webhooks/hook/<name>`，多方案签名校验（Svix `svix-*` 头 + base64 `whsec_` 密钥、GitHub
  `X-Hub-Signature-256`、GitLab `X-Gitlab-Token`、时间戳绑定的通用 V2 且禁止降级 V1、旧版 V1、测试用 `INSECURE_NO_AUTH`），300
  秒重放窗口，每路由固定窗口限流（默认 30 次/分钟），投递 id 幂等（`X-Webhook-Delivery-Id` 或 `svix-id`，1 小时
  TTL），头部事件过滤（`X-Webhook-Event`/`X-GitHub-Event`/`X-Gitlab-Event`），`{event}`/`{body}`
  提示词模板，投递目标（`log`/`telegram`/`discord`/`slack`/`whatsapp_cloud`）与 `deliver_only` 零 LLM 推送

**BlueBubbles iMessage 桥** —— hermes `bluebubbles.py` 移植

- ：`[messaging.bluebubbles]` 在网关挂载 `/webhooks/bluebubbles`，密码认证（查询参数 —— BlueBubbles webhook 无法发送自定义头
  —— 或 `x-password`/`x-guid`/`x-bluebubbles-guid` 头），JSON 载荷 + 表单编码回退，复刻 hermes 事件门控（仅
  `new-message`/`message`/`updated-message`
- 自己发出的消息与 tapback 反应 2000–2005/3000–3005 静默确认），chat-GUID 解析经 LRU-500 缓存 + 严格 `chatIdentifier`
  匹配（不做参与者回退 —— hermes #24157）并支持 v1.9+ `chats[0]` 提取，附件下载入内容寻址媒体缓存，回复按段落分块（4000 字符上限、地址目标经 `chat/new`
  建聊），multipart 附件发送，启动时 ping + server-info + 幂等 webhook 注册

**飞书/Lark** —— hermes `platforms.feishu` 插件，双传输——`connection_mode = "websocket"` 为 hermes 默认

- ：websocket 模式原生运行 lark_oapi 长连接（`src/feishu_ws.rs`：`POST /callback/ws/endpoint` AppID/AppSecret
  握手返回连接 URL（`device_id`/`service_id` 查询参数）+ ClientConfig、手写 protobuf `Frame` 编解码、按 PingInterval（默认
  120 秒）发送 CONTROL ping 并支持 pong 载荷 ClientConfig 动态调参、按 `message_id`/`seq` 重组拆分 DATA 包（5 秒
  TTL）、事件帧免令牌/签名校验分发（SDK `_do_without_validation`）并以原帧回显 + `biz_rt` 头与 `{"code":200}`
  载荷确认、抖动重连——ReconnectNonce 30 秒抖动、ReconnectInterval 120 秒、ReconnectCount −1 = 无限重试）
- `connection_mode = "webhook"` 保留网关挂载 `/webhooks/feishu`，验证令牌先于 challenge 回显校验、`encrypt_key` 配置时启用
  SHA-256 webhook 签名（`timestamp+nonce+encrypt_key+body`）、事件 id 去重、`im.message.receive_v1` 文本 +
  提及占位符归一化（`@_user_N` → `@名称`）、图片/文件/音频/富文帖处理（租户令牌资源下载入媒体缓存）、群组 require-mention 门控，文本回复经
  `/open-apis/im/v1/messages`（chat_id/open_id 接收端解析）、`MEDIA:` 标签走图片/文件上传
- 卫星处理器：`drive.notice.comment_add_v1` 文档评论 agent（hermes `feishu_comment.py` 移植——事件解析 +
  自回/接收者/通知类型过滤、`feishu_comment_rules.json` 三级访问规则（精确 `docType:token`/`wiki:token` 键 > 通配 `*` >
  顶层，enabled/policy/allow_from 逐字段回退，allowlist∪pairing 策略 + `feishu_comment_pairing.json` 授权，mtime
  热加载）、agent 工作期间 OK 表情回应、文档元信息 + 评论 batch_query 并行拉取、全文档 vs 局部评论线程时间线组装（hermes 窗口化选取）+ 评论内文档链接提取与
  wiki 节点解析、hermes 提示词构建、按文档 agent 会话（`comment-doc:<file_type>:<file_token>`）、4000 字符分块回帖（1069302
  错误回退全文档评论））与 `vc.bot.meeting_invited_v1` 会议邀请（hermes `feishu_meeting_invite.py` 移植——解析为入会提示词，以 DM
  形式经正常管道分发给邀请者）
- 表情回应已移植（agent 工作期间经 `/open-apis/im/v1/messages/<id>/reactions` 贴 Typing 徽章——完成即移除、失败换 CrossMark，`FEISHU_REACTIONS` 默认开启
- 本机器人所发消息上的用户表情经机器人作者校验后以 `reaction:<added|removed>:<emoji>` 合成事件路由，bot/app
  来源的表情丢弃以断开生命周期回环——hermes `on_processing_start`/`on_processing_complete`/`_handle_reaction_event`）
- 交互式卡片动作已移植（hermes `send_exec_approval` / `send_update_prompt` +
  `_on_card_action_trigger`）：执行审批卡片（橙色标题栏、3000 字符围栏命令预览、✅ Allow Once / ✅ Session / ✅ Always / ❌
  Deny，smart deny 时隐藏会话档）走 `send_exec_approval` 契约，更新确认卡片（✓ Yes / ✗ No），webhook 模式卡片回调经操作者授权（白名单 ∪
  配对、回调聊天匹配）后回流审批网关解锁，并以内联已解析卡片（`P2CardActionTriggerResponse` JSON）响应，更新确认应答原子写入 `.update_response`
- 适配：按钮值携带无状态 `approve:<session>:<decision>` 载荷取代 hermes 内存审批状态，WebSocket 模式与 lark SDK 一致丢弃 CARD
  帧（卡片动作仅 webhook 模式可用），非审批卡片动作以合成 COMMAND 事件路由（`/card <tag> <json>` 经白名单∪配对准入门控、15 分钟令牌去重窗口，派发分离执行使
  webhook 立即返回 `{}`
- 动作值 JSON 按键排序而非 hermes 插入序）
- 已读回执（`im.message.message_read_v1`）双传输均显式忽略（hermes `_on_message_read_event`）
- `[messaging.feishu]`（`connection_mode` websocket|webhook、`domain` feishu|lark）或
  FEISHU_APP_ID/FEISHU_APP_SECRET/FEISHU_VERIFICATION_TOKEN/FEISHU_ENCRYPT_KEY/FEISHU_CONNECTION_MODE/FEISHU_DOMAIN

**Twilio SMS** —— hermes `platforms.sms` 插件——`SMS_WEBHOOK_PORT` 上的独立 aiohttp webhook 服务器改为网关挂载路由

- ：`/webhooks/twilio` 接收表单编码的 Twilio 回调，`X-Twilio-Signature` HMAC-SHA1 校验（url + 按键排序的 key/value
  拼接、base64 摘要、按 Twilio 文档做默认端口变体回退、未配置 `SMS_WEBHOOK_URL` 时失败关闭，除非
  `SMS_INSECURE_NO_SIGNATURE=true`），64 KiB
  请求体上限、自有号码回声抑制、白名单∪配对准入（`SMS_ALLOWED_USERS`/`SMS_ALLOW_ALL_USERS`，配对码经 SMS 送达），回复去 markdown 并按 1600
  字符分块经 Twilio Messages REST API 发送（HTTP Basic 认证）
- MMS 媒体处理未移植
- `[messaging.sms]` 或 TWILIO_ACCOUNT_SID/TWILIO_AUTH_TOKEN/TWILIO_PHONE_NUMBER/SMS_WEBHOOK_URL

**Microsoft Teams** —— hermes `platforms.teams` 插件——`microsoft-teams-apps` Python SDK 改为原生 Bot
  Framework 协议

- ：`/webhooks/teams` 接收 `message` 活动，活动 id 去重（300 秒窗口）、`<at>` 提及 HTML
  剥离、会话类型映射（personal/groupChat/channel）、发送者门控（aadObjectId 或 id）走白名单∪配对，附件入站复刻 hermes 跳过规则（html
  镜像、自适应/英雄卡片）——文件同意 `downloadUrl`、`image/*` 与 bearer 鉴权的 `contentUrl` 下载入媒体缓存——回复经 OAuth2
  客户端凭证令牌（`login.microsoftonline.com/<tenant>/oauth2/v2.0/token`、`https://api.botframework.com/.default`
  作用域、带缓存）POST 至 `{serviceUrl}v3/conversations/<conv>/activities` markdown 活动
- serviceUrl 按 hermes Bot Framework 主机白名单校验（SSRF 防护）、会话 id 按 Bot Framework 字符集校验
- 执行审批以 AdaptiveCard v1.4 卡片呈现（Action.Execute Allow Once/Allow Session/Always Allow/Deny
  按钮），`adaptiveCard/action` invoke 点按在默认拒绝的白名单门控下（`TEAMS_ALLOWED_USERS`
  为空时拒绝所有点按）解析会话阻塞审批并以替换卡片回显决定，`/approve [all] [session|always]` / `/deny [all]` 聊天命令在所有平台可用（hermes
  斜杠命令语义）
- 摘要写入路径未移植
- `[messaging.teams]` 或 TEAMS_CLIENT_ID/TEAMS_CLIENT_SECRET/TEAMS_TENANT_ID/TEAMS_ALLOWED_USERS

**LINE** —— hermes `platforms.line` 插件——独立 aiohttp 服务器改为网关挂载路由

- ：`/webhooks/line` 校验 `X-Line-Signature`（以 channel secret 为键对原始请求体做 base64 HMAC-SHA256、恒定时间比较），1
  MiB 请求体上限、webhook 事件 id 去重（LRU-1000）、来源解析（user/group/room）+ 复刻 hermes
  三白名单门控（`LINE_ALLOWED_USERS`/`LINE_ALLOWED_GROUPS`/`LINE_ALLOWED_ROOMS`、`LINE_ALLOW_ALL_USERS` 开发豁免）∪
  配对，文本 + 图片/视频/音频/文件内容下载（`api-data.line.me`）入媒体缓存（贴纸/位置降级为文本备注）、loading 指示发送，回复优先使用一次性 reply
  token、被拒后回退计费 Push——去 markdown 保留 URL（`[label](url)` → `label (url)`）、4500 字符气泡、单次调用 5 条消息预算 +
  省略号截断、慢 LLM postback 按钮状态机（hermes PR #18153：agent 运行超过 `slow_response_threshold`——默认 45 秒、0 禁用——时将
  reply token 用于发送 Template Buttons 气泡，其 postback 动作携带请求 id
- 完成的回复进入内存 PENDING → READY/ERROR → DELIVERED 缓存（1 小时 TTL、PENDING 24 小时），用户点按按钮即送达，重复点按回 "Already
  replied ✅"，系统忙碌回执绕过缓存）与出站 `MEDIA:` 标签——LINE 仅接受公网可达 HTTPS 媒体 URL，本地文件以随机 30 分钟令牌注册、由网关
  `/line/media/<token>/<filename>` 提供（允许根目录防御、过期 410、需 `public_url`/`LINE_PUBLIC_URL`）
- 图片上限 10 MB、音视频 200 MB，视频消息附 1×1 回退 PNG 预览
- `[messaging.line]`（`public_url`、`slow_response_threshold`、按钮文案覆盖）或
  LINE_CHANNEL_ACCESS_TOKEN/LINE_CHANNEL_SECRET/LINE_PUBLIC_URL/LINE_SLOW_RESPONSE_THRESHOLD/LINE_PENDING_TEXT/LINE_BUTTON_LABEL/LINE_DELIVERED_TEXT/LINE_INTERRUPTED_TEXT

**Google Chat** —— hermes `platforms.google_chat` 插件，双入站传输

- ：`/webhooks/googlechat` 经 Google `tokeninfo` 端点校验 `Authorization: Bearer` 中的 Google ID 令牌（audience
  + 服务账号邮箱匹配——hermes 的本地 google-auth 证书校验改为在线探测），仅路由 `MESSAGE` 信封（跳过 BOT 发送者、消息名去重 300
  秒窗口），`argumentText` 优先于 `text`，DIRECT_MESSAGE 空间映射为 DM，发送者按邮箱/用户资源白名单 ∪ 配对门控，回复保持在入站线程内，经 Chat REST
  API 发送（`POST /v1/{space}/messages`、4000 字符分块），服务账号 RS256 JWT-bearer 令牌（带缓存、`chat.bot` 作用域）
- 原生文件附件走按用户 OAuth `media.upload` 流程（`oauth.py` 移植，`src/google_chat_oauth.rs`）：Google 对 media.upload
  硬性拒绝服务账号，每位用户经聊天内 `/setup-files` 命令一次性授予 `chat.messages.create` 作用域（PKCE 安装应用流程，`http://localhost:1`
  重定向预期失败——用户将失败 URL 或 code 粘贴回来
- 子命令：无参数状态、`start`、`revoke`、`<code-or-url>` 兑换
- 令牌按发送者邮箱存于 `<home>/google_chat_user_tokens/`，另有遗留单用户槽位），主机侧 CLI 为 `ulnclaw google-chat-oauth
  client-secret|auth-url|auth-code|revoke|check`
- `MEDIA:` 回复以该聊天最近发送者身份上传（multipart `attachments:upload` → 携带返回 `attachmentDataRef` 的
  `messages.create`，两步均用用户令牌、线程附 `messageReplyOption`），401/403 使缓存令牌失效并回退为携带主机路径的设置指引文本提示
- 打字卡片 patch 已移植：回合开始前投递 "ulnclaw is thinking…" 标记卡片（`typing_status_text` 覆盖，hermes
  `send_typing`），回复经 `messages.patch?updateMask=text` 原地改写该卡片——首块 patch、溢出块新建、patch 失败回退新建、空回复 patch
  "(interrupted)"、consumed 哨兵槽位防重复标记（hermes `_typing_messages`/`_TYPING_CONSUMED_SENTINEL` 语义）
- 配置 `pubsub_subscription`（`GOOGLE_CHAT_PUBSUB_SUBSCRIPTION`）后入站改走 Pub/Sub——以 `pubsub` 作用域服务账号令牌对订阅做
  REST 拉取（与 hermes gRPC 流式拉取同语义、无 gRPC 依赖），CloudEvents 信封（`ce-type` 属性）支持 hermes 全部三种载荷格式（Workspace
  Add-ons `chat.messagePayload`、原生 Chat API `{type:MESSAGE}`、中继扁平格式），成员/卡片事件记录日志后直接
  ack，至少一次投递经同一消息名窗口去重，无条件 ack（含畸形载荷），复刻 hermes 监督循环（全抖动指数退避、2 秒基底/120 秒上限、连续 10 次失败或认证/权限错误即终止）
- `[messaging.google_chat]` 或
  GOOGLE_CHAT_SERVICE_ACCOUNT_FILE/GOOGLE_CHAT_HTTP_EVENTS_AUDIENCE/GOOGLE_CHAT_HTTP_EVENTS_SERVICE_ACCOUNT/GOOGLE_CHAT_PUBSUB_SUBSCRIPTION

**Buzz**
- Block 的 Nostr 人机协作平台
- hermes `platforms.buzz` 插件

- ：入站传输可选（hermes `transport`）：`auto`（默认）优先 NIP-42 认证的 WebSocket 订阅，20 秒内无法认证则回退 CLI 轮询
- `websocket` 强制 WS
- `poll` 仅 CLI 轮询。WS 路径以签名 kind-22242 事件应答中继 `AUTH` 质询（`src/nostr_auth.rs` —— 轻量 BIP-340 schnorr +
  nsec bech32 解码，hermes `nostr_auth.py` 移植
- 密钥来自 `BUZZ_PRIVATE_KEY` 或 `~/.config/buzz/*credentials*.json` 文件，可选 `BUZZ_AUTH_TAG` NIP-OA
  归属标签），按频道订阅（`kinds=[9]`、`#h`、`since` 断点续传）并订阅 kind-44100 成员事件流，事件走与轮询相同的去重/提及/白名单机制，1→30 秒退避重连
- 未配置 `BUZZ_PUBKEY` 时从密钥推导自身公钥。轮询路径每 4 秒对每个已配置频道调用本地 `buzz` CLI（`buzz messages get --channel <id>
  --limit 50 [--since <ts>]`，`dm:` 前缀标记私聊频道）。两种传输均仅保留 Nostr kind-9 聊天事件，按事件 id 去重（500
  条上限），抑制自身公钥回声，频道需被提及才放行（完整公钥、≥6 字符公钥前缀或 `@mention`
- 分发前剥离提及），DM 始终放行，可选公钥白名单过滤发送者，回复经 `buzz messages send --channel <id> --content -` 以 stdin
  发送。每条已分发消息派发后经 buzz-cli `reactions add` 追加 👀「已读」tapback（尽力而为，hermes `send_reaction`）
- DM 发现对标 hermes（`_discover_dms` + `_seed_channel` + #68871 分类器）：启动时为每个被监听会话播种高水位（取最新事件标记已见，历史永不重放
- 暂不可读的频道回退为「现在」），并经 `buzz dms list` 发现已有私聊，托管中继返回 `[]` 时以 `buzz channels list` 回退扫描
- 运行中 p 标签指向自身的 kind-44100 成员事件推进 `_membership_since` 订阅游标并重新发现会话，WS 实时为每个新会话追加 `hermes-buzz-dm-<n>`
  订阅（新私聊从头部开始分发），轮询传输每 5 轮扫描再发现一次（`_DM_DISCOVERY_EVERY=5`）
- 经 `channels list` 泄漏进来、名为 "DM" 且描述为空的会话先按群组监听，首条结构化 p 标签且无可见提及的消息到达时闩锁为 chat_type=dm（`_maybe_latch_dm`
- 显式配置的频道与有真实名称/描述的频道永不重分类）
- 群组管理未移植
- `[messaging.buzz]` 或
  BUZZ_CHANNELS/BUZZ_CLI/BUZZ_PUBKEY/BUZZ_POLL_INTERVAL_MS/BUZZ_RELAY_URL/BUZZ_PRIVATE_KEY/BUZZ_CREDENTIALS_FILE/BUZZ_TRANSPORT/BUZZ_AUTH_TAG

**Photon**
- iMessage，经 Photon Spectrum sidecar
- hermes `platforms.photon` 插件，sidecar 客户端传输——ulnclaw 不拉起也不捆绑 Node sidecar，需自行运行并将 `[messaging.photon] sidecar_url` 指向它

- ：等待 sidecar `/healthz` 就绪，消费 `GET /inbound` NDJSON 流（按 messageId 去重，48
  小时窗口）并解析类型化内容（`space`/`sender`/`content` —— `text` | `richlink` | `group` | `attachment` | `voice`
  节点，附件以内联 base64 `data` 入媒体缓存，保留扁平本地路径回退），抑制自送回显（1000 条 id 台账），富链接渲染为标题/摘要/URL 并抑制 iMessage
  富链接预览图（链接后 30 秒内的 `.pluginpayloadattachment` 标记），白名单 ∪ 配对门控，群组 `require_mention` 唤醒词门控（可配置正则，默认
  hermes `hermes` 唤醒词，分发前剥离），发送 `/typing` 输入指示（每聊天 5 秒冷却），回复以 8000 字符上限经 `POST /send`（`{spaceId,
  text}` + markdown `format` 提示，`PHOTON_MARKDOWN` 总开关）以共享 bearer 令牌发送——纯 URL 回复走
  `/send-richlink`，失败回退纯文本
- 表情回应（iMessage tapback）已移植：`PHOTON_REACTIONS` 开启的生命周期 tapback（处理中 👀，完成后先移除再追加 👍 成功/👎 agent
  出错）、sidecar `/react` + `/unreact`，入站 tapback 仅在目标为机器人所发消息时（`targetDirection: outbound` 或目标在自送台账中）以
  `reaction:added:<emoji>` 合成文本路由给 agent——tapback 永不触发提及门控（hermes
  `on_processing_start`/`_add_reaction`/反应路由）
- agent 侧 `send_message action="react"` 面已于 P259 移植（`send_message` 工具的 react/unreact 动作经
  `setMessageReaction` 抵达 Telegram，省略 message_id 时经频道目录回退到最近消息）
- `[messaging.photon]` 或
  PHOTON_SIDECAR_URL/PHOTON_SIDECAR_TOKEN/PHOTON_ALLOWED_USERS/PHOTON_REQUIRE_MENTION/PHOTON_MENTION_PATTERNS/PHOTON_MARKDOWN

**Raft** —— hermes `platforms.raft` 插件，两半齐备

- ：`/webhooks/raft/wake` 要求 `x-raft-bridge-token` 头（未配置时自动生成 32 字节 hex 令牌，并经 `RAFT_CHANNEL_TOKEN`
  共享给拉起的 bridge），16 KiB 请求体上限，`raft-activity.v1` schema 校验 + 安全标量提取，以 `raft:<sessionId>` 聊天分发，并在 JSON
  响应体中返回 agent 回复
- 网关启动时自行拉起 bridge 进程（hermes `_spawn_bridge`：`raft --profile $RAFT_PROFILE agent bridge
  --wake-adapter wake-channel --wake-channel-endpoint <网关 wake url>`、stdin 置空、关停时 SIGTERM + 5 秒宽限 +
  SIGKILL
- PATH 缺少 `raft` 或未设 `RAFT_PROFILE` 时降级为仅 wake 模式）
- `[messaging.raft]` 或 RAFT_BRIDGE_TOKEN/RAFT_PROFILE

**A2A** —— hermes `platforms.a2a` 插件——Agent2Agent v1.0 服务端

- ：在 `GET /.well-known/agent-card.json`（含遗留 `/.well-known/agent.json`）发布 Agent Card，并在 `POST /a2a`
  提供 JSON-RPC 2.0——`message/send` 将入站消息部件经正常 agent 管道分发并返回带回复 artifact 的已完成任务（内存任务台账上限 100
  条），`tasks/get`/`tasks/list`/`tasks/cancel` 查询与管理
- `message/stream`（SSE）与推送通知配置以 capability-false 错误应答
- 可选 bearer 门控（`A2A_TOKEN`），1 MiB 请求上限
- hermes 的独立 `A2A_PORT` HTTP 服务器改由网关路由承载
- `[messaging.a2a]` 或 A2A_AGENT_NAME/A2A_AGENT_DESCRIPTION/A2A_PUBLIC_URL/A2A_TOKEN。入站语音消息进入音频 STT
  管道（见语音转写行）：转写文本以 🎙️ 消息回显并注入回合。交互式 clarify 提问在 WhatsApp 上渲染为原生按钮/列表（见交互式 clarify 行）
- Telegram 渲染原生内联键盘（每选项一个数字按钮 + ✏️ Other (type answer) 行，`cl:<id>:<idx|other>` 回调载荷在 getUpdates
  循环中解析——hermes `send_clarify` 布局）
- Discord 渲染原生按钮（橙色 ❓ 嵌入 + 自包含正文镜像、数字按钮标签按 80 UTF-16 上限做词边界截断、`clarify:<id>:<idx|other>` custom
  id、24 选项上限、INTERACTION_CREATE 路由 + 临时鉴权/过期提示 + UPDATE_MESSAGE 应答编辑、300 秒视觉过期并禁用按钮——hermes
  ClarifyChoiceView）
- Slack 渲染 Block Kit 按钮（mrkdwn 转义 ❓ 区块、`hermes_clarify_choice_<idx>` action id + `<id>|<idx>` 值打包 +
  ✏️ Other… 行，interactive 信封 block_actions 路由 + 双击台账，chat.update 结果改写——hermes
  `_handle_clarify_action`）
- 元宝媒体/表情包通道已移植（见元宝贵条目）
- P225：Telegram 执行审批以行内键盘呈现 —— ✅ Allow Once / Session / Always + ❌ Deny 每行两键排布，`ea:<choice>:<id>`
  回调数据经审批 id → 会话注册表映射（hermes `_approval_state`），先解析后渲染 —— 过期点击显示 ⌛ Approval expired 而非谎报成功（hermes
  #63501），鉴权与 clarify 点击同为白名单∪配对码并集
- `/approve` 文本流仍是回退。P259：每条入站事件记入频道目录（`src/channel_directory.rs`），支撑 agent 侧 `send_message`
  工具（跨频道发送、目标列举、表情回应——见 `send_message` 行）。P337 补齐面向仪表盘的 `/api/messaging/platforms` 线协议面：`GET` 返回全部 26
  个平台的目录（逐平台启用/配置状态、脱敏 env 行、disabled→not_configured→connected 状态阶梯）
- `PUT /api/messaging/platforms/:id` 切换 `[messaging.<id>].enabled` 并在 `.env` 中增删该平台 env 键
- `POST /api/messaging/platforms/:id/test` 返回 `{ok, state, message}` 状态探针——精简对应 hermes `/api/messaging/platforms`


<a id="交互式-clarify-tools-clarify-gateway-py-whatsapp-interactive"></a>

#### 交互式 clarify（`tools/clarify_gateway.py` + WhatsApp interactive） — ✅ 核心

- `src/clarify_gateway.rs` + 消息层集成 —— `clarify` 工具在消息会话中可用：提问登记于有上限的网关注册表（hermes state
  cap），按平台渲染（WhatsApp ≤3 选项用 `interactive.type=button`、4+ 用 `type=list` 并附 ✏️ Other 行，数字标签 +
  正文完整选项文本，20/24/72 字符上限，`cl:<id>:<idx|other>` 按钮 id —— hermes `send_clarify` 布局
- Telegram 渲染内联键盘（每选项一个数字按钮 + ✏️ Other (type answer) 行，`cl:<id>:<idx|other>` callback_data 保持在 64 字节上限内——hermes 布局
- 点按经 answerCallbackQuery 应答并将提问编辑为应答结果——数字点按解析索引→选项文本、Other 切换文本捕获、未授权点按收到 ⛔、过期/被逐条目收到 ⚠️ 提示）
- Discord 渲染嵌入+按钮视图（hermes ClarifyChoiceView 布局——交互回调应答、Other 切换文本捕获、300 秒视觉过期跳过等待文本应答的提问）
- Slack 渲染 Block Kit 按钮（hermes action id/值布局、Other 切换文本捕获、结果经 chat.update 改写提问、原子双击守卫）
- Baileys 桥适配器（`[messaging.whatsapp]`）改以原生单选投票渲染 2–12 选项的 clarify——hermes Baileys
  `send_clarify`，所选选项以纯文本入站——并回退编号文本），并阻塞回合直至应答。点按路由复刻 `_dispatch_interactive_reply`
  语义：索引→选项文本解析、Other 切换文本捕获（`mark_awaiting_text` + ✏️ 提示）、未授权点按认领不分发、过期 id
  回退为以按钮标题作文本分发。会话内下一条纯文本消息将应答等待中的 clarify 而非开启新回合（hermes
  `_maybe_intercept_clarify_text`）。其余平台收到编号文本提问
- `appr:`/`sc:` 前缀（网关审批/slash 确认）与 hermes 无等待者路径相同，回退为文本

<a id="语音转写-tools-transcription-tools-py-gateway-stt-管道"></a>

#### 语音转写（`tools/transcription_tools.py` + gateway STT 管道） — ✅ 核心

- `src/stt.rs` —— hermes 音频 STT 管道：`[stt]` 配置（enabled/echo_transcripts/provider/language + 各
  provider 子块，默认值对齐 hermes），内置 provider
  `local_command`（命令逃生舱，`ULNCLAW_LOCAL_STT_COMMAND`）、`groq`（whisper-large-v3-turbo）、`openai`（whisper-1）、`mistral`（Voxtral）、`xai`、`elevenlabs`（Scribe）、`deepinfra`（在线目录模型发现），均为
  OpenAI 兼容 multipart 上传
- 自定义命令 provider 经 `[stt.providers.<name>]` + 旧式顶层块，保持内置名永远优先的不变式
- 网关语音消息（audio/* 附件）在回合前转写，复刻 hermes 语义 —— provider 失败时回退本地命令、空转写哨兵（#41603）、中性失败标记不进提示词、`🎙️ "<转写>"`
  回显（stt.echo_transcripts）、STT 关闭时 WAV/ffprobe 时长注记
- `transcribe_audio` agent 工具（可选 `stt` 工具集）支持 model/language 覆盖。P327 将该管线暴露给桌面端：`POST
  /api/audio/transcribe`（base64 data-URL 语音输入、返回转写文本
- 25 MiB 上限、mime 校验、provider 就绪检查——对应 hermes `/api/audio/transcribe`
- TTS 已经以 `POST /api/audio/speak` 移植，走 `[tts]` openai/elevenlabs provider——P344），桌面输入框提供麦克风按钮与助手回复 🔊
  朗读动作。已知差异：hermes 默认 `local` provider（faster-whisper，Python）无法嵌入静态二进制 —— 以 `stt.local.command` 或云
  provider 替代

<a id="oauth-登录-技能同步-hermes-cli-portal-cli-py-tools-skills-sync-client-py"></a>

#### OAuth 登录 + 技能同步（`hermes_cli/portal_cli.py`、`tools/skills_sync_client.py`） — ✅ 核心

`src/oauth.rs` + `src/skills_sync.rs`：hermes 门户认证 + Skill Sync 的服务无关移植。`ulnclaw auth login` 对任意配置的
`[oauth]` provider（device_authorization_url/token_url/client_id/scopes）执行 RFC 8628 设备授权许可，处理
authorization_pending/slow_down，令牌存于 `oauth_tokens.json`（0600）、refresh-token
许可、`status`/`refresh`/`logout`/`open`。`ulnclaw sync status|pull|push|now|enable|disable|device` 原样保留
hermes 的 UX：可选技能同步 + 稳定设备 id + 设备标签，`[sync] base_url` 未配置时报告 INERT 门控，pull 绝不覆盖本地技能。传输通用：HTTP(S)
REST（bearer = OAuth 令牌或 `[sync] api_key`）或共享目录（离线/NAS 同步）。Nous 门户专属的订阅特性与组织提案审批流程未移植

<a id="oauth-上游代理-hermes-cli-proxy"></a>

#### OAuth 上游代理（`hermes_cli/proxy/`） — ✅ 核心

`src/proxy_cmd.rs` + `ulnclaw proxy start|status|providers`（P179）：本地 OpenAI 兼容代理，让外部应用复用用户已登录的 OAuth
订阅而无需静态 API key。监听 `127.0.0.1:8645`（`[proxy] host/port`），挂载 `/v1/*`
并带路径白名单（`/chat/completions`、`/completions`、`/embeddings`、`/models`、`/responses`），丢弃客户端 bearer，附加
`oauth_tokens.json` 访问令牌（过期前 60 秒自动刷新并持久化），上游 401/429 时一次性强制刷新重试，10 MB 请求上限（hermes
`MAX_REQUEST_BYTES`），剥离 hop-by-hop 头，字节流恒等透传（保留 SSE），`/health` 探针。差异：适配器为 provider 无关（`[proxy]
upstream_url`），取代 hermes 的 Nous 门户订阅解析器与 xAI 凭证池适配器（凭证池存储本身已精简移植——见凭证池行）

<a id="桌面-gui-apps-desktop-electron"></a>

#### 桌面 GUI（`apps/desktop` Electron） — ✅ 核心

- `desktop-electron/` —— **ulnclaw desktop**：v0.7.0 起 ulnclaw 直接交付 hermes Electron
  桌面端（v2026.8.3）的忠实移植（早前的 Tauri 2 外壳退役）：React 19 + Vite 渲染层（十六视图 + 命令面板 + 主题/字体 + 多语言切换）、xterm.js
  终端面板、文件树 + git 审查侧栏、会话浏览器、经 JSON-RPC WebSocket + HTTP/SSE 对接 ulnclaw 网关的实时回合流式，Electron
  主进程负责拉起/监管内置的静态链接网关二进制（`ULNCLAW_DESKTOP=1`、健康探测 + 有上限重生、启动诊断、托盘 + 原生菜单、`ulnclaw://`
  深链、单实例交接、窗口状态持久化）。网关 JSON-RPC WS 面覆盖桌面的全部方法目录——会话生命周期（`session.create` 按 hermes 懒建行契约铸造会话 id，并把逐会话的
  model/cwd/title 覆盖持久化、由 WS 回合经模型锁强制执行
- resume/close/title/interrupt）、带 queued/interrupted 标志的 prompt.submit、审批/澄清/sudo/密钥应答、config
  get/set、env/MCP 重载、模型选项、斜杠补全 + 执行、消息回应与唤醒词界面、会话分析与生命周期工具（`session.usage` /
  `session.context_breakdown` / `session.status` / `session.save` 转录导出 / `session.branch` 谱系分叉 /
  `session.compress` LLM 摘要（与 `/compress` 斜杠同源）/ `session.redirect` 忙碌回合排队 / `session.activate` /
  `session.active_list` 存活快照，另含 `session.create` 的 `messages` 种子 + `parent_session_id`
  分支契约）、宠物精灵图界面（`pet.info` 精灵图载荷 / `pet.info.meta` / `pet.gallery` 含 petdex 清单合并 / `pet.thumb` /
  `pet.scale` / `pet.disable` / `pet.export`）、门户计费的失败开放桩（`billing.state` / `subscription.state` 返回未登录
- 充值 / step-up / 自动充值 / 订阅变更类操作返回类型化 `unavailable` 拒绝封装）、`browser.manage` 基于 CDP 覆盖的
  status/connect/disconnect、`command.dispatch` 兜底路由、如实报不支持的 `handoff.*`、`preview.restart`
  后台开发服务器重启、`setup.status` / `setup.runtime_check` 引导探测，以及流式 TTS WebSocket。v0.7.1
  起打包壳还支持静默自更新（electron-updater 对接 GitHub release 通道，退出时安装——ulnclaw 增量能力，hermes 桌面无壳级自动更新）

<a id="会话浏览-hermes-cli-sessions-cmd-py-browse-curses-挑选器"></a>

#### 会话浏览（`hermes_cli/sessions_cmd.py browse` + curses 挑选器） — ✅ 核心

- `ulnclaw sessions browse [--source S] [--limit N]`：TTY 上启用原始模式 TUI（crossterm 移植 curses 挑选器 ——
  备用屏幕、↑/↓/PgUp/PgDn/Home/End 滚动导航、键入即过滤 + 退格、绿色 `▶` 选中高亮、Enter 选择、无过滤时裸按 `q` 退出、Esc 先清空过滤再按一次才退出、单步
  ↑/↓ 在列表首尾回绕、暗色列头（Title/Preview · Active · Src · ID）与底部页脚（光标位置 + 过滤前总数）、"终端过小"保护、LF（`Ctrl+J`）形式的
  Enter 同样接受）
- 管道/CI 场景回退为编号 stdin 挑选器
- 按最近活动排序的行（标题 → 首条用户消息预览回退、相对时间、来源、截断 id）、对标题/预览/id/来源的子串过滤、未指定 `--source` 时排除 `tool` 源会话（hermes 语义）
- P165 增加所属项目徽章：cwd 落在项目文件夹下的行渲染 `⌂ slug` 前缀，实时过滤器同时匹配项目 slug（两款挑选器一致，对 `projects.db` 尽力而为）
- 选中后以 `--resume <id>` 重新启动当前二进制（hermes `relaunch`）
- 存储查询 `list_sessions_for_browse` 单条 SQL 返回挑选器行
- P177 在 hermes 挑选器之上升级原始模式 TUI 交互：终端 ≥ 90 列时右侧增加详情窗格，展示高亮会话的完整标题、完整 id、来源、所属项目、cwd、最近活动时间（相对 + 绝对本地时间）与换行后的首条用户消息预览
- `Tab` 循环按来源过滤（全部 → 各出现来源 → 全部，渲染为绿色 `[source: …]` 头部徽章），`F2` 切换最近优先 ↔ 字母序（无标题者排最后）排序（`[sort: A-Z]` 徽章）
- 字符感知的文本排版助手位于 `src/tui_text.rs`，含单元测试
- P224 原始模式交互升级：`F1` 按键帮助浮层（任意键关闭）、`F5` 浏览中从磁盘重载会话列表（新开店库连接 —— 运行中的网关可能随时创建新会话）、`F8` 行内 `y`
  确认后归档高亮会话（hermes `set_session_archived`，自动重载 + 绿色瞬态页脚提示）、`Shift+Tab`
  反向循环来源过滤，PgUp/PgDn/Home/End/Ctrl+C 此前已支持
- P340 原始模式交互升级：`/` 打开转录搜索提示行——Enter 经新存储连接执行 FTS5 消息正文搜索（LIKE 兜底，200 条命中），将列表收窄至匹配会话并在详情窗格显示命中摘录，Esc 清除结果
- `F9` 行内 `y` 确认后永久删除高亮会话（hermes `delete_session`，自动重载 + 页脚提示）

<a id="会话恢复与单会话连续性-cli-py-resume-continue"></a>

#### 会话恢复与单会话连续性（`cli.py --resume/--continue`） — ✅ 核心

- 全局 `-r/--resume <id或前缀>` 与 `-c/--continue` 标志，适用于 `chat` 与 `run`：整个 REPL 会话存于同一条会话记录（此前每轮都会新建记录）
- 恢复时用 `load_messages` 回填 REPL 历史（丢弃 system 行），打印 `Resuming session: <id> (标题)`，每轮经 `run_with_session` 写入同一 id
- `/new` 轮换到新会话键并重置按会话的目标管理器
- `latest_session_id` 按最近活动挑选 `--continue` 目标（跳过已归档）
- 所有带 id 的 `sessions` 动作（`show`/`export`/`recap`/`delete`/`rename`）均经 `resolve_session_id` 接受唯一前缀
- P232 移植 hermes 的 `-c <会话名>` 标题查找：`-c/--continue` 现接受可选值 —— 裸 `-c` 保持续接最近会话，`-c 名称` 先按精确
  id/唯一前缀解析，再按标题解析（`resolve_session_by_title` —— `title #N` 谱系编号变体取最新、跳过已归档、LIKE
  通配符转义），并沿压缩链向前投影（`compression_tip`），使恢复落在存活的链尖而非过期的被压缩父会话

<a id="会话库修复-hermes-state-py-repair-state-db-schema"></a>

#### 会话库修复（`hermes_state.py repair_state_db_schema`） — ✅ 核心

- `ulnclaw sessions repair [--check-only] [--no-backup]`：健康探测（`db_opens_cleanly` —— `PRAGMA
  journal_mode` 首语句触发、`integrity_check`、sessions 读取、FTS MATCH 读探测、回滚式 FTS 写探测）后按破坏程度逐级升级 —— FTS5
  `'rebuild'` 原地重建、`REINDEX` 修复过期 B 树索引、经 `writable_schema` 去重 `sqlite_master`（保留 FTS 索引）、删除 FTS 结构 +
  `VACUUM` 并在下次打开时重建（`initialize_schema` 回填滞后的外联内容索引）
- 先做带时间戳的原始备份 + WAL/SHM 附属文件
- 失败时指向离线 `sessions recover`
- 在打开存储之前执行，因为库结构损坏正是无法打开的情形

<a id="会话删除-重命名-优化-hermes-cli-sessions-cmd-py"></a>

#### 会话删除/重命名/优化（`hermes_cli/sessions_cmd.py`） — ✅ 核心

- `ulnclaw sessions delete <id> [--yes]`（id 或唯一前缀，`resolve_session_id` —— LIKE 转义前缀匹配、精确 id 优先、歧义即未找到
- 除 `--yes` 外 y/N 确认
- 先删消息 + FTS 行再删会话）、`sessions rename <id> <title...>`（hermes `sanitize_title`：剥离 ASCII/Unicode
  控制字符、折叠空白、空标题清除、100 字符上限、跨会话标题唯一
- 回报实际存储标题）、`sessions optimize`（FTS5 `'optimize'` 段合并 + 尽力 WAL checkpoint + `VACUUM`
- 报告合并索引数与前后大小，用 `logical_size_bytes` 页统计避免 WAL 滞后误报）

<a id="供应链安全审计-hermes-cli-security-audit-py"></a>

#### 供应链安全审计（`hermes_cli/security_audit.py`） — ✅ 核心

- `ulnclaw security audit [--json]`：按需对固定版本的 MCP 服务器包做 OSV.dev 审计（`npx pkg@ver` / `uvx pkg==ver`，含 npm 作用域包）
- 未固定版本/本地条目静默跳过不猜测
- `querybatch` + 逐漏洞详情抓取（severity 取 `database_specific`/`ecosystem_specific`、修复版本去重、摘要截断 100 字符）
- 结果按严重度排序、按来源分组，人类可读 + JSON 输出
- hermes 的 venv/插件扫描面对静态 Rust 二进制不适用
- P321 新增 `GET /api/ops/security-audit`（进程内运行，桌面 Doctor 运维面板呈现——对应 hermes `/api/ops/security-audit`）


<a id="功能对标"></a>

## 功能对标

| hermes 功能 | ulnclaw | 说明 |
|---|---|---|
| 工具调用代理循环 | ✅ | 迭代预算、用量统计、step 回调 |
| SQLite 状态库（`hermes_state.py`） | ✅ | sessions/messages/system_prompts/state_meta/async_delegations 表结构，FTS5（不可用时 LIKE 回退），会话血缘 |
| 会话数据库恢复（`session_recovery.py`） | ✅ 核心 | `ulnclaw sessions recover <db> [--out FILE]`：离线、非破坏性——源库连同 WAL/SHM/journal 旁车文件复制到一次性目录，规范表按列交集拷入全新当前表结构库，受损表按 rowid 逐行抢救，孤儿消息重建会话行，重建 FTS，完整性校验 + JSON 报告；绝不就地修复或覆盖在用数据库 |
| 环境探针（`tools/env_probe.py`） | ✅ | 终端后端为本地时，向系统提示注入一行确定性的 Python 工具链说明：python3/python 版本、pip 模块可用性、`pip`↔`python3` 版本错配、PEP 668 外部管理标记（有 uv 时不告警）；健康环境保持静默；进程级缓存由单一后台线程构建，调用方最多等 10 秒后放行；远端后端（docker/ssh）跳过探测；`[agent] environment_probe` 开关（默认开启） |
| 上下文压缩（`conversation_compression.py`） | ✅ | 预算触发，中段对话经二次模型调用摘要，保留系统提示词 + 首条用户消息 + 最近尾部；摘要调用遵循 `[auxiliary.compression]` 路由。P … [详情](#上下文压缩-conversation-compression-py) |
| 流式思考块清洗（`agent/think_scrubber.py`） | ✅ | `think_scrubber.rs`：对流式增量中的 `<think>`/`<thinking>`/`<reasoning>`/`<thought>`/`<REASONING_SCRATCHPAD>` 块做有状态抑制 —— `call_with()` 中每个内容增量都经过状态机喂送（开标签可跨增量分片存活，未闭合开标签受块边界门控），流结束时冲刷暂留的部分标签尾部，非流式路径走完整字符串 `strip_think_blocks`；闭合对总是被抑制，开标签仅在块边界生效，因此仅提及标签名的正文不会被误剥离 |
| 会话标题生成器（`agent/title_generator.py`） | ✅ | `title_generator.rs`：首轮交流后即发即忘的自动标题（后台任务，不增加回复延迟）—— 前 2 轮用户消息守卫、已有标题守卫、`[auxiliary.title_generation]` 路由（`language` 语言固定、`enabled` 开关，`is_truthy_value` 语义，默认 true）；500 字符摘要、答案先经推理块清洗、引号/"Title:" 前缀/首行/80 字符清理；`set_auto_title_if_empty` 原子持久化 —— 生成进行中手动设置的标题优先保留；可选标题回调与 portal/记账标签未移植（无对应实时 UI 面） |
| 持久化目标 —— Ralph 循环（`hermes_cli/goals.py`） | ✅ 核心 | `goals.rs`：跨轮次存续的既定目标 —— 每个助手轮次结束后由 `goal_judge` 辅助模型裁决 done/continue/wait；未完成时把 … [详情](#持久化目标-ralph-循环-hermes-cli-goals-py) |
| 时区感知时钟（`hermes_time.py`） | ✅ | `hermes_time.rs`：IANA 时区解析顺序 `ULNCLAW_TIMEZONE` → `HERMES_TIMEZONE` → 配置 `timezone` → 服务器本地时间（非法时区名告警后回退，进程级缓存 + `reset_cache()`）；系统提示词注入仅含日期的 "Conversation started" 行 + Model/Provider（全天字节稳定以保护前缀缓存，hermes PR #20451）；压缩摘要携带 `Current date` 时间锚点 |
| 审批系统（`approval.py`） | ✅ | 命令归一化（反斜杠续行、`${IFS}`、注释剥离）、硬性底线（直接阻止）、可恢复但昂贵的操作（需确认）；REPL y/N 提示；网关运行审批（ … [详情](#审批系统-approval-py) |
| 威胁模式扫描（`threat_patterns.py`） | ✅ 核心 | 对重新进入上下文的工具结果做提示注入扫描（建议性） |
| 工具集（`toolsets.py`） | ✅ | 全部 33 个工具集定义，含组合（`includes`），默认 `coding` |
| 工具注册表（`registry.py`） | ✅ | check_fn 门控、工具集分组、结果大小截断 |
| Provider 抽象（`runtime_provider.py`） | ✅ | OpenAI 兼容（OpenAI/OpenRouter/DashScope/Ollama/llama.cpp）、原生 Anthropic Messages 传输（`anthropic_messages`：system 参数、tool_use/tool_result 块、SSE 流式、max_tokens 上限、OAuth bearer）、本地 provider 免密钥 |
| Provider 回退链（`fallback_providers`、`try_activate_fallback`） | ✅ 核心 | `[model] fallbacks = ["provider:model", ...]`：模型调用失败时按序推进（每条目惰性构建客户端、密钥回退主运行时），激活的回退在本轮内保持生效，下一轮恢复主 provider（hermes `restore_primary_runtime`）；委派/cron 子代理继承配置 |
| 辅助模型路由（`auxiliary_client.py`） | ✅ 核心 | `[auxiliary.<task>]` 按任务覆盖 provider/模型/base_url/api_key/key_env（`compression`、`vision`、`title_generation`）；`"auto"`/留空继承主运行时；无覆盖时复用主客户端 |
| models.dev 目录（`agent/models_dev.py`） | ✅ 核心 | `models_dev.rs`：拉取 `https://models.dev/api.json`，三级缓存——内存（1 小时 TTL，过期数据立即返回并由后台线 … [详情](#models-dev-目录-agent-models-dev-py) |
| 配置（`config.yaml`） | ✅ | `config.toml` + `.env` 文件、profiles、环境变量优先级 |
| 技能系统 | ✅ | 发现、frontmatter、关联文件、`/skill-name` 调用脚手架（hermes `build_skill_invocation_message` 移植：激活注记 + 技能正文 + 技能目录/辅助文件提示 + 用户指令标记、`skill_usage` 计数；供 `sessions retitle-skills` 识别） |
| 记忆系统 | ✅ | MEMORY.md/USER.md，注入提示词 |
| Cron 调度器 | ✅ | 任务存储 + 计划解析 + 轮询循环（`cron::run_scheduler`） |
| MCP 客户端（`mcp_tool.py`） | ✅ 核心 | stdio JSON-RPC：initialize/tools/list/tools/call；`[[mcp.servers]]` 配置；工具注册为 … [详情](#mcp-客户端-mcp-tool-py) |
| MCP 频道桥（`mcp_serve.py`） | ✅ 核心 | P260 完整移植 hermes `mcp_serve.py`（`src/mcp_serve.rs`， … [详情](#mcp-频道桥-mcp-serve-py) |
| ACP 适配器（`acp_adapter/`） | ✅ 核心 | P261 移植 hermes `acp_adapter/`（`src/acp_adapter.rs`，`ulnclaw acp [--verbose]`）：面向 … [详情](#acp-适配器-acp-adapter) |
| 批量运行器（`batch_runner.py`） | ✅ 核心 | P262 完整移植 hermes `batch_runner.py`（`src/batch_runner.rs`， … [详情](#批量运行器-batch-runner-py) |
| Send CLI（`hermes_cli/send_cmd.py`） | ✅ 核心 | P263 完整移植 hermes `hermes send`（`ulnclaw send`）：脚本/cron/CI 消息投递，无 LLM、无 agent 循环— … [详情](#send-cli-hermes-cli-send-cmd-py) |
| Slack 原生斜杠命令 + 应用清单（`hermes_cli/slack_cli.py` + slack 适配器斜杠流） | ✅ 核心 | P265 移植 hermes `hermes slack manifest` + Slack socket-mode 斜杠路径： … [详情](#slack-原生斜杠命令-应用清单-hermes-cli-slack-cli-py-slack-适配器斜杠流) |
| 网关监控 + OTLP 导出（`agent/monitoring/*`） | ✅ 核心 | P266 完整移植 hermes 网关监控平面（`src/monitoring.rs`）：无内容 `GatewayHealthEvent`/ … [详情](#网关监控-otlp-导出-agent-monitoring) |
| 安装向导（`hermes_cli/setup.py`） | ✅ 核心 | P267 精简移植 hermes 交互式安装向导（`src/setup_cmd.rs`）： … [详情](#安装向导-hermes-cli-setup-py) |
| 模型挑选器（`cmd_model` / `select_provider_and_model`） | ✅ 核心 | P268 移植 hermes `hermes model`：`ulnclaw model [--refresh]` —— 需 TTY 的交互式 provider … [详情](#模型挑选器-cmd-model-select-provider-and-model) |
| 桌面启动器 + 命令别名（`cmd_gui` / `cmd_login` / `cmd_logout` / journey 别名） | ✅ 核心 | P269 移植 hermes 剩余的小型界面：`ulnclaw gui [--binary PATH] [--dev]`（别名 `desktop`）解析已打包的 … [详情](#桌面启动器-命令别名-cmd-gui-cmd-login-cmd-logout-journey-别名) |
| 动态 webhook 订阅（`hermes_cli/webhook.py`） | ✅ 核心 | P270 移植 hermes `hermes webhook`： … [详情](#动态-webhook-订阅-hermes-cli-webhook-py) |
| WhatsApp Cloud 安装向导（`hermes_cli/setup_whatsapp_cloud.py`） | ✅ 核心 | P271 精简移植 hermes `hermes whatsapp-cloud`：`ulnclaw whatsapp-cloud` —— 需 TTY 的交互式向 … [详情](#whatsapp-cloud-安装向导-hermes-cli-setup-whatsapp-cloud-py) |
| WhatsApp 桥状态（`hermes whatsapp` 面） | ✅ 核心 | P272 以只读诊断取代 hermes 的 Baileys 向导：`ulnclaw whatsapp [status]` —— 平台/模式状态、解析后的桥 URL（内置自动拉起 vs 外部）、Node.js 检测、脚本目录安装/依赖新鲜度（无副作用检查）、pidfile 进程状态（运行中/陈旧/外来）、实时 `/health` 探测（connected-as-JID / QR 配对摘要）与后续步骤提示。向导的安装+配对职责由网关监督的桥承担（首次启动安装依赖、启动时 QR 见 bridge.log）——已记录为差异 |
| CLI（`hermes_cli/`） | ✅ 核心 | 带斜杠命令的聊天 REPL（含 `/rollback [N\|hash] [file]`、`/rollback diff <N>`、`/diff` 检查点命令 … [详情](#cli-hermes-cli) |
| Git 工作区 diff（`working_diff.py`） | ✅ | `ulnclaw diff [--staged\|--all] [--dir PATH] [paths...]` + REPL `/gitdiff [staged\|all]`：working/staged/all 三模式，未跟踪文件经 `git diff --no-index` 折入（上限 50 个），带超时；基于检查点的 REPL `/diff` 保持独立 |
| 委派（delegation） | ✅ | SubAgentRunner trait、深度限制、子会话 |
| 混合智能体 MoA（`moa_loop.py`、`moa_config.py`） | ✅ 核心 | `[moa.presets.<name>]` 参考模型并行扇出 + 聚合器综合（`ulnclaw moa run/list/delete`、REPL … [详情](#混合智能体-moa-moa-loop-py-moa-config-py) |
| HTTP 网关（`gateway/platforms/api_server.py`） | ✅ 核心 | `ulnclaw gateway`：OpenAI 兼容 `/v1/chat/completions`（`X-Ulnclaw-Session-Id` 会话续接 … [详情](#http-网关-gateway-platforms-api-server-py) |
| TUI/web/app | ✅ 核心 | TUI：聊天 REPL + 原始模式会话挑选器（`sessions browse`）；web：HTTP 网关提供 OpenAI 兼容 + 会话 API 并带本地应用 CORS，任意浏览器仪表盘可直接接入；桌面：Electron 外壳（`desktop-electron/`，见桌面 GUI 行） |
| 沙箱环境清洗 + passthrough（`environments/local.py` 黑名单、`env_passthrough.py`） | ✅ | terminal/execute_code 子进程继承的环境会剔除 provider/工具凭证黑名单与虚拟环境标记（`VIRTUAL_ENV`/`CONDA_PREFIX`）；技能 `required_environment_variables`（`skill_view` 时注册）与 `[terminal] env_passthrough` 放行其余变量——受保护的 provider 凭证与 `AUXILIARY_*_API_KEY`/`GATEWAY_RELAY_*` 动态密钥永远被拒绝（hermes GHSA-rhgp-j443-p4rf，失败即关闭） |
| 环境（`tools/environments/`） | ✅ 核心 | `terminal` 后端：local（默认）、docker（`ensure_docker_container` inspect→run）、ssh（BatchMode、identity 文件）；`[terminal] backend/container/image/ssh_host/...`；modal/daytona/vercel 暂缓 |
| 检查点管理器（`checkpoint_manager.py`） | ✅ | v2 共享 shadow git 存储（`<home>/checkpoints/store`）：按项目 ref/index，编辑前透明快照（每轮 `write_file`/`patch` 前一次），list/restore/diff/prune CLI，容量上限、超大文件过滤、孤儿/过期自动清理；P317 将检查点经 HTTP 暴露——`GET /api/checkpoints/status`、`GET /api/checkpoints?dir=`、`POST /api/checkpoints/restore`、`POST /api/checkpoints/prune`——并在 Doctor 提供存储概览、逐项目行与清理面板 |
| 浏览器监督器 | ✅ | `ULNCLAW_BROWSER_CDP=auto` 时自动启动受管 headless Chrome/Chromium |
| Camofox 后端（`tools/browser_camofox.py`） | ✅ 核心 | `browser/camofox.rs`：`CAMOFOX_URL` REST 反检测浏览器（Camoufox）后端——全部 12 个 browser 工具经 … [详情](#camofox-后端-tools-browser-camofox-py) |
| 云浏览器 provider（`agent/browser_provider.py` + `agent/browser_registry.py` + `plugins/browser/*`） | ✅ 核心 | `browser/cloud.rs`：`CloudBrowserProvider` trait + 注册表移植——**Browserbase**（ … [详情](#云浏览器-provider-agent-browser-provider-py-agent-browser-registry-py-plugins-browser) |

### 详细说明（功能对标）

<a id="上下文压缩-conversation-compression-py"></a>

#### 上下文压缩（`conversation_compression.py`） — ✅

- 预算触发，中段对话经二次模型调用摘要，保留系统提示词 + 首条用户消息 + 最近尾部
- 摘要调用遵循 `[auxiliary.compression]` 路由。P233 移植三层工具结果持久化（`tools/tool_result_storage.py` +
  `budget_config.py`）：第 2 层将超过阈值（默认 10 万字符
- `read_file` 钉死为无穷大，防 persist→read→persist 循环）的工具结果经 terminal backend 写入
  `<tmp>/ulnclaw-results/<call-id>.txt`（本地直写、docker/ssh 走 stdin 管道，命令在哪跑文件就在哪可达），上下文内替换为
  `<persisted-output>` 1500 字符预览 + 路径（模型可用 read_file 读取全文）
- 第 3 层强制单轮 20 万字符聚合预算，优先溢出最大的未持久化结果
- 写入失败降级为行内截断说明，绝不静默丢失

<a id="持久化目标-ralph-循环-hermes-cli-goals-py"></a>

#### 持久化目标 —— Ralph 循环（`hermes_cli/goals.py`） — ✅ 核心

- `goals.rs`：跨轮次存续的既定目标 —— 每个助手轮次结束后由 `goal_judge` 辅助模型裁决 done/continue/wait
- 未完成时把续跑提示作为普通用户消息回灌，直至目标达成、被暂停/清除或轮次预算（默认
  20）耗尽。完成契约（outcome/verification/constraints/boundaries/stop_when）可通过内联 `field: value` 目标行或 `/goal
  draft`（辅助模型起草）设定
- 子目标（`/subgoal`）并入裁判与续跑提示
- WAIT 裁决把循环停泊在后台进程 pid/会话或截止时间上而不耗轮次（`/goal wait <pid>`，条件满足自动解除）
- 裁判 fail-open，解析连续 3 次或传输连续 5 次失败自动暂停
- 目标状态持久化于 `state_meta`（键 `goal:<session_id>`，重启后仍在，`migrate_goal_to_session` 支持会话轮换）
- REPL `/goal` + `/subgoal` 斜杠命令
- kanban 目标循环留在桌面侧

<a id="审批系统-approval-py"></a>

#### 审批系统（`approval.py`） — ✅

- 命令归一化（反斜杠续行、`${IFS}`、注释剥离）、硬性底线（直接阻止）、可恢复但昂贵的操作（需确认）
- REPL y/N 提示
- 网关运行审批（`POST /v1/runs/:id/approval`，once/session/always/deny，SSE `approval.request`）、fail-closed
  `[approvals] timeout`（默认 300s）、`always` 授权跨重启持久化
- 消息会话在聊天内审批（hermes `tools/approval.py` 网关注册表移植，`src/approval_gateway.rs`）：approve
  回调注册按会话阻塞条目，在平台上渲染审批提示（Teams AdaptiveCard 按钮，其余平台 `/approve` 文本回退，命令先经凭据脱敏），`/approve [all]
  [session|always]` / `/deny [all]` 聊天命令或卡片点按解析最旧的待决条目
- `[approvals] mode = manual|smart|off` —— smart 模式先询问辅助守护 LLM（防提示注入的提示词设计，运维 `smart_policy`
  仅走可信通道），不确定时升级人工，`off` 在硬性底线以下自动放行
- `cron_mode = deny|approve` 管控无人值守 cron 运行（deny = fail-closed 默认）
- P221 补齐 smart 审批面：`denial_breaker_threshold`（默认 3）—— 连续 N 次守护 DENY 后，无人值守拒绝消息从 "do not retry"
  升级为硬停止指令（向用户报告/请求手动执行），任何批准都会重置计数
- `deny` fnmatch 通配规则无条件阻断匹配命令，先于 mode=off/yolo 旁路生效（大小写不敏感 `*`/`?`/`[...]` 语义）。P237 移植 tirith
  执行前内容扫描器（`tools/tirith_security.py`）：每条过闸终端命令额外经外部 `tirith` 二进制扫描（同形异义 URL、管道直灌解释器、终端注入等）
- 退出码即裁决（0 放行 / 1 阻止 / 2 警告），JSON 输出仅用于充实 findings（上限 50 条）/ summary（上限 500 字符）
- block 与 warn 均转为可审批警告并合并进确认原因（`_format_tirith_description`），运行性故障（拉起失败 / 超时 / 未知退出码 / 信号杀死）遵循
  `[security] tirith_fail_open`（默认 true），`.app` 顶级域同形警告被抑制，连续 3 次崩溃打开进程生命周期熔断器，警告去重防止 fail-open 配置错误刷屏
- 解析顺序 PATH → `<home>/bin/tirith` → 从 GitHub releases 自动安装（始终校验 SHA-256
- PATH 上有 cosign 时做来源证明
- 24 小时磁盘失败标记 `.tirith-install-failed`
- `ensure_installed` 在 agent 启动时后台线程下载，启动永不阻塞）
- `[security] tirith_enabled/tirith_path/tirith_timeout/tirith_fail_open`
  可配，`TIRITH_ENABLED`/`TIRITH_BIN`/`TIRITH_TIMEOUT`/`TIRITH_FAIL_OPEN` 环境变量覆盖
- 无 tirith 构建的平台（Windows）静默回退到模式守卫

<a id="models-dev-目录-agent-models-dev-py"></a>

#### models.dev 目录（`agent/models_dev.py`） — ✅ 核心

- `models_dev.rs`：拉取 `https://models.dev/api.json`，三级缓存——内存（1 小时 TTL，过期数据立即返回并由后台线程刷新）→
  磁盘（`$ULNCLAW_HOME/models_dev_cache.json`，任意陈旧度可用）→ 网络单飞获取（失败后进程级退避 5 分钟）
- provider ID 映射 + 同名回退、上下文/能力查询（大小写不敏感、`:cloud`/`-cloud` 后缀回退）、agentic 目录过滤（噪声模式 + Google
  隐藏清单）、`get_provider_info`/`get_model_info`
- `ULNCLAW_MODELS_DEV_URL` 镜像覆盖（http(s)/file）、`ULNCLAW_MODELS_DEV_CACHE` 路径覆盖
- 网关 `/api/model/options` 选择器清单（P168）+ `?refresh=true`
- CLI `ulnclaw models providers\|list\|info\|refresh`

<a id="mcp-客户端-mcp-tool-py"></a>

#### MCP 客户端（`mcp_tool.py`） — ✅ 核心

- stdio JSON-RPC：initialize/tools/list/tools/call
- `[[mcp.servers]]` 配置
- 工具注册为 `mcp__<server>__<tool>`
- npx/uvx/pipx 启动前的 OSV 恶意软件检查（`osv_check.py` 移植：MAL-* 通告阻止启动、fail-open、1
  小时结论缓存、`OSV_ENDPOINT`/`OSV_CHECK_CACHE_TTL` 覆盖）。P235 以内核原语移植 stdio
  看门狗（`mcp_stdio_watchdog.py`）：Linux 上每个 MCP 子进程以 `prctl(PR_SET_PDEATHSIG, SIGKILL)` 拉起，ulnclaw
  硬死亡（kill -9 / 崩溃）时由内核回收服务器进程，避免孤儿进程与下次启动抢占同一上游会话（hermes 用监督进程达成同效
- macOS/Windows 仍走优雅退出回收）。P236 移植 schema 缓存与懒启动（`mcp_schema_cache.py` + `mcp_tool.py`
  `_register_from_cache_sync`）：每个服务器的工具清单持久化到 `<home>/cache/mcp_schema_cache.json`（0700 `cache/` 目录下
  0600 文件、原子暂存重命名），以服务器名 + 连接配置的 SHA-256 指纹为键（与 hermes 相同的载荷结构：command/args/url/transport/tools 过滤器
- 与 hermes 一致不纳入 env）
- 每次成功连接后实时注册会写回刷新清单，`lazy = true` 且缓存指纹匹配的服务器注册工具时不拉起子进程——首次工具调用才执行 OSV 预检、拉起服务器，并将实时工具列表与过期清单对账（刷新缓存
- 幻影工具不再反注册，而是以过期 schema 错误快速失败，因 ulnclaw 处理器不持有注册表句柄
- 完全相同的缓存条目跳过重写）。P238 移植 `/reload-mcp` 与 slash 确认原语（`tools/slash_confirm.py` + `cli.py
  _confirm_and_reload_mcp/_reload_mcp`）：REPL `/reload-mcp` 先警告重建工具面会使 provider 提示词缓存失效，提示 Approve Once
  / Always Approve / Cancel（由 `approvals.mcp_reload_confirm` 门控，默认开启
- 选 "always" 经配置写入器持久化 `false`），随后重新读取 config.toml、注销全部 `mcp:*` 工具、重连所有服务器（懒注册服务器从缓存重新注册）、报告
  Reconnected/Added/Removed 与工具数，并在会话末尾注入变更说明——模型下一回合即见新工具面，而提示词缓存前缀得以保留
- 网关侧确认原语（`src/slash_confirm.rs`：按会话键的待决登记表、confirm_id 取代语义、先弹出再执行防重复回调、300 秒过期、once/always/cancel
  选项）支撑平台按钮 / `/approve` 文本两条裁决路径。P239 移植远程 MCP 传输（hermes `mcp_tool.py` 的 Streamable HTTP + SSE 路径，以手写
  JSON-RPC-over-HTTP 取代 Python MCP SDK）：带 `url` 的 `[[mcp.servers]]` 条目经 Streamable HTTP 连接（POST 携
  `Accept: application/json, text/event-stream`，响应体可为 JSON 或 SSE，捕获/回传 `Mcp-Session-Id`，协议版本
  2025-03-26），`transport = "sse"` 选择 2025-03-26 之前的 SSE 协议（GET 流 → `endpoint` 事件 → POST 并以 `message`
  事件按 id 关联应答），静态 `headers`（如 `Authorization`）随每个请求发送
- schema 缓存指纹自此携带真实 url/transport。P240 移植 MCP OAuth 2.1 + PKCE（`tools/mcp_oauth.py` +
  `tools/mcp_oauth_manager.py` 核心，以手写实现取代 MCP SDK 的 `OAuthClientProvider`）：`[[mcp.servers]] auth =
  "oauth"`（可选 `[mcp_servers.<name>.oauth]`
  client_id/client_secret/scope/redirect_port/redirect_uri/redirect_host/client_name）执行 受保护资源 → 授权服务器
  元数据发现、RFC 7591 动态客户端注册、S256 PKCE 授权码流程（loopback 回调服务器：自动端口、state 校验、300 秒超时、自动拉起浏览器）、令牌交换 + 刷新授权
- 令牌/注册信息/元数据以 0600 持久化于 `<home>/mcp-tokens/<server>.{json,client.json,meta.json}`（hermes 布局）
- 无人值守会话（cron）使用缓存/可刷新令牌，否则快速失败
- 远程请求收到 401 触发一次恢复（刷新 → 重新授权，按服务器去重）并重放一次请求。P241 移植 stdio 环境过滤（hermes `_build_safe_env` +
  `_interpolate_env_vars`）：stdio MCP
  子进程以清空的环境启动，再叠加过滤副本——仅安全基线变量（PATH/HOME/USER/LANG/LC_ALL/TERM/SHELL/TMPDIR 及大小写不敏感的 Windows
  进程/位置变量集）、`XDG_*` 变量、密钥源标记变量（存在于 `<home>/.env`——即 ulnclaw 自身加载的 dotenv 层——中的键）以及服务器 `env`
  块中显式声明的变量得以透传，环境中的 API 密钥/令牌/凭据不会泄漏给 MCP 服务器
- `command`/`args`/`env` 值中的 `${VAR}` 与 Cursor 风格 `${env:VAR}`
  占位符先查密钥作用域、再查进程环境解析，未设置的引用保留字面占位符。P242 移植 dashboard 中介 OAuth 桥（hermes `tools/mcp_dashboard_oauth.py`
  + `web_server.py` 流程登记表/路由）：网关提供 `POST /api/mcp/servers/<name>/auth`（校验服务器为远程且走 OAuth——stdio 服务器用
  env 键认证、头部认证服务器用头部
- 进行中流程上限 8 → 429、按服务器去重 → 409、15 分钟 TTL GC）、`GET /api/mcp/oauth/flows/<flow_id>`（状态 +
  工具列表）与浏览器重定向目标 `GET /api/mcp/oauth/callback/<server>`——开放路由（不经 bearer 门），以 `state` 参数为保护：常量时间比较、重放以
  409 拒绝、provider 错误返回 400
- OAuth 机制经任务本地变量（hermes contextvar）侦测当前流程，将授权 URL 的发布/回调的等待改经流程而非绑定 loopback 端口，redirect_uri 解析为网关回调 URL，并在动态客户端注册时保持一致登记
- worker 在强制重新授权前备份已存令牌、失败时恢复，随后以新令牌探测服务器工具列表（展示于流程快照）。分歧：ulnclaw 网关是不持有代理工具注册表的独立守护进程，故跳过 hermes
  的进程内实时重连——新令牌于下一会话/懒启动时生效。P243 移植 OAuth 管理器加固（hermes `tools/mcp_oauth_manager.py`）：进程级
  `OAuthManager`（`src/mcp/oauth_manager.rs`）维护按服务器的令牌文件 mtime 水位——请求前磁盘监视（每次 POST 一次 `stat()`，hermes
  `HermesMCPOAuthProvider` 流程前重载）使另一进程在磁盘上刷新的令牌（cron 任务、dashboard 桥、其他 profile）无需重启即可被拾取
- 401 恢复按失败的访问令牌去重，经共享的在途 future 合并（hermes `handle_401` / `pending_401`，设计参考 Claude Code
  `pending401Handlers`）：同一令牌上 N 个并发 401 只触发一次恢复——先走外部刷新短路，再走刷新授权/重新授权——所有等待者共享结果，且由派生驱动任务（hermes
  `_inflight_tasks`）保证即使首个请求者被取消恢复也会完成
- reauth 钩子自此携带失败令牌。P244 接通消息适配器投递路径（hermes `gateway/run.py` 拦截 + `_request_slash_confirm` +
  `platforms/base.py send_slash_confirm`）：平台聊天中的 `/reload-mcp` 由 `approvals.mcp_reload_confirm`
  门控，发送前先登记待决确认（杜绝按钮点击竞态），平台覆写 `PlatformSender::send_slash_confirm` 时渲染原生按钮，否则走文本回退
- 入站回复在派发前拦截：`/approve`/`yes`/`ok`/`confirm` → once、`/always`/`remember` →
  always、`/cancel`/`no`/`deny`/`nevermind` → cancel（slash 形式、纯文本与 `!` 前缀 Slack
  风格皆可），在途工具审批优先，无关消息放行且过期确认被丢弃
- 确认后以最新配置执行重载、报告 reconnected/added/removed 与工具数，并在会话历史末尾注入 `[system note]` 变更说明（提示词缓存前缀得以保留）
- 选 `always` 持久化 `approvals.mcp_reload_confirm = false`

<a id="mcp-频道桥-mcp-serve-py"></a>

#### MCP 频道桥（`mcp_serve.py`） — ✅ 核心

- P260 完整移植 hermes `mcp_serve.py`（`src/mcp_serve.rs`，`ulnclaw mcp serve [--verbose]`）：stdio JSON-RPC
  2.0 MCP 服务器，向任意 MCP 客户端（Claude Code、Cursor、Codex 等）暴露消息会话。OpenClaw 9 工具桥面 + hermes 特有
  `channels_list`：`conversations_list`（platform/search 过滤、limit 1–200
  钳制）、`conversation_get`、`messages_read`（仅 user/assistant、2000 字符上限、按时间序 +
  total_in_session）、`attachments_fetch`（按消息行 id 抽取
  `MEDIA:<path>`）、`events_poll`/`events_wait`（游标队列、1000 事件上限、200 ms state.db-mtime 门控轮询 + 启动基线——hermes
  #13414 不重放语义——长轮询上限 300 秒）、`messages_send`（复用 P259 `send_message` 逻辑，含无存活网关时 Telegram/Discord/Slack
  独立 REST 投递）、`channels_list`（频道目录 +
  会话索引回退）、`permissions_list_open`/`permissions_respond`（桥会话内审批台账、decision 校验）。平台会话取自 `state.db` 中
  `platform-<名>-<聊天>` 键的行（经 `COALESCE(session_key, id)` 兼容 hermes #9006 形态）
- 展示名以标题 + 频道目录富化

<a id="acp-适配器-acp-adapter"></a>

#### ACP 适配器（`acp_adapter/`） — ✅ 核心

P261 移植 hermes `acp_adapter/`（`src/acp_adapter.rs`，`ulnclaw acp [--verbose]`）：面向编辑器（Zed）的 Agent
Client Protocol stdio 服务器。双向换行分隔 JSON-RPC 2.0，出站请求按 id 匹配响应。表面：`initialize`（协议 v1、loadSession +
image/embeddedContext 提示能力）、`authenticate`（不广播认证方式）、`session/new` / `session/load`（从
`acp-<sessionId>` 存储行回放历史为 user/agent 消息块）、`session/prompt`（文本 + 图像块 → 经 `run_with_session_images`
的原生多模态回合，流式 `session/update` 通知：`agent_message_chunk`、`agent_thought_chunk`、带 kind 映射 + 未结束调用 id 队列的
`tool_call`/`tool_call_update`、todo 结果转 `plan` 更新——hermes events.py 对等）、协作式
`session/cancel`、`session/set_mode`/`session/set_model` 应答、未知方法 `-32601`（hermes 良性探针语义）。工具审批经
`session/request_permission`（Allow Once / Always Allow / Reject——hermes edit_approval 对等）

<a id="批量运行器-batch-runner-py"></a>

#### 批量运行器（`batch_runner.py`） — ✅ 核心

P262 完整移植 hermes `batch_runner.py`（`src/batch_runner.rs`，`ulnclaw batch --dataset-file X --run-name
R [--batch-size N] [--num-workers N] [--resume] [--verbose] [--max-iterations N] [--model M]`）：JSONL
数据集逐行校验加载，并行批处理（tokio `buffer_unordered` 工作池——批间并行、批内提示词串行，hermes Pool
语义），检查点（`batch_runs/<run>/checkpoint.json`：完成索引 + 每批统计、原子 tmp+rename 写入、run_name 守卫加载），智能续跑（已存
`batch_*.json` 内容扫描 ∪ 检查点索引），hermes from/value 轨迹格式（`system`/`human`/`gpt` 回合，`<tool_call>` +
`<tool_response>` XML 配对、推理草稿透传），工具用量统计聚合（JSON 错误形态失败检测，含 terminal 嵌套 `content.error` 与
`success:false` 模式）、推理覆盖统计与最终 `summary.json`

<a id="send-cli-hermes-cli-send-cmd-py"></a>

#### Send CLI（`hermes_cli/send_cmd.py`） — ✅ 核心

P263 完整移植 hermes `hermes send`（`ulnclaw send`）：脚本/cron/CI 消息投递，无 LLM、无 agent 循环——`--to
platform[:chat[:thread]]` 目标语法，消息体取自位置参数 / `--file PATH` / `-` / 管道 stdin（TTY 感知），`--subject`
标题前置，`--list [platform]` 频道目录渲染并合并已配置未发现的平台，`--json` 原始结果、`--quiet` 仅退出码模式，hermes 退出码契约（0 成功 / 1
投递失败 / 2 用法错误）。复用 P259 `send_message` 管道，含 Telegram/Discord/Slack 独立 REST 路径（bot 令牌平台无需运行网关）

<a id="slack-原生斜杠命令-应用清单-hermes-cli-slack-cli-py-slack-适配器斜杠流"></a>

#### Slack 原生斜杠命令 + 应用清单（`hermes_cli/slack_cli.py` + slack 适配器斜杠流） — ✅ 核心

- P265 移植 hermes `hermes slack manifest` + Slack socket-mode 斜杠路径：`ulnclaw slack manifest [--write
  [PATH]] [--name N] [--description D] [--long-description T | --long-description-file F]
  [--slashes-only] [--no-assistant | --agent-view]` 生成 Slack 应用清单，把每个平台命令注册为原生斜杠命令（`/ulnclaw`
  兜底命令恒居首位、hermes 保留命令跳过表、50 条上限、命令名净化、175–4000 字符长描述校验、assistant/agent/none 三种消息体验及对应 scopes +
  events）。Slack 适配器消费 `slash_commands` socket 信封：经授权门控走常规平台分发路径，回复经 `response_url`
  投递（`replace_original`，postMessage 回退）——hermes `_handle_slash_command` 语义。新增共享层
  `platform_slash.rs`：hermes 网关直答命令子集（`/help` `/skills` `/tools` `/recap` `/title` `/usage`
  `/insights`）在**所有**消息平台免 LLM 回合直答
- `/skill-name` + `/<bundle>` 展开为脚手架 agent 回合（与网关会话聊天对齐）

<a id="网关监控-otlp-导出-agent-monitoring"></a>

#### 网关监控 + OTLP 导出（`agent/monitoring/*`） — ✅ 核心

P266 完整移植 hermes 网关监控平面（`src/monitoring.rs`）：无内容
`GatewayHealthEvent`/`GatewayDiagnosticEvent`/`CronExecutionEvent` 投影，全局有界发射器（fire-and-forget，1024
上限丢弃）、无条件出站脱敏（secrets→PII：bearer/token/`***`/email/uuid/电话清洗——hermes `_PHONE_RE` 语义的无 lookbehind
移植）、持久化 install-id 铸造、`[monitoring]` 配置（hermes 默认值 + 间隔下限：健康 60s / 诊断 5s，下限 5/1）、内置 OTLP/HTTP JSON
trace 导出器（resourceSpans/scopeSpans、按 span 类型的 `ulnclaw.*` 属性白名单 + 500 字符截断、`headers_env`
导出时解析、`/v1/traces` 只追加一次、超时刷新的批量循环）。网关接线：`gateway_started` 生命周期事件、基于实时运行状态 + 启用平台数的周期性心跳采样、来自调度器的
cron 执行事件、`ulnclaw monitoring status` 界面。范围仅健康 + 脱敏诊断——不含提示词、消息、工具参数/结果或用量分析

<a id="安装向导-hermes-cli-setup-py"></a>

#### 安装向导（`hermes_cli/setup.py`） — ✅ 核心

P267 精简移植 hermes 交互式安装向导（`src/setup_cmd.rs`）：`ulnclaw setup [model|terminal|gateway|tools|agent]
[--quick] [--reset] [--non-interactive]` —— 首次安装模式选择（完整安装 / 空白起点）、既有安装的重配置流程（每个提示以当前值为默认）、按 section
单独运行、修改前的时间戳配置备份（#3522）、无 TTY 时的非交互指引、provider
挑选器（OpenAI/Anthropic/OpenRouter/DashScope/Ollama/llama.cpp/自定义）且 API key 写入
`.env`、终端后端挑选器（local/docker/ssh）及各自字段、26 平台消息清单（按平台令牌提示 + 后续指引：`qq setup` / `weixin login` /
config.toml 说明）与 hermes 缺失 home 频道提示、基于实时注册表的 toolset 多选、agent 设置，以及向导结束摘要。相对 hermes
的精简（已记录为差异）：Nous Portal 快速安装、OpenClaw 迁移提议、curses 空格切换清单（改为逗号分隔数字多选）、TTS/telemetry 段落

<a id="模型挑选器-cmd-model-select-provider-and-model"></a>

#### 模型挑选器（`cmd_model` / `select_provider_and_model`） — ✅ 核心

P268 移植 hermes `hermes model`：`ulnclaw model [--refresh]` —— 需 TTY 的交互式 provider + 模型切换器。Provider
清单（内置 + 用户 `[providers.<slug>]` 条目，当前项标记/默认）、缺失时凭据提示写入 `.env`、models.dev 目录模型列表（agentic 模型、40
行上限、目录离线/未知时回退“手动输入模型名”）、配置持久化、切换后摘要（provider/模型/端点/密钥状态）。`--refresh` 先清空 models.dev 挑选器缓存。同时加固
`models_dev::fetch_from_network`：reqwest 阻塞客户端在 scoped OS 线程内创建/销毁，目录抓取在异步网关/分发上下文中不再 panic

<a id="桌面启动器-命令别名-cmd-gui-cmd-login-cmd-logout-journey-别名"></a>

#### 桌面启动器 + 命令别名（`cmd_gui` / `cmd_login` / `cmd_logout` / journey 别名） — ✅ 核心

- P269 移植 hermes 剩余的小型界面：`ulnclaw gui [--binary PATH] [--dev]`（别名 `desktop`）解析已打包的 `ulnclaw desktop`
  可执行文件（`--binary` → `ULNCLAW_DESKTOP_BINARY` →
  `desktop-electron/release/{linux,win,mac}-unpacked`）并分离启动，`--dev` 在 `desktop-electron/` 经 `npm
  start` 运行未打包应用，二进制缺失时打印构建指引
- 顶层 `ulnclaw login` / `ulnclaw logout` 作为 `auth login` / `auth logout` 的别名（hermes `cmd_login`/`cmd_logout`）
- `ulnclaw learning` / `ulnclaw memory-graph` 作为 `journey` 的可见别名（hermes journey 解析器别名）

<a id="动态-webhook-订阅-hermes-cli-webhook-py"></a>

#### 动态 webhook 订阅（`hermes_cli/webhook.py`） — ✅ 核心

- P270 移植 hermes `hermes webhook`：`ulnclaw webhook subscribe <name> [--description D] [--events
  e1,e2] [--secret S] [--prompt P] [--skills s1,s2] [--deliver target] [--deliver-chat-id C]
  [--deliver-only] [--script CMD]`、`webhook list`、`webhook remove <name>`、`webhook test <name>
  [--payload JSON]`（HMAC-SHA256 签名 POST）。订阅持久化于 `webhook_subscriptions.json`（原子写入、0600 权限——存放每路由 HMAC
  密钥
- 命名语法 + 归一化、随机 64 位十六进制密钥铸造、hermes created_at 格式、`--deliver-only` 校验）。网关侧：`dynamic_webhook_route`
  挂载 `/webhooks/:name`（静态平台路由保持优先），每次请求热加载订阅文件——无需重启——并经抽取的 `process_generic_webhook` 辅助函数把每个订阅映射进既有通用
  webhook 管道（签名/限流/幂等/事件过滤/投递）

<a id="whatsapp-cloud-安装向导-hermes-cli-setup-whatsapp-cloud-py"></a>

#### WhatsApp Cloud 安装向导（`hermes_cli/setup_whatsapp_cloud.py`） — ✅ 核心

- P271 精简移植 hermes `hermes whatsapp-cloud`：`ulnclaw whatsapp-cloud` —— 需 TTY 的交互式向导，带 hermes
  字段形状校验器（Phone Number ID 与误贴电话号码陷阱、`EAA` 访问令牌前缀 + OpenAI/Slack/GitHub 误贴诊断、32 位十六进制 App Secret）、自动铸造
  verify token（可重新生成）、收件人白名单归一化，以及按 ulnclaw 路径改写的 SETUP COMPLETE 后续步骤（cloudflared 隧道、网关、Meta webhook
  面板
- `/webhooks/whatsapp`、网关端口）。凭据持久化到 config.toml 的 `[messaging.whatsapp_cloud]`（ulnclaw 解析方式）而非 `.env`
- 仅用于分析的 App ID/WABA ID 步骤省略（无消费方）
- 保留 hermes 退出码契约（0 成功 / 1 中止 / 2 部分完成）

<a id="cli-hermes-cli"></a>

#### CLI（`hermes_cli/`） — ✅ 核心

- 带斜杠命令的聊天 REPL（含 `/rollback [N|hash] [file]`、`/rollback diff <N>`、`/diff` 检查点命令、`/recap`、`/goal` +
  `/subgoal` 既定目标循环、`/kanban` 看板内联操作、`/egress` 出站代理状态、`/pet` + `/hatch` petdex 界面）、一次性
  `run`、sessions/tools/skills/cron/checkpoints 子命令（含 `sessions export --format md\|html` —— SHA256 校验的
  Markdown 或独立 HTML + manifest ——、`sessions recap`、`sessions recover`、`sessions
  prune`/`archive`/`stats`/`delete`/`rename`/`optimize`/`repair`/`browse`/`retitle-skills`、`kanban
  init`/`boards list|create|rm|switch|show|rename|set-workdir`/`create [--project P]`/`list
  [--workflow-template-id
  T]`/`show`/`ready`/`assign`/`claim`/`heartbeat`/`done`/`block`/`unblock`/`archive`/`comment`/`link`/`unlink`/`dispatch
  [--max-spawn N] [--dry-run]`/`gc`/`swarm <goal> --worker ASSIGNEE:TITLE[:skill,skill] [--worker ...]
  --verifier ASSIGNEE --synthesizer ASSIGNEE [--idempotency-key K] [--json]`/`specify [id |
  --all]`/`decompose [id | --all]`/`diagnostics [id] [--min-severity S] [--json]`/`schedule`/`promote
  [--force]`/`reclaim`/`reassign [--reclaim]`/`edit`/`set-model [--provider
  P]`/`attach`|`attachments`|`attach-rm`/`tail [--follow]`/`stats [--json]`/`watch [--assignee P]
  [--kinds K] [--interval S]`（kanban 任务引擎：boards、带 TTL 的认领锁 + 过期接管、带图标的 hermes
  状态生命周期、评论与事件轨迹、`kanban_task_*` 插件钩子）、`project
  create/list/show/add-folder/remove-folder/rename/set-primary/use/archive/restore/bind-board/scan
  [--root P --max-depth N]/repos [--clear]`（锚定 kanban worktree 的一等项目登记簿 + git 仓库发现缓存）、`secrets
  status/sync/bitwarden setup|install|status|disable/onepassword
  setup|status|set|remove|disable`、`egress
  install/setup/start/stop/restart/reload/status/disable/config`（托管 iron-proxy 沙箱出站防火墙）、`computer-use
  status/doctor/install`、`plugins list/install/update/remove/enable/disable/accept-hooks`、`hooks
  list/test/revoke/doctor`、`pairing list/approve/revoke/clear-pending`、`weixin login`（微信 iLink
  扫码登录）、`auth login/status/refresh/logout`、`slack manifest`（Slack 应用清单生成器，注册原生斜杠命令）、`monitoring
  status`（网关监控 / OTLP 导出状态）、`setup [section] [--quick] [--reset]`（交互式安装向导）、`model [--refresh]`（交互式
  provider + 模型切换器）、`gui [--binary PATH] [--dev]`（桌面启动器
- 别名 `desktop`）、`login`/`logout`（auth 别名）、`webhook subscribe|list|remove|test`（动态 webhook
  订阅）、`whatsapp-cloud`（WhatsApp Business Cloud API 安装向导）、`whatsapp [status]`（Baileys 桥诊断）、`sync
  status/pull/push/now/enable/disable/device`、`uninstall --full/--dry-run/--yes`（代码检出 + shell PATH 条目
  + 包装符号链接 + 可选清除主目录
- hermes `uninstall.py` 移植 —— Windows 注册表/环境变量步骤未移植））、`moa run/list/delete`、`models
  providers/list/info/refresh`（models.dev 目录）、`skills blueprints/schedule/unschedule`、`diff`、`init`

<a id="混合智能体-moa-moa-loop-py-moa-config-py"></a>

#### 混合智能体 MoA（`moa_loop.py`、`moa_config.py`） — ✅ 核心

- `[moa.presets.<name>]` 参考模型并行扇出 + 聚合器综合（`ulnclaw moa run/list/delete`、REPL `/moa <prompt>`）
- loud/silent 降级策略、全部失败提前返回、聚合失败回退拼接结果
- P169 移植了持久门面 + trace + 隐私过滤：`[model] provider = "moa"` 让整个 agent 循环跑在预设上（`model` 选择预设——hermes
  `build_moa_facade`）：按用户轮参考扇出并在工具循环迭代间缓存（hermes reference
  cache）、指引附加在聚合器提示末尾（`_attach_reference_guidance`，prompt 缓存友好）、调用方的工具转发给聚合器、聚合失败回退返回指引块
- `[moa] save_traces`/`trace_dir` 按消毒后的会话 id 在 `moa-traces/` 下逐轮写入 JSONL 记录（hermes `save_moa_turn`）
- `privacy_filter = display|full` 用集中式密钥形态 + MoA 邮箱/格式化电话模式脱敏参考输出（`display`），`full` 额外脱敏注入聚合器提示的文本（hermes #59959）

<a id="http-网关-gateway-platforms-api-server-py"></a>

#### HTTP 网关（`gateway/platforms/api_server.py`） — ✅ 核心

- `ulnclaw gateway`：OpenAI 兼容 `/v1/chat/completions`（`X-Ulnclaw-Session-Id` 会话续接、`stream: true` SSE
  令牌流 + `hermes.tool.progress` 事件）、`/v1/responses`（经 `previous_response_id` 有状态续接、`stream: true`
  Responses-API SSE 事件）、`/v1/models`、`/api/model/options` 多 provider 选择器清单（P168，hermes `inventory.py`
  移植：当前 provider 行经 models.dev 增强含能力/成本映射、`[providers.<slug>]` 配置行带有界 `/models` 实时探测、env 密钥认证的规范
  provider 行、`include_unconfigured` 骨架行带 `auth_type`/`key_env`/`warning` 配置提示、`explicit_only`
  过滤、规范声明顺序、按实验室精选模型 + 格式化定价、`[model_catalog] excluded_providers`
- `?refresh=true` 刷新目录缓存并重新探测）、`/v1/capabilities`、`/v1/runs`（异步运行 + SSE 事件 + 停止 +
  审批）、`/api/sessions` 增删查改 + 会话聊天 +
  chat/stream（斜杠直通：`/help`/`/skills`/`/tools`/`/recap`/`/title`/`/usage` 免 LLM 回合直接执行
- `/skill-name` + `/<bundle>` 调用展开为 hermes 技能脚手架用户轮——复刻 hermes gateway/run.py 的技能命令共享）+
  `PATCH`（title/end_reason）+ `fork` + 会话级模型锁（每轮生效）+ `recap` + `POST /api/sessions/prune|archive`
  批量清理（可过滤、dry-run 预览——对应 `sessions prune/archive`）、`/api/jobs` 定时任务 HTTP API（增删查改 + pause/resume/run
  + `GET /api/jobs/delivery-targets` + `POST /api/jobs/fire` Chronos NAS 触发 webhook——P219：`deliver` 目标
  `local`/`origin`/平台名/`platform:chat[:thread]`/`all` 持久化于任务并在触发时按已注册平台发送器与 home 频道环境变量解析，`[SILENT]`
  抑制、带抬头的包装投递、失败摘要与 `last_delivery_error` 跟踪
- 触发 webhook 校验 `[cron.chronos]` 配置的 NAS 铸造 JWT（RS/ES、JWKS URL 或内联 PEM、`purpose=cron_fire`），应答
  401/400/200-gone/202 并后台运行任务、按认领去重重试）、`/v1/skills`、`/v1/toolsets`、`/metrics`（Prometheus
  计数器/量表——ulnclaw 运维扩展）、`/api/usage`（令牌核算：进程计数器 + 全时会话库总量 + 按会话明细——ulnclaw
  运维扩展）、`/v1/delegations`（后台委派登记——ulnclaw 运维扩展）、`/v1/browser/status|connect|disconnect`（实时 CDP 端点控制，对齐
  hermes `/browser connect`——ulnclaw
  运维扩展）、`/api/uploads`（二进制上传入内容寻址媒体缓存——桌面剪贴板图片粘贴）、`/api/fs/*`（网关文件系统浏览：`list`
  带隐藏/供应商目录过滤、`read-text`/`write-text` 带大小上限与原子临时文件改名、`read-data-url` 供附加下载、`git-root` 与 `default-cwd`
  探测、`download` 附加流式下载支持 `?token=` 查询鉴权 + `mkdir`（P335，对应 hermes `/api/files/download|mkdir`）——对应
  hermes `/api/fs`）、`/api/media`（将网关本机图片以 base64 data URL 服务——图片扩展名白名单、25 MiB 上限、限定于网关
  `images`/`screenshots`/`cache`/`media-cache` 媒体根
- 对应 hermes `/api/media`——P338）、`/api/learning/graph` + `/api/learning/node`
  GET/PUT/DELETE（学习"星图"：已学非基础技能 + MEMORY.md/USER.md 记忆块，含 related-skill 与词汇重叠连边
- 节点详情/归档/编辑变更——hermes web_server `/api/learning/*` 对位，CLI 对位 `ulnclaw journey`）、`/api/projects`
  项目登记簿增删查改（文件夹、主目录、归档/恢复、活跃指针、board 绑定含 workdir 镜像）与 `/api/projects/scan|repos`
  发现端点（P162）、`/api/backups` 快速快照（清单/创建/清理 + 按快照恢复——hermes `/api/ops/backup` 对位，CLI 对位 `ulnclaw
  backup`）、Bearer 令牌鉴权。单实例守卫（P155，hermes `gateway run --replace` 契约）：运行中的网关写 `<home>/gateway.pid`（pid
  + `/proc` 启动时刻令牌——防 PID 复用误判）
- 新启动在已有存活实例时拒绝（过期记录自愈清理），`--replace` 终止旧实例（SIGTERM → SIGKILL 升级）并接管，`--force` 并行运行

<a id="camofox-后端-tools-browser-camofox-py"></a>

#### Camofox 后端（`tools/browser_camofox.py`） — ✅ 核心

- `browser/camofox.rs`：`CAMOFOX_URL` REST 反检测浏览器（Camoufox）后端——全部 12 个 browser 工具经 REST
  路由（标签页会话、带元素引用的可访问性快照、点击/输入/滚动/后退/按键、从快照提取图片、截图供视觉分析）
- CDP 覆盖优先
- `CAMOFOX_API_KEY` bearer 鉴权、`CAMOFOX_USER_ID`/`CAMOFOX_SESSION_KEY` 身份覆盖 + 已有标签页收养、Docker 环回 URL
  重写（`CAMOFOX_REWRITE_LOOPBACK_URLS` + 别名）、从 `/health` 发现 VNC URL、读取操作的 SSRF 私有页面防护、console/原始
  CDP/对话框明确报不支持
- `CAMOFOX_MANAGED_PERSISTENCE` 受管持久化（稳定的 UUIDv5 profile 级 userId，对应 hermes `browser.camofox.managed_persistence`）
- 网关与 REPL browser status 报告后端

<a id="云浏览器-provider-agent-browser-provider-py-agent-browser-registry-py-plugins-browser"></a>

#### 云浏览器 provider（`agent/browser_provider.py` + `agent/browser_registry.py` + `plugins/browser/*`） — ✅ 核心

`browser/cloud.rs`：`CloudBrowserProvider` trait +
注册表移植——**Browserbase**（`BROWSERBASE_API_KEY`+`BROWSERBASE_PROJECT_ID`，`POST
/v1/sessions`，keepAlive/proxies/advancedStealth/timeout 开关及 hermes 402 回退链，`REQUEST_RELEASE`
关闭）、**Browser Use**（直连 `BROWSER_USE_API_KEY` 或经 `[browser] use_gateway` 的受管 Nous gateway，`POST
/browsers` 含 `X-Idempotency-Key` 受管语义、`timeoutAt` 过期权威、`PATCH {action: stop}`
关闭）、**Firecrawl**（`FIRECRAWL_API_KEY`，`POST /v2/browser` 含 `FIRECRAWL_BROWSER_TTL`，DELETE 关闭）。解析复刻
`_resolve`：`"local"` 彻底禁用云模式，显式名称无论可用性一律胜出（给出精确的缺凭据错误），否则按 browser-use → browserbase
传统顺序按可用性遍历——firecrawl 仅限显式选择，避免 web-extract 密钥被静默导向付费云浏览器。`with_session` 惰性创建/缓存会话、退役过期端点，`main`
退出时释放会话（hermes atexit 清理）。端点解析新增 `[browser] cdp_url` 配置层（env > config > cloud > 托管启动）


<a id="存储布局"></a>

## 存储布局

```
~/.ulnclaw/                 （支持 ULNCLAW_HOME 覆盖；兼容 HERMES_HOME 以便迁移）
├── config.toml             主配置
├── .env                    KEY=VALUE 密钥（进程环境变量优先）
├── state.db                SQLite：sessions、messages、cron_jobs、meta（+FTS5）
├── kanban.db               kanban 看板
├── memory/MEMORY.md        代理记忆
├── memory/USER.md          用户画像
├── skills/<name>/SKILL.md  技能
├── sessions/*.todos.json   每会话 todo 列表
├── images/  audio/         生成的产物
├── sandboxes/              execute_code 脚本
├── approvals.json          持久化的 "always" 审批授权
├── media-cache/            内容寻址的消息平台媒体缓存
├── pairing/                DM 配对存储（{platform}-pending/approved.json）
├── shell-hooks-allowlist.json   钩子同意记录
└── checkpoints/store/      共享 shadow git 存储（按项目 ref/index）
```

<a id="已知差异"></a>

## 已知差异

- CLI 上审批交互为终端 y/N 提示（hermes 有更丰富的平台化流程）；网关通过
  HTTP 暴露运行审批（once/session/always/deny）。chat-completions 请求没有
  run 上下文，确认级命令按设计自动拒绝。Smart 审批（LLM 守护）与 cron
  审批模式已移植；无人值守的运行默认 fail-closed，`cron_mode = "approve"`
  可放行。
- 浏览器监督器直接启动本地 Chrome/Chromium；hermes 驱动外部 `agent-browser`
  守护进程。Camofox REST 后端已移植（含受管持久化，以
  `CAMOFOX_MANAGED_PERSISTENCE` 环境变量替代 hermes 的 config.yaml 开关）；
  云浏览器 provider（Browserbase、Browser Use、Firecrawl）已在
  `browser/cloud.rs` 移植。
- 网关实现了 api_server 平台的子集（多 profile 复用
  `/p/<profile>/...` 已移植 —— 见功能表）。
- `/api/model/options` 清单：hermes 的凭据池行与 Nous 免费档门控未移植
  （凭证池存储本身已精简移植——见凭证池行——但选择器仍按 provider 单行展示）；多 provider 行集、选择器
  提示、精选模型与定价已移植（P168，见 HTTP 网关行）。
- 压缩使用 字符数/4 的 token 估算而非分词器。
- `patch` 模糊链实现了全部 9 种 hermes 策略；相似度基于 LCS 比率
  （difflib.SequenceMatcher 的等价实现），边界阈值与 CPython 实现可能略有差异。
- 环境覆盖 local/docker/ssh；hermes 的 modal/daytona/vercel 后端及其凭据
  流程未移植。
- 检查点跳过 hermes 的 pre-v2 旧存储迁移（仅全新存储），孤儿判定使用
  工作目录存在性（不记录卷设备/inode 证据）。
- Secrets：`secrets sync` 干跑对比的是启动钩子已应用后的 env（hermes 行为
  相同）；Bitwarden `token` 轮换子命令已移植（`secrets bitwarden token
  [--access-token] [--no-verify]` —— 先对新令牌做 Bitwarden 校验再落盘，
  粘错令牌不会弄坏可用的旧令牌，并清除旧令牌指纹缓存）；bws 自动安装的
  Windows 资产路径未测试。
- Computer-use：驱动载荷（SOM 截图 b64、AX 树）直接透传，不含 hermes 的 PNG
  后处理/多模态驱逐层；macOS TCC 授权流程已暴露给桌面端
  （`/api/tools/computer-use/permissions/grant` 以可轮询后台动作拉起
  `cua-driver permissions grant`，仅 macOS）；嵌入式守护进程 socket 模式
  未移植；`install` 直接调用上游 trycua 安装脚本。
- 插件：ulnclaw 插件是讲 hermes shell-hook JSON 协议的子进程（目录插件 +
  `[hooks]` 配置），不是 Python 导入；核心触发 hermes v2026.8.3 运行期
  实际发出的全部钩子（23 个中的 13 个 —— 其余 10 个在 hermes 中也仅存于
  目录）；pre_verify 无 ulnclaw verify 循环可挂载；`ulnclaw kanban` 引擎
  现已在 claim/done/block 时触发 kanban_task_claimed/completed/blocked
  钩子，agent 侧 kanban_* 工具现已改用同一 KanbanStore 引擎
  （P119 统一了此前独立的表）；P122 移植了调度器 tick（`kanban dispatch` CLI +
  `POST /api/kanban/dispatch`：过期认领回收（存活 pid 自动续期）、父任务完成的
  todo→ready 晋升、就绪任务经分离的 `ulnclaw run` 生成 worker（ULNCLAW_KANBAN_TASK
  环境）、实时并发上限、连续 2 次 spawn 失败自动阻塞）；P123 新增网关内嵌定时调度
  （`[kanban] dispatch_in_gateway / dispatch_interval_secs / max_spawn`，默认开/60 秒/2）
  与 hermes kanban-stop 提醒（worker 未调用 kanban_complete/block 就结束时最多重新
  提示 2 次，`ULNCLAW_KANBAN_STOP_NUDGE=0` 可关闭）；P124 补齐了按任务
  git-worktree 隔离（`[kanban] worktrees`，默认开启：每个被调度的 worker 运行在
  `<repo>/.worktrees/<task-id>` 的 `kanban/<task-id>` 分支上，重启可复用；
  `ulnclaw kanban gc` 清理已完成/归档任务的树，分支保留）；P125 移植了 hermes
  kanban swarm（`hermes_cli/kanban_swarm.py`）：`ulnclaw kanban swarm <goal>
  --worker ASSIGNEE:TITLE [--worker ...] --verifier ASSIGNEE --synthesizer
  ASSIGNEE [--json]` 构建 workers→verifier→synthesizer 任务图 ——
  根黑板/审计任务（创建即 done）、N 个注入 swarm 协议简报的 ready worker、
  链接到所有 worker 的验证者、链接到验证者的综合者；拓扑以 `blackboard` 评论 +
  `swarm` 事件发布，调度器随父任务完成逐级晋升验证者/综合者
  （`recompute_ready`）；P127 补齐 swarm 面：worker 技能透传
  （`--worker ASSIGNEE:TITLE:skill,skill`，验证者固定
  `requesting-code-review`、综合者固定 `humanizer` —— 与 hermes 逐字一致）、
  任务级 `skills`/`max_runtime_seconds`/`idempotency_key` 列（增量迁移；
  `kanban create --skill X --max-runtime N --idempotency-key K`，网关创建
  API 同字段）、幂等 swarm 恢复（同键 ⇒ 从根黑板重建拓扑，不重复建图）、
  调度器 `reap_timed_out`（SIGTERM + 5 秒宽限 + SIGKILL，任务回 ready 并记
  `timed_out` 事件），派生 worker 的启动提示词内联强制加载的技能
  （hermes 以 `--skills` 参数对传递）；P128 移植了 triage 流水线
  （`hermes_cli/kanban_specify.py` + `kanban_decompose.py`）：
  `kanban create --triage` 把想法暂存进新的 `triage` 列，`kanban specify`
  经 `auxiliary.triage_specifier` 生成 Goal/Approach/Acceptance-criteria
  规格并晋升 triage→todo，`kanban decompose` 将其扇出为 2-6 个子任务
  依赖图并按 profile 名册路由（`[kanban] orchestrator_profile /
  default_assignee / auto_promote_children`；根任务作为所有子任务的父级
  存留作唤醒卡，Kahn 环检测，--all 批量时对单任务失败容错），
  `kanban diagnostics` 移植 `kanban_diagnostics.py` 规则引擎（幻觉卡号、
  正文幻影引用、重复 spawn 失败、worker 崩溃循环、阻塞超 24 小时、
  block/unblock 循环、滞留 ready、triage 无辅助模型），阈值与严重度
  排序与 hermes 一致；P129 补齐了其余 hermes kanban CLI 面：
  schedule/promote（父任务门控、--force 覆盖）/reclaim/reassign
  （--reclaim）/edit/set-model、附件 CLI（attach/attachments/attach-rm，
  稳定 id）、tail --follow 事件流、按看板状态统计、boards
  rename/set-workdir；P130 新增全板 `kanban watch` 实时事件流
  （assignee/kind 过滤，hermes watch 后端）、`kanban stats` 采用 hermes
  `board_stats` 语义（按 assignee 统计 + 最老 ready 等待时长 +
  `--json`）与 `kanban dispatch --json`；P131 移植网关通知底座：
  `kanban_notify_subs` 表（task × platform × chat × thread 主键，
  订阅时游标快照到当前最新事件、chat_type/profile/metadata 自愈）、
  `kanban notify-subscribe / notify-list / notify-unsubscribe` CLI、
  供网关通知器使用的 `unseen_events_for_sub` + `advance_notify_cursor`
  构件，以及 `kanban log [--tail N]` —— 打印任务在
  `<home>/kanban/worker-logs/` 下的 worker 日志，tail 采用 hermes 的
  断行安全语义；P132 新增 `task_runs` 尝试历史表（hermes `Run` 生命
  周期：认领时开 run，携带认领锁/TTL 与运行时限，心跳与 spawn 出的
  worker pid 同步写入，按 hermes outcome 语义关闭 —— done/block/
  reclaim/过期回收/超时分别对应 completed/blocked/reclaimed/
  timed_out，CLI 直接完成未认领任务与调度器 spawn 失败则合成瞬时
  run；重新认领时把残留活跃 run 恢复为 `reclaimed`）、
  `kanban runs [--json] [--state-type status|outcome --state-name V]`
  CLI（hermes 表格格式）与 `latest_run` / `latest_summary` 存储
  辅助方法；P133 接通网关调度器的 triage 自动分解路径（hermes
  `_auto_decompose_tick`）：每个 tick 从配置实时重读
  `[kanban] auto_decompose`（默认开）/ `auto_decompose_per_tick`
  （默认 3），翻转开关在下一 tick 即停止失控扇出、无需重启网关
  （hermes #49638 故障安全语义 —— 配置读取失败则本轮跳过），
  随后在 dispatch 扇出之前经辅助 LLM 分解至多 N 个 triage 任务，
  成功记 info、无操作跳过记 debug；P134 补齐 hermes kanban CLI 的
  最后几块：`kanban context`（完整移植 `build_worker_context` ——
  带上限的正文/附件、历史尝试的 run 摘要与 metadata、已完成父任务
  的交接结果与相对时间陈旧度提示、assignee 跨任务角色历史、带上限
  的评论区；`kanban_show` 工具同步返回 `worker_context`，spawn 出的
  worker 无需额外往返即可读取）、`kanban repair`（integrity_check +
  内容寻址隔离备份 + 仅索引损坏的 REINDEX 自动修复，其余情况保守
  失败）、`kanban assignees`（配置名册与看板 assignee 合并、按状态
  计数）、`kanban daemon`（hermes 已弃用的存根，指向网关；`--force`
  保留独立循环）以及 `ls`/`new` 可见别名；P135 接通通知投递：
  网关运行 kanban notifier 循环（hermes kanban_watchers 通知器，
  5 秒一拍），轮询 `kanban_notify_subs`，领取未送达的终态事件
  （completed/blocked/gave_up/crashed/timed_out/status，其中
  archived/unblocked 只推游标不发声，避免堵塞后续事件），按 hermes
  消息格式渲染（✔ 完成 + 交接首行、⏸ 阻塞 + 原因、⏱ 超时、
  ✖ 崩溃/放弃、🔄 状态变更，带 @assignee 与 [board] 标签），经已
  注册的平台发送器投递，投递后推进每个订阅的游标；订阅在任务
  崩溃/重试周期中保留，仅当任务真正 done/archived 时移除（去重靠
  游标）。与 hermes 的范围差异：无按 profile 的适配器归属（单一
  共享存储）、无线程路由与死聊天清理（PlatformSender 无失败通
  道），发送视为已送达；P136 移植统一失败记账与熔断器（hermes
  `_record_task_failure`）：任务新增 `consecutive_failures` /
  `last_failure_error` / `max_retries` 列，每次 spawn 失败与超时
  尝试都消耗重试预算，达到阈值（按任务 `max_retries` > 调度器
  limit > 默认 2）即 ready→blocked 并发出 `gave_up` 事件（payload
  含 failures / effective_limit / limit_source / trigger_outcome），
  计数器在任务完成与主动 unblock 时清零（hermes 重新起步策略）。
  CLI：`kanban create --max-retries N`（校验 >= 1，与 hermes 一
  致），网关创建 API 接受同一字段；P137 补齐调度器的 worker 健康
  检测（hermes `detect_crashed_workers` + `detect_stale_running`）：
  每 tick 立即回收 worker pid 已死亡的 running 任务（30 秒启动
  宽限期，`ULNCLAW_KANBAN_CRASH_GRACE_SECONDS` 可覆盖；发
  `crashed` 事件、run 以 `crashed` 关闭、计入熔断预算），以及运行
  超过 `[kanban] stale_timeout_seconds`（hermes
  `dispatch_stale_timeout_seconds`，默认 14400，0 关闭，网关循环
  实时重读）且心跳缺失或超过 1 小时的任务（worker 先 SIGTERM 后
  SIGKILL，发 `stale` 事件、run 以 `stale` 关闭，按 hermes 策略
  不计为失败）；两者分别进入 `DispatchResult.stale` / `.crashed`；
  P138 加固内嵌调度器的运维安全（hermes gateway 循环）：排他
  `flock` 单例锁（`<home>/kanban/dispatcher.lock`）保证全机只有
  一个网关进程在调度 —— 第二个网关记录竞争日志、继续提供 HTTP
  但不调度（防配置漂移与重启竞争的兜底）—— 并新增调度器卡死
  健康遥测：ready 队列连续 6 拍非空却零 spawn 时告警（300 秒
  节流）；P139 移植 hermes 的按任务工作区：任务新增
  `workspace_kind`（默认 `scratch` / `worktree` / `dir`）、
  `workspace_path`、`branch_name` 三列；`kanban create --workspace
  scratch|worktree|worktree:<path>|dir:<path>` 与 `--branch <名>`
  （仅 worktree 可用，校验文案与 hermes 一致），网关创建 API 接受
  相同字段；调度器在 spawn 之前解析工作区（hermes
  `resolve_workspace` / `_resolve_worktree_workspace`）：scratch 目录
  位于 `<home>/kanban/workspaces/<id>`，`dir:` 路径必须为绝对路径
  （防混淆代理人穿越，沿用 hermes 威胁模型），worktree 以看板
  `default_workdir` 为锚（未配置时回退调度器 CWD，保留 P139 之前的
  行为；hermes 则直接报错），在 `<repo>/.worktrees/<task-id>` 物化
  分支 `wt/<task-id>`（或 `--branch` 指定），被兄弟任务占用的检出
  会自动改用同仓库下的新树；解析出的路径与分支持久化到任务行供
  重试复用，解析失败按 `workspace:` 前缀计入 spawn 失败熔断，
  `kanban claim` 认领时解析并打印工作区（hermes `_cmd_claim`），
  `[kanban] worktrees=true` 对未显式指定 `--workspace` 的任务保持
  原语义，decompose 子任务继承根任务的工作区类型/路径（worktree
  子任务各自独占新树，hermes 兄弟任务策略）；P140 补齐重生守卫
  与时长语法：`kanban create --max-runtime` 接受
  `30s`/`5m`/`2h`/`1d` 与纯秒数（hermes `_parse_duration`）；
  调度器对立即重试无益的就绪任务延后重生（hermes
  `check_respawn_guard`）—— `rate_limit_cooldown`（最近一次 run 以
  `rate_limited` 结束且仍在冷却期内，
  `ULNCLAW_KANBAN_RATE_LIMIT_COOLDOWN_SECONDS` 默认 300，0 关闭）、
  `blocker_auth`（最近失败命中配额/鉴权模式）、`recent_success`
  （1 小时内有已完成 run 且其后无主动重新入队）、`active_pr`
  （24 小时内评论中出现 GitHub PR 链接）；被守卫的任务保持
  ready，每次延后都会记录 `respawn_guarded` 事件，网关 dispatch
  API 一并返回；P141 移植唤醒路由：任务新增 `session_id` 列，由
  agent `kanban_create` 工具写入（网关创建 API 亦接受该字段）；
  订阅任务进入可唤醒终态事件（`completed` / `gave_up` /
  `crashed` / `timed_out` / `blocked` —— hermes `_WAKE_KINDS`）
  时，通知器以 hermes 格式的唤醒文案（`[kanban] Task <id>
  <status>. …`）自 POST 网关自身的 `/v1/chat/completions` 并携带
  `X-Ulnclaw-Session-Id`，恢复创建者会话（hermes
  `_self_post_chat_completion`：通配绑定走回环、配置了密钥则带
  bearer、单轮上限 600 秒、429/瞬时错误按 2/5/10 秒退避重试、
  其余 HTTP 错误快速失败）；唤醒在文本通知之后尽力异步执行，
  不阻塞其他订阅；P142 移植类型化阻塞（hermes
  `block_task(kind=…)`）：`kanban block --kind dependency` 把任务
  停进 `todo`（`dependency_wait` 事件），由父任务门控 +
  `recompute_ready` 在父任务完成后自动晋升 —— 无需人工与定时
  解锁；`needs_input` / `capability` / `transient` / 未类型化进入
  `blocked` 并持久化 `block_kind` 与 `block_recurrences`，解锁循环
  熔断器在同一原因于解锁后再次阻塞达到
  `BLOCK_RECURRENCE_LIMIT`（2）次时把任务改路由到 `triage`
  （`block_loop_detected` 事件）—— 复发计数刻意跨越 unblock 保留、
  仅在任务完成时清零；`unblock_task` 现在按未完成父任务重新门控
  （父任务未了则 blocked → `todo`），与 hermes 的不变式修复一致；
  agent `kanban_block` 工具与网关 block API 同步接受 kind；P143 补齐生命周期 CLI 面：批量 `kanban
  done/block/schedule/unblock/promote/archive`（多个 id，hermes
  `task_ids` + `--ids`），`kanban done --summary/--metadata` 把结构化
  交接（完整 summary + JSON 事实）写入收尾 run，`completed` 事件则
  携带 summary 首行（400 字符上限）供通知器渲染，`kanban archive
  --rm` 彻底清除已归档任务及其全部关联行（护栏：仅 archived 可
  删除），`kanban unblock --reason` 先记评论再解锁，`kanban promote
  --dry-run/--json` 由无副作用的 `validate_promote` 支撑，`kanban
  watch --tenant`；archive 现在同时把进行中的 run 以 reclaimed 收
  尾，并立即晋升那些仅被已归档父任务挡住的子任务
  （`recompute_ready` 按 hermes 语义把 archived 父任务视同完成）。
  P144 补齐完成态恢复：`kanban edit --result/--summary/--metadata`
  可改写已 done 任务的交接（result 文本 + 最近一次 completed run 的
  summary/metadata，缺少 run 行时自动合成；发出 `edited` 事件），
  terminal kanban 工具新增 `summary` + `metadata` 参数供 worker 交付
  结构化事实，block 操作先写 `BLOCKED: <reason>` 评论再落锁
  （hermes `_cmd_block` 对齐）。P145 把 `recompute_ready` 扩展到
  blocked 列：父任务全部 done/archived 的阻塞任务自动恢复为 ready
  （保留 `consecutive_failures`，发出 `promoted` 事件），除非阻塞是
  粘性的 —— 最近一次 blocked/unblocked 事件是 worker/运维主动发起的
  `blocked`（#28712）—— 或失败计数已达到有效上限（任务 `max_retries`
  > 调度器 `failure_limit` > 默认 2，#35072）；调度器经
  `dispatch_once` 透传自身配置的上限。P146 补上不可 spawn 门控与健康
  探针：`dispatch_once` 接受配置的 profile 集合，把 assignee 不在集合
  内的就绪任务归入 `skipped_nonspawnable`（只能经 claim 拉取的控制面
  通道，绝不自动 spawn —— hermes #kanban-dispatcher-crash-loop）；
  网关调度器的 stuck 警告改由 `has_spawnable_ready` 判定，就绪队列里
  只有通道任务时视为"正常空闲"，仅在确有可 spawn 工作（未指派或已配
  置 profile 的任务）等待时才告警。P147 移植完成工件：`kanban done
  --artifact <path>`（可重复）、agent `kanban_done` 工具（`artifacts`
  数组）与网关 complete API 会把托管 scratch 工作区内的文件先行暂存
  到 `<home>/kanban/attachments/<task>/`（25 MiB 上限，缺失/超限的声
  明直接让完成失败并回滚），登记为 `artifact` 附件并发出 `attached`
  事件，同时合并 summary/result 文本中提到的绝对交付物路径，最终路
  径随 `completed` 事件与 run metadata 下发（hermes
  `kanban_complete(artifacts=[...])`、
  `_persist_scratch_completion_artifacts`、
  `_merge_completion_prose_artifacts`）。P148 移植 review 列：
  `review` 加入状态集（🔍）；worker 开 PR 后调用 `kanban review <id>
  [--reason]` / `kanban_review` 工具（running → review，收尾 worker
  run，发出 `review_requested` 事件）；`dispatch_once` 新增共享
  max_spawn 上限的 review 循环 —— 未指派的 review 任务归入
  `skipped_unassigned`，未知 assignee 归入 `skipped_nonspawnable`，
  认领时开新 run 且不再复查父任务依赖（`claim_review_task`），
  `<home>/skills/` 装有 `sdlc-review` 技能时强制加载；
  `has_spawnable_review` 并入网关健康探针。P149 增加按 profile 的
  并发上限：`[kanban] max_in_progress_per_profile`（hermes #21582）
  即使全局仍有余量，也拒绝为已达在飞上限的 assignee 再生 worker ——
  计数每 tick 从 running 列播种、dry-run 的拟 spawn 同样计数；被跳过
  的任务归入 `skipped_per_profile_capped`（CLI 行 + dispatch JSON）。
  P150 移植反幻觉完成门：`kanban done --created-card <id>`（可重复；
  agent `created_cards` 数组与网关 complete API 同步）逐一核验声明的
  卡片 —— 必须存在，且由该 worker 的 profile 创建、以 worker 任务 id
  作为 created_by、或已挂为 worker 任务的子任务。幻影 id 发出
  `completion_blocked_hallucination` 事件并在零改动下阻断完成
  （hermes `HallucinatedCardsError`）；核验通过的 id 随 `completed`
  事件下发，summary/result 文本中无法解析的 `t_<hex>` 引用在成功完成
  后以 `suspected_hallucinated_references` 事件提示（仅告警，hermes
  `_scan_prose_for_phantom_ids`）。P151 增加按 tick 的调度锁
  （#35240）：每次 `dispatch_once` 都在 `<kanban.db>.dispatch.lock`
  的非阻塞 `flock` 下进行；失败的调度器（例如逃出服务重启的孤儿进
  程）返回 `skipped_locked = true` 且零数据库写入，下一间隔再试 ——
  CLI（`dispatch: skipped …`）与 dispatch API JSON 均可见。P152 增
  加 worker 日志轮转：`kanban/worker-logs/` 下的按任务日志在达到
  `[kanban] worker_log_rotate_bytes`（默认 2 MiB）时轮转，保留一份
  `.log.1` 备份代，同一代内追加写入 —— 重生 attempt 不再截断先前输
  出（hermes `worker_log_rotation_config`）。P153 封死过期 worker
  竞态：调度改为先认领再 spawn（hermes 顺序），spawn 时 run 行已存
  在；worker 携带 `ULNCLAW_KANBAN_RUN_ID`（hermes
  `HERMES_KANBAN_RUN_ID`），其完成/阻塞以 `expected_run_id` 提交 ——
  原子的 `current_run_id` 守卫拒绝已被回收的 attempt，绝不覆盖新
  attempt（CLI Done/Block、`kanban_complete`/`kanban_block` 工具与
  网关 complete/block API 全链路透传）。已认领 attempt 的 spawn/工作
  区失败现在收尾 run、释放认领回 ready 并计入失败（hermes
  `_record_spawn_failure`）。P154 增加 `[kanban] max_in_progress`
  全局并发上限（#33488）：当板上运行中任务已达上限时，本轮调度直接提
  前返回（积压任务保持 ready，不落任何跳过桶）；否则有效 spawn 上限
  收紧为 `max_spawn` 与 `max_in_progress` 的较小者，使运行列恰好填满
  上限 —— 慢 worker（本地模型、资源受限主机）先消化存量，避免堆积任
  务超时。P154 同时移植了一次性 scratch 工作区提示（hermes
  `_maybe_emit_scratch_tip`）：整个安装内首次物化 scratch 工作区时，
  调度器警告 scratch 产物是临时的（任务完成即删除），在任务上记录
  `tip_scratch_workspace` 事件，并写入 `.scratch_tip_shown` 哨兵文件
  使提示不再重复；worktree/dir 工作区按设计保留，从不提示。P156 增加看板 goal 模式
  worker（hermes `create --goal` / `--goal-max-turns`）：goal 卡片 spawn 的
  worker 在同一会话内把运行包进 Ralph 式评判循环 —— 每轮结束后由辅助评判
  模型（`[auxiliary.goal_judge]`）对照卡片标题+正文评估最新回复；`continue`
  注入续跑提示，`done` 先发一次明确的 kanban_complete 提醒、再提醒无效则以
  "评判已完成但从未收尾" 阻塞卡片，turn 预算耗尽（或任务被回收/归档）以粘性
  block 留待人工审查。goal 卡片的完成在 CLI `kanban done` 与 `kanban_complete`
  工具两条路径上过 #38367 评判门：评判可用时结论必须为 `done`，否则带着评判理
  由拒绝完成（未配置/不可达评判时 fail-open；网关 `/api/kanban` complete 端点
  有意不门控）。P157 增加 `create --initial-status running|blocked`
  （hermes `VALID_INITIAL_STATUSES`）：`blocked` 直接把卡片停入 blocked 等
  待人工运维审查（优先于 `--triage`），`running` 保持默认流程（CLI 与网关
  create API 同步支持）。P158 增加工作流模板钩子（hermes
  `workflow_template_id` / `current_step_key` 任务列）：外部工作流引擎在建卡
  时打标（网关 create API 携带两字段），并经 `kanban list
  --workflow-template-id` 查回（SQL 级过滤；网关 list API 同名查询参数）。
  模板引擎本体在两个项目中都位于看板之外。P159 把按任务 model/provider
  覆盖接到 worker（hermes `model_override` / `provider_override`）：新增
  `provider` 任务列，`kanban create --provider` 与网关 create 请求体，
  `kanban set-model [--provider P]`（清 model 时一并清 provider；只给
  provider 不给 model 按 hermes 契约拒绝），全局 `-m/--model` 与
  `--provider` CLI 标志（优先于配置与 profile，spawn 出的 `ulnclaw run`
  worker 携带同名标志），`dispatch_spawn` 依卡片透传 `--model` /
  `--provider`。P160 移植了 hermes 的一等项目登记簿（`projects_db` +
  `project` CLI）：按 profile 隔离的 `projects.db` 具名多文件夹工作区
  （`ulnclaw project create/list/show/add-folder/remove-folder/rename/
  set-primary/use/archive/restore/bind-board` —— slug 唯一化、主目录重指、
  活跃项目指针；`bind-board` 同时把主仓库镜像为绑定 board 的
  `default_workdir`），以及 `kanban create --project <id|slug>`（CLI 与
  网关 create 请求体）：建卡时解析项目，把 worktree 锚定到项目主仓库下
  （`<repo>/.worktrees/<task-id>`）并派生确定性分支
  `<slug>/<task-id>[-<title-slug>]`，存于新增 `tasks.project_id` 列；
  无法解析的链接静默丢弃（hermes 丢弃悬空引用语义）。P161 补上了 hermes
  从未提供的仓库发现扫描器（其 `discovered_repos` 缓存表存在但仅 Electron
  桌面端以 TypeScript 走盘扫描）：`ulnclaw project scan [--root PATH ...]
  [--max-depth N]` 查找 git 检出（`.git` 目录或 worktree 文件；隐藏与跳过
  清单目录剪枝、符号链接不跟随、支持嵌套检出），以 replace 语义 +
  `cli-scan:v1` 策略键写入缓存；`project repos [--clear]` 列出/清空缓存。P162 把登记簿暴露给网关
  服务桌面界面：`/api/projects` 增删查改（`PATCH` board 绑定与 CLI
  `bind-board` 相同地把主仓库镜像为 board 的 `default_workdir`；文件夹
  增删、主目录设置、归档/恢复、硬删除、活跃指针）以及
  `/api/projects/scan|repos` 发现端点。P164 把会话接入项目：
  `/api/sessions` 行（list 与 get）携带 `project` slug（按会话 cwd 对
  `projects.db` 文件夹做最长前缀匹配，归档项目除外；projects.db 缺失时
  降级为 `project: null`），桌面侧栏以徽章渲染——对齐 hermes 桌面按
  项目分组会话的契约。
  看板其余有意保留的
  差异：调度期 `default_assignee` 应用（ulnclaw 在默认 profile 上 spawn
  未指派任务而不是跳过）。
- 消息平台：图片附件以原生多模态内容注入用户轮（P226，对齐 hermes
  媒体注入）：≤ 8 MB 的 `image/*` 文件 base64 编码为 `data:` URL，随本轮
  请求发送 —— OpenAI 兼容端以 `image_url` 部件、Anthropic 以 base64 图像
  块呈现；其余媒介（以及 `[messaging] multimodal_injection = false` 时的
  图片）仍为缓存路径引用，agent 用 vision_analyze/video_analyze/read_file
  检视。注入的图片仅存活于当前轮（会话历史保留文本轮）。
  语音消息经 `[stt]` 管道转写，但内置 `local` faster-whisper provider
  在静态二进制中需以 `stt.local.command` 或云 provider 替代；Telegram clarify + 执行审批行内键盘已移植（P225）；
  hermes 全部二十二个平台适配器均已移植（微信/QQ/元宝以原生适配器加入；WhatsApp Cloud + MS-Graph 接入、通用 webhook 平台、Twilio SMS、Microsoft Teams、LINE、Google Chat、Raft 与 A2A 均经网关 webhook 路由）；配对与 hermes 相同按
  发送者 id 生效，但配置式白名单仍以聊天/频道 id 为粒度（认证门控 =
  白名单 OR 已批准配对）。
- OAuth/同步：流程与提供方无关（任意 RFC 8628 端点），不绑定 Nous 门户；
  同步只搬运技能包（无组织提案/审批工作流、无订阅门控）。

<a id="http-路由对标附录"></a>

## HTTP 路由对标附录（P339）

对 hermes `web_server.py`（v2026.8.3，112 条路由）与 ulnclaw 网关路由表
（v0.7.0-rc13 时点 217 条：P351 的 198 条核心路由 + 下表的 19 条插件
REST 命名空间路由）做逐路由审计：每条 hermes 路由要么已被覆盖（可能路径
不同），要么属设计性决议不迁移——不存在无声缺漏。

### 以 ulnclaw 路径覆盖

| hermes 路由 | ulnclaw 等价物 |
|---|---|
| `/api/files`、`/api/files/read` | `/api/fs/list`、`/api/fs/read-text`（另含 `write-text`、`read-data-url`） |
| `/api/files/download` | `/api/fs/download`（P335，`?token=` 查询鉴权） |
| `/api/files/mkdir` | `/api/fs/mkdir`（P335） |
| `/api/files/upload`、`/api/files/upload-stream`、`/api/chat/image-upload` | `/api/uploads`（内容寻址媒体缓存） |
| `/api/hermes/update/check` | `/api/update/check`（P324） |
| `/api/hermes/update` | `ulnclaw update` CLI（终端进度显示；网关从不自更新） |
| `/api/ops/doctor` | `/api/doctor` |
| `/api/ops/backup` | `/api/backups`（另含 `/api/backups/:id/restore`、`/api/backups/prune`） |
| `/api/portal` | `/api/oauth/status`（P334，精简只读状态） |
| `/api/providers/validate` | `/api/providers/custom-endpoints/validate`（P333） |
| `/api/status`、`/api/system/stats` | `/api/health`、`/api/system`（P334） |
| `/api/analytics/usage` | `/api/usage` + `/api/analytics/models` + `/api/insights` |
| `/api/learning/node` | `/api/learning/node`（GET/PUT/DELETE） |
| `/api/sessions/import` | 同路径（P348——经 `GET /api/sessions/:id/export?format=json` 的可移植 JSON 往返） |
| `/api/providers/oauth*` | 同路径（P350——精简设备码目录：基于 `[oauth]` 流程的 list/start/poll/submit/cancel/disconnect） |
| `/api/dashboard/plugins*`、`/api/dashboard/agent-plugins*`、`/api/dashboard/plugin-providers` | 同路径（P351——精简插件市场：活动清单 + 重新扫描、基于精选 `plugin-hub/index.json` + 本地目录候选的合并市场载荷（安装/更新/移除/启用/禁用）、持久化到 `[plugins]` 的记忆/上下文 provider 选择、`dashboard.hidden_plugins` 可见性开关） |
| `/api/plugins/kanban/*`（hermes 将 kanban 插件的 FastAPI 路由挂载进 web server） | 同路径（v0.7.0-rc13——桌面 kanban 插件 REST 命名空间由 `KanbanStore` 原生承载：`board` 按 hermes 列序输 … [详情](#api-plugins-kanban-hermes-将-kanban-插件的-fastapi-路由挂载进-web-server) |
| `/api/media` | `/api/media`（P338） |
| `/api/audio/speak`、`/api/audio/speak-stream`、`/api/audio/elevenlabs/voices` | 同路径（P344——精简 openai/elevenlabs `[tts]` provider；P349 新增免费 `edge` provider；`speak-stream` 为流式 TTS WebSocket——文本进、24 kHz int16 PCM 帧出，走 openai/elevenlabs 分块 API，带断句、空闲冲刷与打断（barge-in）；无分块 API 的 provider 回单帧 `fallback`，客户端降级到 `/api/audio/speak`） |
| `/api/messaging/platforms` | `/api/messaging/platforms`（另含 `PUT :id`、`POST :id/test`；P337） |
| `/api/webhooks`、`/api/webhooks/{name}`、`/api/webhooks/enable`、`/api/webhooks/{name}/enabled` | 配置驱动：webhook 平台在 `[messaging.*]` 配置中声明；入站走开放的 `/webhooks/*` 路由 |
| `/api/model/auxiliary` | 配置驱动：config.toml 的 `[auxiliary.<task>]` 覆盖（无仪表盘面） |

#### 详细说明

<a id="api-plugins-kanban-hermes-将-kanban-插件的-fastapi-路由挂载进-web-server"></a>

##### `/api/plugins/kanban/*`（hermes 将 kanban 插件的 FastAPI 路由挂载进 web server）

同路径（v0.7.0-rc13——桌面 kanban 插件 REST 命名空间由 `KanbanStore` 原生承载：`board` 按 hermes 列序输出 + 卡片聚合字段、boards
增删改、任务详情（comments/events/attachments/links/runs/diagnostics）、PATCH
状态语义（`done`→complete、`blocked`→block、`scheduled`→schedule、`ready`→unblock、`archived`→archive、`running`→400）、批量操作、任务日志
tail、multipart 附件、reassign/reclaim、辅助模型估算（20 秒超时保护，契约
`{ok,est_tokens,complexity,rationale,model}`，绝不抛 HTTP 错误）、profiles 名册（config assignees ∪ 已存描述）+
PATCH、projects、orchestration GET/PUT（读写 `[kanban.*]` 配置）、dispatch 委派、assignees。hermes 挂载 Python
插件路由，ulnclaw 由引擎直接提供同一契约。achievements 插件不迁移——vendored 桌面只内置 kanban 插件）


### 设计性决议（不迁移）

- **网关生命周期**（`/api/gateway/start|stop|restart|drain`）——桌面外壳
  监管 gateway 子进程（`ULNCLAW_DESKTOP=1`），CLI 或服务管理器监管独立
  gateway；让网关按仪表盘请求自我重启正是单实例守卫（P155）要防的故障
  模式。接管走 `ulnclaw gateway --replace`。
- **仪表盘动作**（`/api/actions/{name}/status`）——hermes 用它轮询仪表盘
  驱动的自更新/重启动作；ulnclaw 的更新在 CLI 侧带终端进度执行，不存在
  需要轮询的长时仪表盘动作。
- **记忆 provider**（`/api/memory/provider`、
  `/api/memory/providers/{name}/config|setup`）——ulnclaw 的记忆是文件式
  MEMORY.md/USER.md 双件 + `memory` 工具，`/api/memory` 提供清单与定向
  重置；可插拔第三方记忆后端不在移植范围。
- **引导向导**（`/api/messaging/telegram/onboarding/*`、
  `/api/messaging/whatsapp/onboarding/*`）——配置向导在 CLI 侧
  （`ulnclaw setup`、`ulnclaw whatsapp-cloud`；P267/P271）；WhatsApp 走
  自管 Baileys 桥，其配对是桥自身启动流程，`ulnclaw whatsapp status`
  负责检视（P272）。
- **插件静态资源**（`/dashboard-plugins/{name}/{path}`）——不迁移面向
  第三方仪表盘扩展的逐插件 JS/CSS 托管；ulnclaw 桌面插件视图是原生的，
  市场插件贡献钩子/工具而非浏览器资源。市场/安装管线本身已在 hermes
  路径覆盖（P351）。
- **Provider OAuth**（`/api/providers/oauth*`）——已在同路径覆盖
  （P350）：基于服务无关 `[oauth]` 设备流程的精简设备码目录，含
  start/poll/submit/cancel/disconnect 会话。厂商专属市场式握手
  （Anthropic 重定向、Codex、Nous 门户）不在移植范围。
- **模型集成**（`/api/model/moa`）——不迁移 Mixture-of-Agents 集成路由；
  单网关模型 + 会话级模型锁定的设计已取代它。
- **TTS provider**——线协议面已覆盖（见上表；P344），edge/openai/
  elevenlabs 均已移植；P349 实现免费 `edge` provider（微软 read-aloud
  websocket 协议 + Sec-MS-GEC 7.x DRM 令牌交换——无需 API key）。本地
  模型（piper/neutts/kittentts）仍不在移植范围。
- **运维杂项**（`/api/ops/checkpoints*`、`/api/ops/config-migrate`、
  `/api/ops/debug-share`、`/api/ops/import*`、
  `/api/ops/backup/download`）——检查点由 `/api/backups` + 会话分叉取代；
  配置迁移是 hermes 一次性升级路径；调试包留在本地（`ulnclaw doctor`）；
  会话导入已有专用端点（P348）；备份下载由恢复端点取代。
- **策展器**（`/api/curator/run`、`/api/curator/paused`）——学习策展为
  手工操作：经 `/api/learning/node` 的节点编辑/归档与桌面 Learning 视图。

<a id="完成状态"></a>

## 完成状态

agent 核心已与 hermes-agent v2026.8.3 对齐 —— 以下各面均已移植，至此
v2026.8.3 对标面全部完成：

- 全部核心工具；
- 完整 `sessions` 面（`list/show/search/export/recap/recover/prune/archive/stats/delete/rename/optimize/repair/browse`）；
- 启动恢复（`--resume`/`--continue`，一会话一记录的连续性）；
- CDP 浏览器客户端 + 接入层 + Camofox 后端 + 云浏览器 provider（Browserbase / Browser Use / Firecrawl，P264）；
- HTTP 网关（含 `/v1/browser/*` 实时端点控制与 profile 多路复用）；
- 技能/捆绑/记忆/目标/检查点/定时任务/用量分析/doctor；
- 外部秘密源（command 助手 / Bitwarden / 1Password）；
- computer-use（cua-driver）；
- 子进程插件系统；
- 消息平台网关（Telegram/Discord/Slack/Signal/微信/QQ/元宝/邮件/Mattermost/Matrix/钉钉/企微/飞书/HomeAssistant/SMS/WhatsApp/IRC/ntfy/SimpleX/Teams/LINE/Google Chat/Buzz/Photon/Raft/A2A）；
- OAuth 设备流登录 + 技能同步；
- `send_message` 跨频道工具 + 频道目录（P259）；
- MCP 频道桥 `ulnclaw mcp serve`（P260）；
- ACP 编辑器适配器 `ulnclaw acp`（P261）；
- 并行批量运行器 `ulnclaw batch`（P262）；
- 脚本化消息 CLI `ulnclaw send`（P263）；
- Slack 原生斜杠命令 + `ulnclaw slack manifest` 应用清单生成器（P265）；
- 网关监控 + OTLP 健康/诊断导出（`[monitoring]`，P266）；
- 交互式安装向导 `ulnclaw setup`（P267）；
- 交互式模型切换器 `ulnclaw model`（P268）；
- 桌面启动器 `ulnclaw gui` + `login`/`logout`/`learning`/`memory-graph` 别名（P269）；
- 动态 webhook 订阅 `ulnclaw webhook`（P270）；
- WhatsApp Cloud 安装向导 `ulnclaw whatsapp-cloud`（P271）；
- WhatsApp 桥诊断 `ulnclaw whatsapp status`（P272）；
- 桌面 GUI（`desktop-electron/`，hermes Electron 桌面端的忠实移植）及其余 CLI。

`sessions` 面仅有意省略 `optimize-storage`（ulnclaw 自始即采用紧凑的
外联内容 FTS 布局，无旧布局可迁移）；`-c <会话名>` 标题查找已于 P232 移植。

有意不移植（超出本地 agent 范围的 hermes 面）：

- Electron 桌面应用本身 —— v0.7.0 起撤销：ulnclaw 现直接交付（`desktop-electron/` 为 hermes desktop v2026.8.3 的忠实移植，后台对接 ulnclaw 网关；早前的 Tauri 外壳、其精简对位界面清单与桌面装饰/Electron 专有面豁免一并退役——忠实移植悉数包含）；
- Python 插件导入/entry-point 包及其携带的 provider 注册（ulnclaw 插件体系改用 shell 钩子线协议 + 目录插件，并含 git install/update/remove 生命周期）；
- Nous 门户专属的订阅门控与组织提案审批流程（OAuth/同步为 provider 无关）；
- xAI 凭证池代理适配器（凭证池存储已精简移植——见凭证池行——但 xAI 适配器本身仍未移植）；
- cua-driver 内嵌守护/套接字模式，以及上下文级截图驱逐（P228 已移植视觉后处理一半——对返回截图按 `max_image_dimension` 做客户端最长边强制限制；驱逐一半在 v2026.8.3 检出中无参考实现）。

另有若干 CLI 面刻意不移植（已记录为差异，按 v2026.8.3 审计）：

- `hermes console` —— “安全”命令控制台与 ulnclaw 聊天 REPL 的斜杠命令面重叠（`/help` `/tools` `/skills` `/sessions` ……已可在免 LLM 回合下应答）；
- `hermes dashboard` / `hermes serve` —— 独立 Web UI 服务器由 `desktop-electron/` 应用 + 网关 OpenAI 兼容 API 面取代（网关保留 hermes dashboard 的本地应用 CORS 模型）；
- `hermes honcho` —— Honcho AI 托管记忆集成（第三方 SaaS）无 ulnclaw 对应物；本地记忆 + 已学技能 + 记忆图谱覆盖进程内范围；
- `hermes migrate` —— 配置格式迁移不适用（ulnclaw 无遗留格式可迁移）；
- `hermes claw` —— OpenClaw 迁移导入器不适用（ulnclaw 无 OpenClaw 谱系）；
- `hermes profile` —— 配置管理架构不同：ulnclaw 使用 `[profiles.<name>]` 配置覆盖 + 网关多路复用（`/p/<profile>/...` 镜像），而非每 profile 独立主目录的 CLI；
- `hermes whatsapp` —— Baileys 桥向导由网关监督的内置桥取代：`npm` 依赖在网关首次启动时安装，QR 配对经桥自身启动流程完成，`[messaging.whatsapp]` 开关平台（无需单独向导进程保持同步）；其检查职责由 `ulnclaw whatsapp status`（P272）覆盖。

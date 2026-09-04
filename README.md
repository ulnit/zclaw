# ulnclaw 🦞

<p align="center">
  <img src="https://img.shields.io/badge/License-MIT%20OR%20Apache--2.0-green?style=for-the-badge" alt="License: MIT OR Apache-2.0">
  <img src="https://img.shields.io/badge/Language-Rust-DEA584?style=for-the-badge&logo=rust&logoColor=white" alt="Rust">
  <img src="https://img.shields.io/badge/Parity-hermes--agent%20v2026.8.3-blueviolet?style=for-the-badge" alt="hermes-agent v2026.8.3 parity">
  <a href="README.zh-CN.md"><img src="https://img.shields.io/badge/Lang-中文-red?style=for-the-badge" alt="中文"></a>
</p>

**A high-performance AI agent engine written in Rust — a port of [hermes-agent v2026.8.3](https://github.com/NousResearch/hermes-agent/tree/v2026.8.3) by [Nous Research](https://nousresearch.com).**

ulnclaw re-implements the Hermes Agent engine in Rust: the same tool surface
(50+ built-in tools), the same SQLite session / memory / skills / cron storage
layout, the same toolset composition — with native performance and a single
static musl binary. Use any model you want (OpenAI-compatible endpoints,
Ollama, Anthropic, DashScope, …) and switch with `ulnclaw model` — no code
changes. Run it on your laptop, a $5 VPS, or in Docker sandboxes — and talk to
it from the terminal, the desktop app, or 26 messaging platforms.

Core parity with hermes-agent v2026.8.3 is complete — see the
[parity matrix](docs/en/hermes-parity.md) for the full feature-by-feature
mapping, including the HTTP route parity appendix.

## Highlights

<table>
<tr><td><b>🔧 50+ built-in tools</b></td><td>Terminal/process, file read/write/patch/search, web search/extract, memory, todo, delegation, <code>execute_code</code>, vision, image/video generation, browser automation, TTS, kanban, tool search — grouped into hermes-compatible toolsets (<code>coding</code>, <code>web</code>, <code>file</code>, <code>safe</code>, <code>debugging</code>, …) with enable/disable policy.</td></tr>
<tr><td><b>💬 A real terminal interface</b></td><td>Interactive chat with slash commands (<code>/new /search /memory /skills /sessions /rollback /diff /recap /goal /kanban …</code>), streaming tool output, session resume (<code>--continue</code> / <code>--resume</code>).</td></tr>
<tr><td><b>📡 Lives where you do</b></td><td>26 messaging platforms from a single gateway process — Telegram, Discord, Slack, Signal, WeChat, QQ, Yuanbao, Email, Mattermost, Matrix, DingTalk, WeCom, Feishu, Home Assistant, SMS, WhatsApp, IRC, ntfy, SimpleX, Teams, LINE, Google Chat, Buzz, Photon (iMessage), Raft, A2A — with cross-channel <code>send_message</code> and voice-note transcription.</td></tr>
<tr><td><b>🧠 Memory &amp; skills</b></td><td>Persistent memory with prompt injection, FTS5 full-text session search, skills with security scanning, skill sync, learning timeline (<code>ulnclaw journey</code>).</td></tr>
<tr><td><b>⏰ Scheduled automations</b></td><td>Built-in cron scheduler with delivery to any platform, schedulable skill blueprints, suggested automations.</td></tr>
<tr><td><b>🤝 Delegates &amp; parallelizes</b></td><td>Background subagent delegation with a persistent async registry, kanban task engine with swarm mode, <code>execute_code</code> pipelines, Mixture-of-Agents fan-out (<code>moa</code>).</td></tr>
<tr><td><b>🌐 Browser &amp; computer use</b></td><td>12 <code>browser_*</code> tools over CDP — managed headless Chrome/Chromium, Camofox anti-detect, cloud sessions (Browserbase / Browser Use / Firecrawl) — plus <code>computer_use</code> via the cua-driver daemon, approval-gated like hermes.</td></tr>
<tr><td><b>🔌 Extensible</b></td><td>MCP client (stdio + Streamable HTTP/SSE, OAuth 2.1 + PKCE, lazy servers), plugins + shell hooks firing all 13 hermes hook events, ACP adapter, OpenAI-compatible HTTP gateway, MCP channel bridge.</td></tr>
<tr><td><b>🛡️ Secure by default</b></td><td>Approval system with fail-closed gateway approvals, secrets vaults (Bitwarden SM / 1Password / command), SSRF guards, secret redaction, sandbox credential scrub, egress firewall for Docker sandboxes, transparent checkpoints.</td></tr>
<tr><td><b>🗜️ Context management</b></td><td>Budget-triggered middle-turn compression, three-layer tool-result persistence, <code>/context</code> window breakdown.</td></tr>
<tr><td><b>🖥️ Desktop GUI</b></td><td><b>ulnclaw desktop</b> — a faithful port of the hermes Electron desktop: 16 views, command palette, xterm.js terminal panes, live turn streaming over WebSocket/SSE, silent whole-shell auto-update.</td></tr>
<tr><td><b>📦 Runs anywhere</b></td><td>Single static musl binary; terminal backends: local, Docker, SSH.</td></tr>
</table>

## Quick Start

### Build from source

```bash
cargo build --release --target x86_64-unknown-linux-musl   # static binary
# binary: target/x86_64-unknown-linux-musl/release/ulnclaw
```

### First run

```bash
ulnclaw setup        # interactive onboarding wizard (provider, terminal, platforms, tools)
ulnclaw model        # switch provider/model interactively
ulnclaw init         # write a default config to ~/.ulnclaw/config.toml

ulnclaw run "Summarize the README.md file"   # one-shot run
ulnclaw chat                                 # interactive chat
ulnclaw chat --continue                      # continue the most recent session
ulnclaw chat --resume <session-id>           # resume a session by id or unique prefix

ulnclaw gui          # launch the desktop app (alias: desktop)
```

## CLI Quick Reference

Every command ships `--help`; `ulnclaw completion bash|zsh|fish|elvish|powershell`
generates shell completions.

```bash
# Sessions & history
ulnclaw sessions list|search|browse        # browse = interactive picker + transcript viewer
ulnclaw sessions export <id> --format md   # also: import / delete / rename / optimize
ulnclaw sessions recover|repair            # offline recovery for damaged state.db
ulnclaw insights                           # usage analytics over sessions

# Skills & automations
ulnclaw skills list|blueprints|scan        # scan = security check before trusting a skill
ulnclaw cron list                          # create/show/pause/resume/run + blueprints
ulnclaw suggestions                        # suggested automations (accept/dismiss)
ulnclaw journey                            # learning timeline

# Tools & models
ulnclaw tools                              # list toolsets + enabled tools (enable/disable X)
ulnclaw models providers                   # models.dev catalog (list/info/refresh)
ulnclaw moa list|run                       # Mixture of Agents presets
ulnclaw fallback add|remove                # provider:model failover chain

# Gateway & platforms
ulnclaw gateway --host 127.0.0.1 --port 8642   # OpenAI-compatible API + platforms
ulnclaw dashboard status                   # dashboard server (run/stop)
ulnclaw pairing list                       # DM pairing codes (approve/revoke)
ulnclaw weixin login                       # WeChat QR-scan login
ulnclaw spotify-auth login                 # Spotify PKCE OAuth for the spotify_* tools

# Projects & kanban
ulnclaw project list|create|scan           # first-class project registry + git discovery
ulnclaw kanban list                        # task engine: create/claim/done/swarm/...

# Security & secrets
ulnclaw approvals                          # manual | smart | off
ulnclaw secrets status|sync                # external vaults (bitwarden/onepassword setup)
ulnclaw security audit                     # OSV.dev audit of pinned MCP packages
ulnclaw computer-use status                # desktop control via cua-driver (doctor/install)
ulnclaw plugins list                       # plugins + shell hooks (hooks doctor)

# Ops
ulnclaw doctor                             # diagnose config/deps (--fix, --online)
ulnclaw status                             # status of all components (--deep)
ulnclaw logs                               # tail/filter logs (-f, --level, --component)
ulnclaw update --check                     # stash -> ff pull -> rebuild
ulnclaw backup                             # zip backup of home (list/restore/prune)
ulnclaw config get|set|unset               # env-style keys go to .env
ulnclaw dump                               # copy-pasteable setup summary for support
```

## Desktop App

Prebuilt installers ship on the releases page for every `v*` tag (product name
**ulnclaw desktop**), built from the Electron shell in `desktop-electron/` — a
faithful port of the hermes desktop (v2026.8.3): React 19 renderer with sixteen
views (chat, sessions, jobs, usage, models, skills, kanban, projects, runs,
webhooks, plugins, pairing, profiles, config, doctor, settings), command
palette, xterm.js terminal panes, file tree with git review, and live turn
streaming over a JSON-RPC WebSocket plus HTTP/SSE.

- **Windows** — `ulnclaw-<ver>-win-x64.exe` (NSIS, per-user). Fully
  self-contained: the statically linked `ulnclaw` gateway binary rides inside
  the bundle; the shell spawns `ulnclaw gateway` on `127.0.0.1:8642` and
  probes `/health`. Boot diagnostics on the failure card.
- **macOS** — `ulnclaw-<ver>-mac-arm64.dmg` (Apple Silicon) /
  `ulnclaw-<ver>-mac-x64.dmg` (Intel). Ad-hoc signed: on first launch use
  right-click › Open, or run `xattr -cr "/Applications/ulnclaw desktop.app"`
  if Gatekeeper reports it as damaged.
- **Linux** — `ulnclaw-<ver>-linux-x86_64.AppImage` (deb/rpm also build).
  Every release ships a `SHA256SUMS.txt`.
- **Silent auto-update (v0.7.1+)** — electron-updater on the GitHub release
  channel replaces the entire shell (app + bundled gateway) in one background
  restart; logs under `[shell-update]` in `~/.ulnclaw/logs/desktop.log`.

First launch needs no API key: the gateway boots keyless and the onboarding /
Models view walk you through adding a provider key. Configuration lives in
`~/.ulnclaw/config.toml` (Windows: `%USERPROFILE%\.ulnclaw\config.toml`),
exactly as in the CLI flow below.

### Launching with `ulnclaw gui`

```bash
ulnclaw gui                        # alias: `ulnclaw desktop`; spawn the packaged app detached
ulnclaw gui --dev                  # run the unpackaged app from desktop-electron/ (`npm start`)
ulnclaw gui --binary PATH          # explicit executable
ULNCLAW_DESKTOP_BINARY=PATH ulnclaw gui
```

Binary resolution order: `--binary` → `ULNCLAW_DESKTOP_BINARY` →
`desktop-electron/release/{linux,win,mac}-unpacked/…` (produced by
`npm run pack`). If nothing is found, the command prints build instructions
(`cd desktop-electron && npm install && npm run dist`) — unlike `hermes gui`,
it does **not** build the app on demand. The desktop app shares the CLI's
config, keys, sessions, and skills; full shell details in
[desktop-electron/README.md](desktop-electron/README.md).

## HTTP Gateway

One process serves the OpenAI-compatible API, the messaging platforms, the
desktop app, and any browser dashboard:

```bash
ulnclaw gateway --host 127.0.0.1 --port 8642

curl -H "Authorization: Bearer $ULNCLAW_GATEWAY_KEY" \
     -H "Content-Type: application/json" \
     -d '{"messages":[{"role":"user","content":"Hello!"}]}' \
     http://127.0.0.1:8642/v1/chat/completions
```

- `/v1/chat/completions` + `/v1/responses` with SSE streaming; async
  `/v1/runs` with approval resolution; sessions API; 100+ `/api` management
  endpoints (local-app CORS built in).
- Messaging platforms run inside the gateway
  (`[messaging.telegram|discord|slack|signal|weixin|qq|yuanbao|email|...]`,
  plus webhook platforms: whatsapp_cloud/msgraph/webhook/bluebubbles/feishu/
  sms/teams/line/google_chat/raft/a2a).
- Optional profile multiplexing (`multiplex_profiles`) serves
  `/p/<profile>/...` mirrors with fail-closed secret scopes — see the
  [multiplexing gateway design note](docs/design/multiplexing-gateway.md).

## Configuration

`ulnclaw init` writes a default `~/.ulnclaw/config.toml`:

```toml
# timezone = "Asia/Shanghai"        # IANA zone for prompt timestamps

[model]
provider = "ollama"                 # or "openai", "anthropic", "dashscope", ...
model = "qwen3:32b"
base_url = "http://localhost:11434/v1"
# max_retries = 2                   # retry 429/5xx/network with backoff
# fallbacks = ["openai:gpt-5.2-mini", "ollama:qwen3:32b"]   # failover chain

# Auxiliary model routing — run secondary calls on a different model
# [auxiliary.compression]           # context-compression summaries
# provider = "openai"
# model = "gpt-5.2-mini"
# [auxiliary.vision]                # vision_analyze / browser_vision
# [auxiliary.title_generation]      # session titles after the first exchange

# MCP servers — stdio, remote HTTP/SSE, or OAuth-protected
[[mcp.servers]]
name = "filesystem"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/home/me"]
# lazy = false                      # true = register from schema cache, spawn on first call

# [[mcp.servers]]
# name = "remote"
# url = "https://mcp.example.com/mcp"   # transport = "sse" for legacy SSE
# auth = "oauth"                        # OAuth 2.1 + PKCE on first use, tokens cached

# [gateway]
# host = "127.0.0.1"
# port = 8642
# key = "sk-..."                    # env ULNCLAW_GATEWAY_KEY overrides

# [terminal]
# backend = "docker"                # "local" (default) | "docker" | "ssh"

# [approvals]
# mode = "manual"                   # manual | smart (aux-LLM guardian) | off

# [security]
# allow_private_urls = false        # web tools stay away from private/internal IPs

# [checkpoints]
# enabled = true                    # transparent snapshots before write_file/patch
```

Browser automation connects via `ULNCLAW_BROWSER_CDP` (an existing browser with
remote debugging), `CAMOFOX_URL` (anti-detect server), or
`[browser] cloud_provider` (browserbase / browser-use / firecrawl). See
[docs/en/tools.md](docs/en/tools.md) and
[docs/en/providers.md](docs/en/providers.md) for the full reference.

## Library Quick Start

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
    register_builtin_tools(&mut tools);          // all 50+ hermes-style tools

    let agent = Agent::new(Arc::new(provider), tools)
        .with_config(AgentConfig { approval: false, ..Default::default() })
        .with_store(Arc::new(SqliteSessionStore::open_default()?));

    println!("{}", agent.chat("List the files in this directory").await?);
    Ok(())
}
```

## Documentation

| Topic | Description |
|---|---|
| [Hermes Parity Matrix](docs/en/hermes-parity.md) | Tool/feature mapping vs hermes-agent v2026.8.3, incl. HTTP route parity |
| [Architecture](docs/en/architecture.md) | Project structure, agent loop, key modules |
| [Tools & Toolsets](docs/en/tools.md) | 50+ tools, toolset composition, terminal backends |
| [Providers](docs/en/providers.md) | Model providers, credentials, fallbacks |
| [Integration Guide](docs/en/integration.md) | Gateway, embedding, messaging platforms |
| [API Reference](docs/en/api-reference.md) | HTTP gateway endpoints |
| [Development Guide](docs/en/development.md) | Dev setup, testing |
| Design notes | [Multiplexing Gateway](docs/design/multiplexing-gateway.md) · [Browser CDP Client](docs/design/browser-cdp.md) · [Desktop Electron](docs/design/desktop-electron.md) |
| [Desktop App](desktop-electron/README.md) | The Electron shell (`ulnclaw desktop`) |

## Migrating from hermes-agent

ulnclaw targets feature parity with hermes-agent v2026.8.3 and reuses its
storage layout (`~/.ulnclaw/` mirrors `~/.hermes/`), so sessions, memory,
skills, and cron jobs follow the same SQLite schema. See the
[parity matrix](docs/en/hermes-parity.md) for the exact mapping, and
`ulnclaw import-agent` to import Claude Code / Codex setups.

## Building & Testing

```bash
cargo test                     # 990 tests
cargo build --release --target x86_64-unknown-linux-musl   # static binary
```

## Contributing

Contributions are welcome! Please run `cargo test` before submitting a pull
request, and keep the [parity matrix](docs/en/hermes-parity.md) in sync when
adding or changing hermes-equivalent behaviour.

## License

MIT OR Apache-2.0

ulnclaw is a Rust port of [hermes-agent](https://github.com/NousResearch/hermes-agent)
by [Nous Research](https://nousresearch.com). See that project for the
original design and documentation.

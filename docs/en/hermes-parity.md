# Hermes Agent Parity Matrix (v2026.8.3)

**Contents** — [Tool parity](#tool-parity) · [Feature parity](#feature-parity) · [Storage layout](#storage-layout) · [Known differences](#known-differences) · [HTTP route parity appendix](#http-route-parity-appendix) · [Completion status](#completion-status)

This document tracks how ulnclaw maps to
[hermes-agent v2026.8.3](https://github.com/NousResearch/hermes-agent/tree/v2026.8.3).
ulnclaw is a Rust re-implementation of the hermes agent engine: same tool
surface, same storage layout, same configuration semantics — native
performance and a single static binary.

<a id="tool-parity"></a>

## Tool parity

| hermes tool | ulnclaw | Notes |
|---|---|---|
| `terminal`, `process` | ✅ full | foreground/background, timeouts, workdir tracking, background session registry (list/log/wait/kill), failure intelligence (benign exit-code semantics + output-pattern recovery hints) |
| Tool-output limits (`tool_output_limits.py`) | ✅ | `[tool_output] max_bytes/max_lines/max_line_length` tune terminal output head+tail cap (default 100k chars), read_file pagination cap (2000 lines), and per-line clamp with `... [truncated]` marker (2000 chars); non-positive values coerce to defaults; behaviour-preserving when unset |
| Terminal failure hints (`terminal_hints.py`, `_interpret_exit_code`) | ✅ | `exit_code_meaning` for benign non-zero exits (grep/rg/diff/find/test/curl/git semantics, last pipeline/chain segment wins, `VAR=val` prefixes skipped); at most one `hint` per failed command from an ordered output-pattern scan (gh JSON-field drift, merge conflicts, command not found — python/pip special-cased, ModuleNotFoundError/ImportError, "already exists", gh rate limits, permission denied) plus exit-code-only hints 124/126/137; bounded 4000-char scan window, first match wins |
| Secret redaction (`agent/redact.py`) | ✅ core | terminal output (foreground + process log/wait) and read_file content pass through the redactor: ~55 vendor-prefix tokens (sk-/ghp_/glpat-/AKIA/xox…/JWT/private keys/DB … [details](#secret-redaction-agent-redact-py) |
| ANSI stripping (`ansi_strip.py`) | ✅ | full ECMA-48 coverage (CSI incl. private-mode/colon params/intermediates, OSC with BEL/ST terminators, DCS/SOS/PM/APC, nF and single-byte escapes, 8-bit C1) strips terminal + execute_code output before it reaches the model; `sanitize_display_text` additionally drops bare control chars and normalizes CR for safe terminal re-rendering |
| Binary extension guard (`binary_extensions.py`) | ✅ | `read_file` rejects ~80 binary extensions by pure string check (no I/O), pointing at vision_analyze/terminal; `.pdf` stays readable (text-based) |
| `read_file`, `write_file`, `patch`, `search_files` | ✅ full | line-numbered reads with `next_offset` pagination, fuzzy replace (whitespace/indent tolerant), V4A multi-file patches, unified diffs, ripgrep-style search |
| `web_search`, `web_extract` | ✅ full | pluggable backends: Tavily / Brave / SearXNG / built-in DuckDuckGo; HTML→text extraction |
| URL safety / SSRF guard (`tools/url_safety.py`) | ✅ core | `url_safety` module: blocks web fetches to private/internal addresses (loopback, RFC1918, link-local, CGNAT 100.64/10, benchmark 198.18/15, ULA, IPv4-mapped IPv6); cloud metadata endpoints (169.254.169.254, metadata.google.internal, ECS task metadata…) are **always** blocked; wired into `web_extract` (per-URL check + redirect re-validation via reqwest policy + credential-bearing URL refusal: token-prefix and sensitive-query-param blocks), opt-out `[security] allow_private_urls` / `ULNCLAW_ALLOW_PRIVATE_URLS`; fail-closed on DNS errors with a proxy carve-out (hermes semantics) |
| `memory` | ✅ full | `MEMORY.md` + `USER.md`, atomic batched `operations`, char limits (2200/1375), injected into every system prompt |
| `todo` | ✅ full | session task list, merge mode, single `in_progress` enforcement |
| `session_search` | ✅ full | SQLite FTS5 discovery + scroll shapes, session lineage |
| `clarify` | ✅ full | single/multi-select or open-ended via frontend callback |
| `skills_list`, `skill_view`, `skill_manage` | ✅ full | SKILL.md frontmatter, linked files (references/templates/scripts), path-traversal guard |
| Blueprints (`tools/blueprints.py`) | ✅ core | skills that declare `metadata.hermes.blueprint.schedule` in frontmatter become schedulable: `skills blueprints` (list), `skills schedule <name>` (creates the `blueprint:<skill>` … [details](#blueprints-tools-blueprints-py) |
| Skills guard (`tools/skills_guard.py`) | ✅ core | `skills scan <name> [--source <repo>] [--json] [--force]`: static scanner `skills-guard-v1` over SKILL.md + linked files — 119 threat patterns (exfiltration/destructive/persistence/supply-chain/prompt-injection), invisible-unicode detection, structural limits (50 files / 1 MB / 256 KB per file, symlink-escape + exec-bit checks), trust levels (builtin / agent-created / trusted repos incl. prefix aliases / community), verdict policy (critical→dangerous, high→caution; community+caution blocked, dangerous blocked even for trusted, `--force` only overrides caution for non-community) |
| `delegate_task` | ✅ full | parallel sub-agents, depth limit, isolated context, child sessions; hermes v2026.8.3 background semantics: top-level delegations dispatch fire-and-forget (`mode: background` … [details](#delegate-task) |
| `execute_code` | ✅ full | python3 subprocess sandbox, 120s cap |
| `cronjob` | ✅ full | create/list/update/pause/resume/remove/run; `30m` / `every 2h` / `0 9 * * *` / ISO one-shot schedules; SQLite job store; scheduler loop — the gateway auto-dispatches due jobs every 30s as tracked cron runs (cron approval scope, outcome recorded on the job row) and `ulnclaw cron run <id>` executes one immediately from the CLI; P219 delivery parity: `deliver` param (`origin` default when created inside a chat, else `local`; platform / `platform:chat[:thread]` / comma-mix / `all` routing tokens), origin capture from the live messaging context, resolved targets receive the final response (or a compact failure summary) through the platform senders with hermes wrap header + `[SILENT]` suppression |
| `tool_search` | ✅ full | keyword search over the registered tool catalog |
| `vision_analyze` | ✅ full | routes through the chat provider (`analyze_image`), `[auxiliary.vision]` provider/model override |
| `image_generate` | ✅ full | OpenAI images API, saves PNG under `<home>/images` |
| `text_to_speech` | ✅ full | OpenAI TTS or custom `ULNCLAW_TTS_ENDPOINT` |
| `ha_list_entities`, `ha_get_state`, `ha_list_services`, `ha_call_service` | ✅ full | Home Assistant REST API, gated on `HASS_URL` + `HASS_TOKEN` |
| `kanban_*` (12 tools) | ✅ full | local SQLite coordination board riding the SAME `KanbanStore` engine and `kanban.db` as the `ulnclaw kanban` CLI and the gateway `/api/kanban/*` endpoints (one board, three surfaces — hermes parity): create (with `parents`)/list/show/comment/heartbeat (auto claim todo→ready→running)/complete/block/unblock/link/attach/attach_url/attachments; unique-prefix id resolution, worker context via `ULNCLAW_KANBAN_TASK`/`HERMES_KANBAN_TASK` (workers default task_id to their own task; create/unblock/link are orchestrator-only, hermes gating), REPL `/kanban` board ops via `run_slash` |
| `browser_*` (12 tools) | ✅ full | CDP WebSocket client (`browser` module): endpoint discovery, page session, accessibility snapshots with element refs, click/type/scroll/press/screenshot/evaluate/dialogs … [details](#browser-12-tools) |
| `close_terminal`, `read_terminal`, `focus_pane`, `open_preview` | ✅ core | desktop GUI affordances (hermes `close_terminal_tool.py` / `read_terminal_tool.py` / `focus_pane_tool.py` / `open_preview_tool.py`): registered only under `ULNCLAW_DESKTOP=1` … [details](#close-terminal-read-terminal-focus-pane-open-preview) |
| `computer_use` | ✅ core | cua-driver MCP backend (`src/computer_use.rs`) — full hermes tool schema + approval semantics, see the Computer Use row below; registers once the driver is reachable (`ulnclaw computer-use doctor`) |
| `discord`, `discord_admin` | ✅ core | P245 full port of hermes `tools/discord_tool.py` (`src/discord_tool.rs`): 15 REST-API actions split into `discord` core (fetch_messages / search_members / create_thread) and … [details](#discord-discord-admin) |
| `feishu_doc_read`, `feishu_drive_*` | ✅ core | P246 full port of hermes `tools/feishu_doc_tool.py` + `tools/feishu_drive_tool.py` (`src/feishu_doc_tool.rs`): `feishu_doc_read` (toolset `feishu_doc`) reads a document's raw … [details](#feishu-doc-read-feishu-drive) |
| `spotify_*` (7 tools) | ✅ core | P247 full port of hermes `plugins/spotify` (`src/spotify_tool.rs` + `src/spotify_auth.rs`): `spotify_playback` (get_state/currently-playing/play/pause/next/previous/seek/set_repeat … [details](#spotify-7-tools) |
| `yb_*` (5 yuanbao tools) | ✅ core | P248 full port of hermes `tools/yuanbao_tools.py` (`src/yuanbao_tool.rs`): `yb_query_group_info` / `yb_query_group_members` (find/list_bots/list_all + mention hint, role labels … [details](#yb-5-yuanbao-tools) |
| `send_message` | ✅ core | P259 full port of hermes `tools/send_message_tool.py` + `gateway/channel_directory.py` (`src/send_message_tool.rs` + `src/channel_directory.rs`): actions … [details](#send-message) |
| `x_search` | 🟡 gated | full port of hermes `x_search_tool.py`: xAI Responses-API `x_search` server tool with handle allow/exclude filters (max 10, `@` stripped), strict client-side date-range validation (YYYY-MM-DD, no inverted/pure-future windows), `enable_image_understanding` / `enable_video_understanding`, retry-with-backoff on 5xx/transient failures, `degraded`/`degraded_reason` markers when filters yield no citations, `[x_search]` config (model / reasoning_effort / timeout_seconds / retries); registered only with `XAI_API_KEY` **and** the opt-in `x_search` toolset enabled (hermes parity — SuperGrok OAuth path not ported) |
| `video_analyze` | ✅ core | full port of hermes `vision_tools.video_analyze_tool`: local file / `file://` / HTTP(S) sources (remote downloads gated by the SSRF guard, cached under `cache/video/temp_video_files/` and cleaned up), extension→mime table (mp4/webm/mov/avi/mkv/mpeg/mpg), 20 MB warn + 50 MB base64 hard cap, inline `video_url` data-URL payload, `[auxiliary.vision]` routing with main-provider fallback, one retry on empty responses; opt-in `video` toolset (hermes parity) — requires a provider that accepts video |
| `video_generate`, `bfl_flux3_*` | ✅ core | `video_gen.rs` provider registry (hermes plugin design: single-available auto-select, configured-name fail-closed, `success_response`/`error_response` contract) + unified … [details](#video-generate-bfl-flux3) |
| `project_list`, `project_create`, `project_switch` | ✅ core | full port of hermes `tools/project_tools.py` + `hermes_cli/projects_db.py`: per-profile `projects.db` (projects / project_folders / project_meta / discovered_repos, WAL with … [details](#project-list-project-create-project-switch) |
| Skill usage telemetry + learning graph (`skill_usage`, `learning_graph`, `learning_mutations`) | ✅ core | hermes `tools/skill_usage.py` + `agent/learning_graph.py` + `agent/learning_mutations.py` ports: `<home>/skills/.usage.json` sidecar (view/use/patch counters, lifecycle state … [details](#skill-usage-telemetry-learning-graph-skill-usage-learning-graph-learning-mutations) |
| Learning timeline / `journey` CLI (`learning_graph_render`, `journey`) | ✅ core | hermes `agent/learning_graph_render.py` + `hermes_cli/journey.py` ports: `learning_graph_render.rs` — desktop-ported color math (palette derivation, complementary memory ink … [details](#learning-timeline-journey-cli-learning-graph-render-journey) |
| Skill curator CLI (`curator`) | ✅ core | hermes `hermes_cli/curator.py` local half (the LLM consolidation run stays desktop-side): `curator.rs` — idle-days computation (activity with created_at fallback), prune candidate … [details](#skill-curator-cli-curator) |
| Persistent goals / Ralph loop (`goals`) | ✅ core | hermes `hermes_cli/goals.py` port: `goals.rs` — `GoalContract` (outcome/verification/constraints/boundaries/stop_when, alias-table `parse_contract` so an incidental colon isn't … [details](#persistent-goals-ralph-loop-goals) |
| Gateway profile multiplexing (`/p/<profile>`) + CDP session liveness | ✅ core | hermes api_server profile-prefix middleware port: every gateway route is mirrored under `/p/<profile>/...`; `[gateway] multiplex_profiles = true` backs each mirror with its own … [details](#gateway-profile-multiplexing-p-profile-cdp-session-liveness) |
| Startup tips (`tips.py`) | ✅ core | `tips.rs`: feature-discovery one-liner corpus rewritten for ulnclaw's surface (slash commands, goals, CLI subcommands, config knobs, tools, gateway, hidden gems) + dependency-free xorshift64* `get_random_tip`; the chat REPL prints a `✦ Tip:` line at startup and on `/new` (hermes welcome/new-session tip parity) |
| REPL display & composer UX (`hermes_cli/focus_view.py`, `prompt_stash.py`, `clipboard.py`) | ✅ core | `src/focus_view.rs`, `src/prompt_stash.rs`, `src/clipboard.rs` — three hermes CLI-UX modules. **Focus view** (`/focus [on\\|off\\|status]`): display-only reduced-output mode — snaps … [details](#repl-display-composer-ux-hermes-cli-focus-view-py-prompt-stash-py-clipboard-py) |
| Sessions prune/archive/stats (`session_filters.py`) | ✅ core | `session/filters.rs` — duration parsing (`5h`/`30m`/`2d`/`1w`, bare number = days), point-in-time parsing (durations = that long ago; ISO timestamps naive=local), epoch … [details](#sessions-prune-archive-stats-session-filters-py) |
| Skin/theme engine (`skin_engine.py`) | ✅ core | `skin.rs`: all 9 hermes built-in skins as data (default, ares, mono, slate, daylight, warm-lightmode, poseidon, sisyphus, charizard — 258 color entries + branding + spinner faces), default-skin inheritance for partial palettes (`build_skin_config`), `list_skins`/`load_skin` (unknown → default), process-wide active skin (`init_skin_from_config` from `[display] skin`, `get/set_active_skin`), `get_color`/`get_branding` accessors, truecolor ANSI `colorize` (NO_COLOR aware); `ulnclaw skins` CLI lists themes with the active marker; REPL tips render in the active skin's `banner_dim`. Deferred: user YAML skins in `<home>/skins/` (no YAML dep), TUI status-bar/prompt-toolkit surfaces |
| Welcome banner & update check (`banner.py`) | ✅ core | `banner.rs`: skin-aware welcome panel rendered with box-drawing chars — braille claw-swipe hero + model line (shortened slug, `.gguf` strip, 28-char cap, models.dev context lookup … [details](#welcome-banner-update-check-banner-py) |
| Browser CDP attach layer (`browser_connect.py`) | ✅ core | `browser/connect.rs`: Chromium-family candidate discovery for macOS/Windows/Linux (incl. WSL `/mnt/c` install paths) covering Chrome/Chromium/Brave/Edge; dual-stack loopback CDP … [details](#browser-cdp-attach-layer-browser-connect-py) |
| Doctor (`doctor.py`) | ✅ core | `doctor.rs` + `ulnclaw doctor` CLI: hermes boxed-banner report with ✓/⚠/✗/ℹ checks in sections — Version & Updates (banner git state + 6h-cached upstream behind-count from P61) … [details](#doctor-doctor-py) |
| Session insights (`agent/insights.py`) | ✅ core | `insights.rs` + `ulnclaw insights [--days N] [--source S] [--json]` CLI + REPL `/insights [days]` + gateway chat `/insights [N] [--days N] [--source S]` slash command: … [details](#session-insights-agent-insights-py) |
| Pets (`agent/pet/` + `hermes_cli/pets.py`) | ✅ core | `src/pets.rs` + `ulnclaw pets list\|install\|select\|show\|off\|scale\|remove\|doctor\|hatch`: petdex mascot engine — public manifest fetch (petdex.dev, 300 s in-process cache + … [details](#pets-agent-pet-hermes-cli-pets-py) |
| Suggested automations (`cron/suggestions.py` + `suggestions_cmd.py`) | ✅ core | `cron/suggestions.rs`: JSON store at `<home>/cron/suggestions.json` (owner-only writes via tmp+rename) with hermes semantics — pending/accepted/dismissed statuses, dedup-key … [details](#suggested-automations-cron-suggestions-py-suggestions-cmd-py) |
| Status report (`hermes_cli/status.py` + `hermes_cli/subcommands/status.py` + `timefmt.py`) | ✅ core | `status.rs`: port of `show_status` — panel header + Environment (version / home / config.toml / .env), Model+Provider+Base URL, API Keys (config.toml `model.api_key` row + … [details](#status-report-hermes-cli-status-py-hermes-cli-subcommands-status-py-timefmt-py) |
| Log viewer + file logging (`hermes_cli/logs.py` + `hermes_cli/subcommands/logs.py` + `hermes_logging.py` rotating handlers) | ✅ core | `logs.rs`: viewer port — `LOG_FILES` registry (agent/errors/gateway), `_parse_since` (Ns/m/h/d cutoffs), timestamp/level/logger-name regexes (logger regex extended for Rust `::` … [details](#log-viewer-file-logging-hermes-cli-logs-py-hermes-cli-subcommands-logs-py-hermes-logging-py-rotating-handlers) |
| Self-updater (`hermes_cli/subcommands/update.py` + `update_cmd.py` git core) | ✅ core | `update.rs`: `--check` port of `_cmd_update_check` — branch resolution (`--branch` > current branch > master, hermes `_resolve_update_branch`), shallow-repo awareness (`--depth 1` … [details](#self-updater-hermes-cli-subcommands-update-py-update-cmd-py-git-core) |
| Backup & restore (`hermes_cli/backup.py` + `hermes_cli/subcommands/backup.py`) | ✅ core | `backup.rs`: full zip backup (hermes `run_backup` — exclusion sets `_EXCLUDED_DIRS/_SUFFIXES/_NAMES` adapted, self-exclusion of the output zip, progress/errors summary … [details](#backup-restore-hermes-cli-backup-py-hermes-cli-subcommands-backup-py) |
| Fallback chain CLI (`hermes_cli/fallback_cmd.py` + `fallback_config.py`) | ✅ core | `fallback.rs`: runtime chain already existed (`[model] fallbacks` specs + `agent::with_fallback_specs` / `parse_fallback_spec`); this adds the management CLI — `list` (primary + … [details](#fallback-chain-cli-hermes-cli-fallback-cmd-py-fallback-config-py) |
| Active session leases (`hermes_cli/active_sessions.py`) | ✅ core | `active_sessions.rs`: cross-process lease registry at `<home>/runtime/active_sessions.json` guarded by flock on `active_sessions.lock` (hermes `_FileLock`); entries carry … [details](#active-session-leases-hermes-cli-active-sessions-py) |
| Config management CLI (`hermes_cli/config.py` config_command) | ✅ core | `config_cmd.rs`: `show` (panel header + paths + full config with secret-key redaction via `status::redact_key`), `get <key> [--json]` (dotted paths into config.toml; ALL_CAPS keys … [details](#config-management-cli-hermes-cli-config-py-config-command) |
| Shell completion (`hermes_cli/completion.py`) | ✅ core | `ulnclaw completion <shell>` via clap_complete: bash / zsh / fish (hermes set) plus elvish / powershell; generated from the live clap command tree, so it tracks subcommands automatically (hermes walks the argparse tree for the same reason); SIGPIPE restored to default so piping into `head` exits cleanly |
| Setup dump & version (`hermes_cli/dump.py`, `build_info.py`) | ✅ core | `ulnclaw dump [--show-keys]`: plain-text, copy-pasteable setup summary — version + git SHA/commit-date, os, profile, home, model/provider, effective terminal backend with … [details](#setup-dump-version-hermes-cli-dump-py-build-info-py) |
| Memory CLI (`main.py cmd_memory`) | ✅ core | `ulnclaw memory`: per-store status (entries + bytes for `memory/MEMORY.md` agent notes and `memory/USER.md` user profile, injected into every turn's system prompt); `ulnclaw memory reset [all\|memory\|user] [--yes]`: hermes-style erase banner (`◆ file (desc) — N bytes`), interactive `yes` confirmation unless `--yes`, per-file `✓ Deleted` report; REPL `/memory` shows current contents; P323 exposes `GET /api/memory` (status census) and `POST /api/memory/reset` (targeted erase) with a persistent-memory section in the desktop Config view (hermes `/api/memory` parity) |
| Approval mode CLI (`hermes_cli/approval_mode.py`) | ✅ core | `ulnclaw approvals [manual\|smart\|off]`: shows the effective terminal-approval mode or persists a new one via the canonical config writer (`approvals.mode` in config.toml), re-reads the file to verify the value became effective and reports usage errors / managed-config failures hermes-style; mode semantics (`manual` human prompt, `smart` auxiliary-guardian LLM first, `off` auto-approve outside the hardline floor) match the terminal guard |
| Prompt-size diagnostic (`hermes_cli/prompt_size.py`) | ✅ core | `ulnclaw prompt-size [--json]`: measures the fixed per-call payload — system prompt split into its four tiers (base identity / persistent memory / environment / volatile date+model) with chars + bytes, memory-file sizes, tool count + JSON-schema KB, per-toolset schema sizes largest-first (answers "what should I disable to cut tokens?"), and installed SKILL.md sizes largest-first (skills load on demand, not part of the base prompt); shares `agent::DEFAULT_SYSTEM_PROMPT` and the same building blocks as `Agent::effective_system_prompt`, so the numbers match what is actually injected; P321 exposes `GET /api/ops/prompt-size` (desktop Doctor ops panel — hermes `/api/ops/prompt-size` parity) |
| Debug share bundle (`hermes_cli/debug.py`) | ✅ core | `ulnclaw debug report [--lines N] [--no-redact] [--output DIR]`: collects the hermes-style share bundle locally (no pastebin upload) — `report.txt` (force-redacted `ulnclaw dump` + agent/errors/gateway log tails) plus each present full log, every file self-contained with the dump header and a redaction banner; one snapshot per file derives both tail and full views (rotation-safe), secrets pass through the redaction engine plus email masking, `.1` rotation fallback, on-disk logs never modified |
| Skill bundles (`agent/skill_bundles.py`, `hermes_cli/bundles.py`) | ✅ core | `ulnclaw bundles list\|show\|create\|delete\|reload`: YAML bundles in `<home>/skill-bundles/` naming skill sets to load together (`name/description/skills/instruction`, file stem as fallback name, slug normalization shared with skills, duplicate slugs first-wins, broken YAML skipped without breaking discovery); REPL `/<bundle> [instruction]` loads every member skill's SKILL.md into one turn with the hermes header (loaded/missing lists, bundle instruction, user instruction), bundles win over same-named unknown commands, hyphen/underscore interchangeable; missing skills skipped with a note (forgiving `-s` preloading stance) |
| Import agent setups (`hermes_cli/agent_import.py`) | ✅ core | `ulnclaw import-agent [claude-code\|codex] [--source DIR] [--dry-run] [--overwrite]`: detect→parse→map→apply with per-item imported/skipped/conflict/error records; claude-code: … [details](#import-agent-setups-hermes-cli-agent-import-py) |
| Sessions retitle-skills (`hermes_cli/sessions_cmd.py retitle-skills`) | ✅ core | `ulnclaw sessions retitle-skills [--limit N] [--apply]` (dry run by default): `list_skill_scaffolded_sessions` (titled sessions whose first user turn matches the … [details](#sessions-retitle-skills-hermes-cli-sessions-cmd-py-retitle-skills) |
| Secrets vaults (`agent/secret_sources/`) | ✅ core | `src/secrets.rs` + `ulnclaw secrets status\|sync [--apply]`: external secret sources applied at startup before any provider reads env (hermes env-loader hook). Three sources with … [details](#secrets-vaults-agent-secret-sources) |
| Credential pool (`agent/credential_pool.py` + `hermes_cli/auth.py` + dashboard `/api/credentials/pool`) | ✅ core | P330 lean port: `src/credential_pool.rs` — `<home>/credentials-pool.json` store of manual per-provider API-key entries. Gateway `GET /api/credentials/pool` (per-provider rows with … [details](#credential-pool-agent-credential-pool-py-hermes-cli-auth-py-dashboard-api-credentials-pool) |
| iron-proxy egress firewall (`hermes egress`, `agent/proxy_sources/iron_proxy.py`) | ✅ core | `src/iron_proxy.rs` + `src/egress_cmd.rs` + `ulnclaw egress install\\|setup\\|start\\|stop\\|restart\\|reload\\|status\\|disable\\|config` + `/egress` slash: managed iron-proxy v0.39.0 … [details](#iron-proxy-egress-firewall-hermes-egress-agent-proxy-sources-iron-proxy-py) |
| Computer Use (`tools/computer_use/`) | ✅ core | `src/computer_use.rs` + `ulnclaw computer-use status\|doctor\|install`: background desktop control via the cua-driver daemon (MCP over stdio, hermes `cua_backend.py`). Full hermes … [details](#computer-use-tools-computer-use) |
| Plugins (`hermes_cli/plugins.py`, `agent/shell_hooks.py`) | ✅ core | `src/plugins.rs` + `ulnclaw plugins list\|install\|update\|remove\|enable\|disable\|accept-hooks`: Rust-native port of the hermes plugin architecture via the shell-hook wire protocol (a … [details](#plugins-hermes-cli-plugins-py-agent-shell-hooks-py) |
| Messaging platforms (`gateway/platforms/`) | ✅ core | `src/messaging.rs` — the hermes platform-gateway architecture runs inside `ulnclaw gateway`: adapters normalize incoming chat messages into a `MessageEvent`, a per-chat session … [details](#messaging-platforms-gateway-platforms) |
| Interactive clarify (`tools/clarify_gateway.py` + WhatsApp interactive) | ✅ core | `src/clarify_gateway.rs` + messaging integration — the `clarify` tool works in messaging sessions: prompts register in a bounded gateway registry (hermes state cap), render on the … [details](#interactive-clarify-tools-clarify-gateway-py-whatsapp-interactive) |
| Speech-to-text (`tools/transcription_tools.py` + gateway STT pipeline) | ✅ core | `src/stt.rs` — hermes' audio STT pipeline: `[stt]` config (enabled/echo_transcripts/provider/language + per-provider blocks with hermes defaults), built-in providers … [details](#speech-to-text-tools-transcription-tools-py-gateway-stt-pipeline) |
| OAuth login + skill sync (`hermes_cli/portal_cli.py`, `tools/skills_sync_client.py`) | ✅ core | `src/oauth.rs` + `src/skills_sync.rs`: service-agnostic port of hermes' portal auth + Skill Sync. `ulnclaw auth login` runs the RFC 8628 Device Authorization Grant against any … [details](#oauth-login-skill-sync-hermes-cli-portal-cli-py-tools-skills-sync-client-py) |
| OAuth upstream proxy (`hermes_cli/proxy/`) | ✅ core | `src/proxy_cmd.rs` + `ulnclaw proxy start\|status\|providers` (P179): local OpenAI-compatible proxy that lets external apps ride the user's stored OAuth subscription instead of a … [details](#oauth-upstream-proxy-hermes-cli-proxy) |
| Desktop GUI (`apps/desktop` Electron) | ✅ core | `desktop-electron/` — **ulnclaw desktop**: since v0.7.0 ulnclaw ships a faithful port of hermes' Electron desktop (v2026.8.3) itself, retiring the earlier Tauri 2 shell: React 19 … [details](#desktop-gui-apps-desktop-electron) |
| Sessions browse (`hermes_cli/sessions_cmd.py browse` + curses picker) | ✅ core | `ulnclaw sessions browse [--source S] [--limit N]`: raw-mode TUI on a TTY (crossterm port of the curses picker — alternate screen, ↑/↓/PgUp/PgDn/Home/End navigation with … [details](#sessions-browse-hermes-cli-sessions-cmd-py-browse-curses-picker) |
| Session resume & live-session continuity (`cli.py --resume/--continue`) | ✅ core | Global `-r/--resume <id-or-prefix>` + `-c/--continue` flags on `chat` and `run`: the whole REPL conversation lives in ONE session row (previously every turn created a fresh row) … [details](#session-resume-live-session-continuity-cli-py-resume-continue) |
| Sessions repair (`hermes_state.py repair_state_db_schema`) | ✅ core | `ulnclaw sessions repair [--check-only] [--no-backup]`: health probe (`db_opens_cleanly` — `PRAGMA journal_mode` first-statement trip, `integrity_check`, sessions read, FTS MATCH read probe, rolled-back FTS write probe) then escalating strategies — FTS5 `'rebuild'` in place, `REINDEX` for stale B-tree indexes, `sqlite_master` de-duplication via `writable_schema` (keeps the FTS index), drop-FTS-schema + `VACUUM` with rebuild on next store open (`initialize_schema` backfills a lagging external-content index); timestamped raw backup + WAL/SHM sidecars first; failure points at offline `sessions recover`; runs before the store opens since a malformed schema is exactly the case where open fails |
| Sessions delete/rename/optimize (`hermes_cli/sessions_cmd.py`) | ✅ core | `ulnclaw sessions delete <id> [--yes]` (id or unique prefix via `resolve_session_id` — LIKE-escaped prefix match, exact id wins, ambiguous → not found; y/N confirm unless `--yes`; messages + FTS rows removed first), `sessions rename <id> <title...>` (hermes `sanitize_title`: ASCII/Unicode control-char stripping, whitespace collapsing, empty → title cleared, 100-char limit, cross-session title uniqueness; reports the stored title), `sessions optimize` (FTS5 `'optimize'` segment merge + best-effort WAL checkpoint + `VACUUM`; reports merged-index count and before/after size using `logical_size_bytes` page accounting so WAL lag can't understate the win) |
| Supply-chain security audit (`hermes_cli/security_audit.py`) | ✅ core | `ulnclaw security audit [--json]`: on-demand OSV.dev audit of pinned MCP server packages (`npx pkg@ver` / `uvx pkg==ver`, scoped npm packages included); unpinned/local entries are skipped silently rather than guessed; `querybatch` + per-vuln detail fetch (severity from `database_specific`/`ecosystem_specific`, deduped fixed versions, summaries truncated at 100 chars); findings sorted by severity and grouped by source, human + JSON rendering; hermes' venv/plugin surfaces don't apply to a static Rust binary; P321 exposes it as `GET /api/ops/security-audit` (runs in-process, surfaced in the desktop Doctor ops panel — hermes `/api/ops/security-audit` parity) |

### Detailed notes (tool parity)

<a id="secret-redaction-agent-redact-py"></a>

#### Secret redaction (`agent/redact.py`) — ✅ core

- terminal output (foreground + process log/wait) and read_file content pass through the redactor:
  ~55 vendor-prefix tokens (sk-/ghp_/glpat-/AKIA/xox…/JWT/private keys/DB connstrings/auth & x-api-key
  headers), ENV-assignment masking for env-dump commands, JSON/YAML secret fields otherwise
- file-read content gets non-reusable `«redacted:prefix…»` sentinels so agents can't write truncated
  keys back (hermes #35519)
- web-URL query-param redaction stays opt-in
- the Smart-DENY owner-override flow IS fully ported — `smart_denied` flag, once/deny-only choices,
  one-operation scope, no persistence under smart-DENY, human-approval reset of the denial breaker
  (see the smart-approval row)
- profile secret scopes ARE ported — see the multiplexing row

<a id="blueprints-tools-blueprints-py"></a>

#### Blueprints (`tools/blueprints.py`) — ✅ core

- skills that declare `metadata.hermes.blueprint.schedule` in frontmatter become schedulable:
  `skills blueprints` (list), `skills schedule <name>` (creates the `blueprint:<skill>` cron job with
  the skill attached), `skills unschedule <name>`
- malformed blueprint blocks error loudly
- `skills list` marks blueprints with the schedule. P229 wired the hermes blueprint suggestion flow:
  `skills blueprints` lazily registers a pending `blueprint`-source suggestion for every unscheduled
  blueprint (hermes `register_blueprint_suggestion` parity — dedup-latched, MAX_PENDING-capped
- accepting produces the same `blueprint:<skill>` job as `skills schedule`). The unified suggestion
  surface is `cron/suggestions.rs` + REPL/CLI `/suggestions` (accept/dismiss/catalog/clear, curated
  starter catalog). `export_blueprint` (the `skills publish` share path) remains not ported — ulnclaw
  has no skills hub/publish surface

<a id="delegate-task"></a>

#### `delegate_task` — ✅ full

- parallel sub-agents, depth limit, isolated context, child sessions
- hermes v2026.8.3 background semantics: top-level delegations dispatch fire-and-forget (`mode:
  background`, delegation_id, live transcripts under `cache/delegation/live/<id>/task-N.log`) and ONE
  consolidated result re-enters the conversation when all children finish (REPL drain + gateway
  session-chat drain)
- orchestrator children (depth > 0) stay synchronous
- one-shot/stateless sessions force synchronous execution with a note (`tools/async_delegation.py`
  port incl. durable sqlite registry: dispatches + consolidated results persist to
  `async_delegations`, startup recovery turns rows still `running` after a crash into terminal
  `unknown` outcomes, drains claim undelivered rows through the durable delivery-claim lifecycle —
  per-claim `delivery_attempts`, 300s stale-claim takeover, completions whose origin session is gone
  converge to terminal `dropped` after 8 attempts, successful injection marks `delivered` under the
  claim token)
- `GET /v1/delegations` + `/v1/delegations/:id` registry endpoints (ulnclaw ops extension)

<a id="browser-12-tools"></a>

#### `browser_*` (12 tools) — ✅ full

- CDP WebSocket client (`browser` module): endpoint discovery, page session, accessibility snapshots
  with element refs, click/type/scroll/press/screenshot/evaluate/dialogs
- `ULNCLAW_BROWSER_CDP` accepts ws://, http://host:port, or `auto` (supervisor launches a managed
  headless Chrome/Chromium)
- hermes SSRF guards ported (`browser/guard.rs`): sensitive-query + cloud-metadata floor
  unconditional, private-address guard for non-local endpoints or containerized terminals,
  post-redirect recheck, JS URL-literal screening for console/eval, raw-CDP allowlist on private pages
- browser outputs force-redacted
- live endpoint override via REPL `/browser connect` and gateway `/v1/browser/connect|disconnect|status`
- Camofox REST backend via `CAMOFOX_URL`
- cloud browser providers (Browserbase / Browser Use / Firecrawl) via `browser/cloud.rs` — hermes
  provider registry semantics incl. `[browser] cloud_provider` selection, legacy availability walk,
  session expiry retirement, and atexit session release (see the Cloud browser providers row)

<a id="close-terminal-read-terminal-focus-pane-open-preview"></a>

#### `close_terminal`, `read_terminal`, `focus_pane`, `open_preview` — ✅ core

- desktop GUI affordances (hermes `close_terminal_tool.py` / `read_terminal_tool.py` /
  `focus_pane_tool.py` / `open_preview_tool.py`): registered only under `ULNCLAW_DESKTOP=1`, routed
  through the `desktop` bridge — a host app installs an emitter (`ulnclaw::desktop::set_emitter`) that
  receives `(ui_session_id, event, payload)` events (`terminal.close`, `pane.reveal`, `preview.open`)
  plus a blocking `read_terminal` callback
- without a wired host they report "desktop only", never kill processes, and normalize bare domains
  (`www.cnn.com` → https, `localhost:3000` → http)
- `react_to_message` (hermes `react_to_message_tool.py` port): agent emoji tapbacks — one reaction
  per author, same-emoji toggles off, defaults to the latest user message (`messages_back` steps
  earlier, `message_row_id` targets exactly), persisted in `messages.display_metadata` and painted
  live via the `message.reaction` bridge event
- gated on `ULNCLAW_DESKTOP=1` **and** `[display] message_reactions`. Since P231 the bridge events
  ride `GET /api/desktop/events` SSE when the gateway is a desktop shell's child, so the desktop
  webview is a first-class emitter host

<a id="discord-discord-admin"></a>

#### `discord`, `discord_admin` — ✅ core

- P245 full port of hermes `tools/discord_tool.py` (`src/discord_tool.rs`): 15 REST-API actions
  split into `discord` core (fetch_messages / search_members / create_thread) and `discord_admin`
  (guild/channel/role/member/pin/thread management)
- token resolved under the profile secret scope
- schema gated twice — privileged intents from `GET /applications/@me` via non-blocking detection
  (memory cache → 24 h disk cache keyed by sha256(token)[:16] → permissive default + one background
  detection, hermes `_detect_capabilities_nonblocking`) hide the GUILD_MEMBERS actions and annotate
  the missing MESSAGE_CONTENT case, and `[discord] server_actions` (comma string or array) allowlists
  both the schema and call time
- per-guild permissions are not pre-checked — 403s are enriched with actionable permission guidance (hermes `_enrich_403`)
- 4 MiB response / 64 KiB error-body caps, 15 s timeout

<a id="feishu-doc-read-feishu-drive"></a>

#### `feishu_doc_read`, `feishu_drive_*` — ✅ core

- P246 full port of hermes `tools/feishu_doc_tool.py` + `tools/feishu_drive_tool.py`
  (`src/feishu_doc_tool.rs`): `feishu_doc_read` (toolset `feishu_doc`) reads a document's raw content
  via `/open-apis/docx/v1/documents/:id/raw_content`
- `feishu_drive_list_comments` / `feishu_drive_list_comment_replies` / `feishu_drive_reply_comment`
  / `feishu_drive_add_comment` (toolset `feishu_drive`) cover comment-thread list/reply/add with
  `user_id_type=open_id`, `page_size` clamped 1–100, hermes-faithful request bodies (text_run elements
  / reply_elements)
- credentials resolve secret scope → env/`.env` → `[messaging.feishu]` config, tenant tokens cached
  per app id with early refresh (~110 min) — hermes injects a thread-local lark client so the tools
  only work inside a Feishu comment context, ulnclaw works in any session with credentials configured
  (superset)

<a id="spotify-7-tools"></a>

#### `spotify_*` (7 tools) — ✅ core

- P247 full port of hermes `plugins/spotify` (`src/spotify_tool.rs` + `src/spotify_auth.rs`):
  `spotify_playback`
  (get_state/currently-playing/play/pause/next/previous/seek/set_repeat/set_shuffle/set_volume/recently_played
  incl. 204 empty-state descriptions), `spotify_devices` (list/transfer), `spotify_queue` (get/add),
  `spotify_search` (7 item types), `spotify_playlists`
  (list/get/create/add_items/remove_items/update_details), `spotify_albums` (get/tracks),
  `spotify_library` (tracks/albums × list/save/remove) — Spotify Web API client with bearer auth, one
  401 retry on forced token refresh, friendly 401/403-Premium/404/429 error mapping
- id/URI/open.spotify.com-URL normalization
- auth via `ulnclaw spotify-auth login` — PKCE S256 loopback flow (hermes `login_spotify_command`
  parity: same scopes, default redirect `http://127.0.0.1:43827/spotify/callback`, state-nonce
  validation, RFC 7636 challenge), tokens stored under `providers.spotify` in `auth.json` with 120
  s-skew early refresh and dead-token quarantine on terminal refresh failure (hermes
  `resolve_spotify_runtime_credentials` parity)
- tools gate on stored auth status (`logged_in`)

<a id="yb-5-yuanbao-tools"></a>

#### `yb_*` (5 yuanbao tools) — ✅ core

- P248 full port of hermes `tools/yuanbao_tools.py` (`src/yuanbao_tool.rs`): `yb_query_group_info` /
  `yb_query_group_members` (find/list_bots/list_all + mention hint, role labels user/yuanbao_ai/bot)
  run as WS biz RPCs over the live adapter (`src/yuanbao.rs` `YuanbaoHandle` — hermes
  `get_active_adapter` parity, re-registered per session) with new proto encoders/decoders
  (`encode_query_group_info` / `encode_get_group_member_list` + nested GroupInfo / repeated MemberInfo
  rsp decoding)
- `yb_send_dm` resolves recipients by partial-name member lookup, sends chunked C2C text with group
  context plus media via the COS upload path (`MEDIA:` tags in the message are extracted like hermes
  `extract_media`)
- `yb_search_sticker` / `yb_send_sticker` use the ported sticker catalog (id/name/random lookup,
  TIMFaceElem, optional group quote-reply ref)
- tools gate on adapter liveness (hermes `_check_yuanbao`)
- divergence: ulnclaw has no per-turn session env, so `chat_id`/`group_code` are explicit arguments

<a id="send-message"></a>

#### `send_message` — ✅ core

- P259 full port of hermes `tools/send_message_tool.py` + `gateway/channel_directory.py`
  (`src/send_message_tool.rs` + `src/channel_directory.rs`): actions `send`/`list`/`react`/`unreact`
  with the faithful target grammar (bare platform → `<PLATFORM>_HOME_CHANNEL` / `EMAIL_HOME_ADDRESS`
  home channel, `platform:chat_id`, `platform:chat_id:thread_id` Telegram topics / Discord threads,
  `@username`, Slack `C/G/D` + `U…` user + `@handle` + thread-ts forms, Matrix
  `!room`/`@user`/`:$thread`, Weixin/WhatsApp-JID/E.164/photon-GUID/ntfy/email/yuanbao guards),
  human-friendly channel-name resolution (exact id → exact name/display-label → unambiguous prefix →
  unambiguous substring) against a persistent channel directory (`<home>/channel_directory.json`)
  refreshed by every inbound dispatcher event and overlaid by a user-maintained `channel_aliases.json`
- `MEDIA:<path>` tags deliver natively on Telegram (photo/document with hermes
  `_media_caption_split` 1024-char caption semantics), Discord (single multipart message,
  `payload_json` + files) and Slack (`chat.postMessage` + modern three-step file uploads) with an
  honest text-description fallback elsewhere
- reactions via Telegram `setMessageReaction`, Discord `PUT/DELETE …/reactions/…/@me`, Slack
  `reactions.add/remove`, with `message_id` omitted → most recent inbound message recalled from the
  directory
- gated on live platform adapters (hermes `_check_send_message`)

<a id="video-generate-bfl-flux3"></a>

#### `video_generate`, `bfl_flux3_*` — ✅ core

- `video_gen.rs` provider registry (hermes plugin design: single-available auto-select,
  configured-name fail-closed, `success_response`/`error_response` contract) + unified
  `video_generate` tool (text/image/reference-to-video, soft validation, model resolution arg >
  `[video_gen]` config > provider default)
- `managed_gateway.rs` Nous tool-gateway transport (auth.json bearer + `TOOL_GATEWAY_USER_TOKEN`,
  `{vendor}-gateway` URL building, presigned `nous-upload:` media uploads)
- all six `bfl_flux3_*` tools with pinned schemas, local-path upload prep, poll-until-done retrieval
  (throttle/transport-error handling, 240s backstop), signed-URL download to `~/Downloads` with
  `.part` staging + collision suffixes + prompting guide
- `video_gen_xai.rs` xAI Imagine backend (OAuth access-token reuse from auth.json → `XAI_API_KEY`
  fallback, text/image-to-video model routing incl. 1.5 model, edit/extend submit+poll flows) +
  `xai_video_edit`/`xai_video_extend` tools (public-HTTPS-URL validation, `provider_not_configured`
  gating)
- `video_gen_backends.rs` FAL backend (six model families — LTX 2.3, Pixverse v6, Veo 3.1, Seedance
  2.0, Kling v3 4K, Happy Horse — capability-driven payloads, `FAL_KEY` direct queue REST or Nous
  `fal-queue` managed gateway) and DeepInfra backend (OpenAI-compatible `/videos` create→poll→download
  into `~/videos`)
- no OAuth refresh — cached Nous tokens are used as-is

<a id="project-list-project-create-project-switch"></a>

#### `project_list`, `project_create`, `project_switch` — ✅ core

- full port of hermes `tools/project_tools.py` + `hermes_cli/projects_db.py`: per-profile
  `projects.db` (projects / project_folders / project_meta / discovered_repos, WAL with DELETE
  fallback + additive column migrations), slug validation + `-2` collision suffixes, multi-folder
  workspaces with primary pointer (implicit first-folder primary, demote/repoint on removal),
  archive/restore/hard-delete with folder cascade, active-project pointer, longest-prefix
  `project_for_path` resolution, deterministic kanban branch names
  (`<slug>/<task-id>[-<title-slug>]`), repo-discovery cache with policy reconciliation
- tools ship in the opt-in `project` toolset (GUI sessions only — off the core coding set, hermes
  parity) and the host app installs a workspace re-anchor callback
  (`projects_db::set_project_workspace_callback`)

<a id="skill-usage-telemetry-learning-graph-skill-usage-learning-graph-learning-mutations"></a>

#### Skill usage telemetry + learning graph (`skill_usage`, `learning_graph`, `learning_mutations`) — ✅ core

- hermes `tools/skill_usage.py` + `agent/learning_graph.py` + `agent/learning_mutations.py` ports:
  `<home>/skills/.usage.json` sidecar (view/use/patch counters, lifecycle state, pinning,
  agent-created provenance, atomic writes), telemetry wired into `skill_view`/`skill_manage` (bump
  view/patch, mark agent-created, forget on delete), skill archive/restore via `skills/.archive`
  (collision timestamp suffixes, pinned skills refused)
- learning graph payload — learned-skill filter (agent-created or used), `related_skills` edges,
  memory cards from `MEMORY.md`/`USER.md` bullet entries, lexical memory→skill edges (top-4 per card),
  clusters + density stats
- journey node mutations (`node_detail`/`delete_node`/`edit_node`) aligned with the memory tool's bullet format

<a id="learning-timeline-journey-cli-learning-graph-render-journey"></a>

#### Learning timeline / `journey` CLI (`learning_graph_render`, `journey`) — ✅ core

- hermes `agent/learning_graph_render.py` + `hermes_cli/journey.py` ports:
  `learning_graph_render.rs` — desktop-ported color math (palette derivation, complementary memory
  ink, smoothstep age gradient), recency computation (timed + ordinal fallback), day/month/year
  bucketed timeline with proportional skill/memory bars colored by dominant category (learning
  heatmap), numbered charted-signal markers, cumulative trajectory sparkline, legend/axis/summary
  trimmings
- `ulnclaw journey` CLI — timeline frame (`--reveal`, `--width/--height`, `--no-color`), `--play`
  animation, `--json` payload dump, `journey list`, `journey delete <node> [-y]` (skills archived,
  memories rewritten), `journey edit <node>` via `$EDITOR`
- TUI pre-render (`render_frames`) and the GUI star-map remain desktop-only surfaces

<a id="skill-curator-cli-curator"></a>

#### Skill curator CLI (`curator`) — ✅ core

- hermes `hermes_cli/curator.py` local half (the LLM consolidation run stays desktop-side):
  `curator.rs` — idle-days computation (activity with created_at fallback), prune candidate selection
  (agent-created, unpinned, non-archived, idle ≥ N days, idlest first), status summary, relative
  timestamp rendering
- `skill_usage.rs` reports — `usage_report` (every skill on disk with provenance/counters/last
  activity), `unmanaged_report` / `list_unmanaged_skill_names` / `adopt_skill` (provenance stamping),
  `list_archived_skill_names`
- CLI `ulnclaw curator status\|pin\|unpin\|archive\|restore\|list-archived\|usage [--sort
  activity\|name\|recent] [--json]\|prune [--days N] [--dry-run] [-y]\|adopt [names \|
  --all-unmanaged] [--dry-run] [-y]\|list-unmanaged`
- also hardened the gateway env-override tests with a process-wide env lock
- P316 exposed the curator over HTTP — `GET /api/curator` (status summary + archived list +
  activity-sorted usage table) + `POST /api/curator/pin|unpin|archive|restore` (pinned skills refuse
  archival), rendered in the desktop Skills view's curation section

<a id="persistent-goals-ralph-loop-goals"></a>

#### Persistent goals / Ralph loop (`goals`) — ✅ core

- hermes `hermes_cli/goals.py` port: `goals.rs` — `GoalContract`
  (outcome/verification/constraints/boundaries/stop_when, alias-table `parse_contract` so an
  incidental colon isn't mangled, empty-field omission, labelled `render_block`), `GoalState` serde
  round-trip (status, turn budget, subgoals, parse/transport failure counters, pid/session/time wait
  barriers), `parse_judge_response` (verdict + legacy `done` bool, code-fence strip, embedded-JSON
  extraction, wait-directive downgrade when no target), background-process block rendering for the
  judge
- `GoalManager` per-session orchestration persisted in `state_meta` keyed `goal:<session_id>`
  (set/set_contract/pause/resume/clear/mark_done, subgoal add/remove/clear,
  wait_on/wait_on_session/wait_for_seconds/stop_waiting with lazy auto-clear, status_line,
  next_continuation_prompt with contract>subgoals>plain priority, render_contract)
- fail-open `judge_goal` via the `goal_judge` auxiliary task (contract>subgoals>plain prompt,
  background processes, transport vs parse failure tracking) + `draft_contract`
- `evaluate_after_turn` state machine split into a pure testable `apply_verdict` (wait-barrier
  short-circuit without burning a turn, WAIT park, DONE, transport auto-pause at 5, parse auto-pause
  at 3, turn-budget exhaustion, continue) + async judge wrapper
- `migrate_goal_to_session`
- terminal.rs gains background-process pid capture +
  `background_process_running`/`background_process_exists`/`list_background_processes` backing session
  wait barriers
- REPL `/goal` (status/show/draft/pause/resume/clear/wait/unwait, inline contract, auto-kick) +
  `/subgoal` (list/add/remove/clear)
- `AuxiliaryTaskConfig.max_tokens` config knob

<a id="gateway-profile-multiplexing-p-profile-cdp-session-liveness"></a>

#### Gateway profile multiplexing (`/p/<profile>`) + CDP session liveness — ✅ core

- hermes api_server profile-prefix middleware port: every gateway route is mirrored under `/p/<profile>/...`
- `[gateway] multiplex_profiles = true` backs each mirror with its own stack (agent from
  `[profiles.<name>]` override, profile-scoped home `<home>/profiles/<name>` —
  state.db/approvals.json/cron/skills), lazily built + cached (`ProfileHub`), unknown profile → 404
  `Unknown or unconfigured profile`
- multiplexing off → prefix accepted but served by the default profile (hermes `_resolve_request_profile` parity)
- mirrors enforce the same bearer auth. CDP client hardening: `CdpClient.is_connected` (read/write
  loops flip a closed flag on socket loss and fail in-flight calls fast — no 30s timeout wedge),
  `with_session` drops dead cached sessions and reopens transparently. Profile secret scopes (P222,
  hermes `agent/secret_scope.py` port): with multiplexing on, every `/p/<profile>/...` request runs
  inside that profile's fail-closed secret scope (`<home>/profiles/<name>/.env` + hydrated external
  sources, genuinely-global vars excluded)
- scoped reads resolve against the scope only — no cross-profile process-env fallthrough — and
  unscoped credential reads fail loud with `UnscopedSecretError` instead of leaking another profile's
  value
- the cron scheduler installs a scope around every job run, `spawn_scoped` re-installs the captured
  scope inside spawned run tasks (hermes `copy_context()` parity), and multiplexing off restores
  plain-env behavior everywhere

<a id="repl-display-composer-ux-hermes-cli-focus-view-py-prompt-stash-py-clipboard-py"></a>

#### REPL display & composer UX (`hermes_cli/focus_view.py`, `prompt_stash.py`, `clipboard.py`) — ✅ core

- `src/focus_view.rs`, `src/prompt_stash.rs`, `src/clipboard.rs` — three hermes CLI-UX modules.
  **Focus view** (`/focus [on\|off\|status]`): display-only reduced-output mode — snaps tool progress
  to `off` while remembering the configured mode (restored verbatim on `/focus off`), counts hidden
  tool lines honestly per turn (only what the configured mode would have shown), prints the `⋯ N tool
  lines hidden · /focus off to show` recovery line after each turn, plus a `◉ focus` status-bar
  segment
- display-only invariant: never changes what is sent to the model. **Tool progress** (`/verbose
  [off\|new\|all\|verbose]`): hermes tool_progress_mode cycle over REPL tool-callback scrollback (`⚙
  <tool>` lines
- `new` dedupes consecutive repeats). **Prompt stash** (`/stash [text\|list\|pop [n]\|drop
  <n>\|clear]`): session-scoped in-memory stack of parked drafts (the hermes Ctrl+S gesture: content →
  park, empty + 1 item → pop, empty + 2+ → browse
- newest-first, 20-item cap, 60-char previews, `📌 n` prompt indicator, never written to disk).
  **Clipboard** (`/paste`): cross-platform clipboard-image extraction saved as PNG under
  `<home>/clipboard/` (macOS pngpaste/osascript, Windows/WSL2 PowerShell WinForms + Get-Clipboard +
  FileDropList fallbacks, Linux wl-paste on Wayland with non-PNG normalization via ImageMagick and
  xclip on X11) + `write_clipboard_text` (pbcopy → Set-Clipboard base64 → wl-copy → xclip → xsel,
  CJK-safe) + SSH-session detection (OSC 52 hint)
- desktop-side Ctrl+S keybinding stays in the desktop shell

<a id="sessions-prune-archive-stats-session-filters-py"></a>

#### Sessions prune/archive/stats (`session_filters.py`) — ✅ core

- `session/filters.rs` — duration parsing (`5h`/`30m`/`2d`/`1w`, bare number = days), point-in-time
  parsing (durations = that long ago
- ISO timestamps naive=local), epoch formatting, `PruneFilters` with typed WHERE-clause builder
  (ended-only, last-active COALESCE(MAX(message ts), started_at), source/end_reason exact, title/model
  case-insensitive substring, cwd prefix, message/token/tool-call bounds, tri-state archived) +
  human-readable `describe()`
- store `list_prune_candidates` (oldest-activity-first), `prune_sessions` (messages + FTS first),
  `archive_sessions` (soft-hide, idempotent), `set_session_archived`, `session_count_by_source`
- CLI `ulnclaw sessions prune|archive` (hermes semantics: bare prune = older than 90 days, any
  filter suppresses the implicit cutoff, bare archive refused, preview + y/N confirm + `--dry-run`,
  `--include-archived`) and `sessions stats` (totals, per-source counts, db size)
- hermes' billing/chat/branch/cost filters map to columns ulnclaw doesn't track and stay unported

<a id="welcome-banner-update-check-banner-py"></a>

#### Welcome banner & update check (`banner.py`) — ✅ core

- `banner.rs`: skin-aware welcome panel rendered with box-drawing chars — braille claw-swipe hero +
  model line (shortened slug, `.gguf` strip, 28-char cap, models.dev context lookup via
  `spawn_blocking` + 2s cap), `approvals.mode = "off"` warning (hermes YOLO line), cwd + session id,
  "Available Tools" grouped by enabled toolsets (8 shown, `+N more toolsets`), skills by category with
  `+N more` overflow, `N tools · N skills · /help for commands` summary
- ULNCLAW block-letter wordmark on terminals ≥95 cols (hermes logo gate)
- git update check with 6h `$ULNCLAW_HOME/.update_check` cache invalidated on version change —
  scoped `git fetch` behind-count with shallow-clone SHA-compare path, official-SSH remotes via `git
  ls-remote` (count unknown → `-1` sentinel), repo dir = `$ULNCLAW_REPO` → build-time
  `CARGO_MANIFEST_DIR` → `$ULNCLAW_HOME/ulnclaw`
- `prefetch_update_check` background thread + `get_update_result(500ms)` while the agent is constructed
- panel-title version label `ulnclaw vX · upstream <sha8>` (+carried commits), latest-tag lookup
  with gitee release URL (per-process cache). Deferred: rich hyperlinks in the title, skin
  `banner_hero`/`banner_logo` overrides

<a id="browser-cdp-attach-layer-browser-connect-py"></a>

#### Browser CDP attach layer (`browser_connect.py`) — ✅ core

- `browser/connect.rs`: Chromium-family candidate discovery for macOS/Windows/Linux (incl. WSL
  `/mnt/c` install paths) covering Chrome/Chromium/Brave/Edge
- dual-stack loopback CDP probes — `is_browser_debug_ready` (`/json/version` → `/json`, TCP connect
  for `ws://…/devtools/browser/…`), `discover_local_cdp_url` (IPv4 then `[::1]`, catching browsers
  pushed to IPv6-only by an IPv4 squatter)
- port arbitration — `local_port_in_use` distinguishes free-vs-squatted, `find_free_debug_port`
  requires bindability on both loopbacks
- diagnostics-rich visible debug launch `launch_chrome_debug` (per-candidate `LaunchAttempt` states
  ready/starting/exited/spawn-failed, stderr tail in `<home>/chrome-debug/launch-stderr.log`, exit-0
  single-instance absorption hint, `manual_chrome_debug_command` fallback incl. macOS `open -a` form)
- `connect_local_default` composes the full hermes `/browser connect` default flow. REPL `/browser
  connect` (no URL) runs that flow, sets the live override on success and injects the hermes system
  note into the conversation
- `/browser disconnect` injects the revert note. Managed-launch candidate list also gains
  Brave/Edge. Gateway `/v1/browser/*` unchanged (already parity)

<a id="doctor-doctor-py"></a>

#### Doctor (`doctor.py`) — ✅ core

- `doctor.rs` + `ulnclaw doctor` CLI: hermes boxed-banner report with ✓/⚠/✗/ℹ checks in sections —
  Version & Updates (banner git state + 6h-cached upstream behind-count from P61), Configuration Files
  (config.toml presence/TOML validity/model configured, `.env` key scan), Directory Structure (home +
  sessions/skills/memory/cron/checkpoints/logs, state.db), Auth Providers (`resolve_api_key` chain:
  config → ULNCLAW_API_KEY → OPENAI_API_KEY → ANTHROPIC_API_KEY
- keyless local providers noted), External Tools (git, Chromium-family candidates from P62, bundled
  SQLite), Toolsets (enabled/disabled + unknown-name detection via `resolve_toolset`), Skills
  (installed count + frontmatter sanity), Profiles (per-profile model/toolset overrides + profile
  home)
- `--fix` creates missing home/subdirs and a default config.toml (hermes `--fix` fast path),
  `--online` probes the provider endpoint (`/v1/models` with bearer key
- `/api/tags` for ollama-style locals) via blocking reqwest, `--json` emits the serialized report
- issues summary with numbered manual steps + `--fix` tip, exit 0 parity (hermes doctor never fails the shell)

<a id="session-insights-agent-insights-py"></a>

#### Session insights (`agent/insights.py`) — ✅ core

- `insights.rs` + `ulnclaw insights [--days N] [--source S] [--json]` CLI + REPL `/insights [days]`
  + gateway chat `/insights [N] [--days N] [--source S]` slash command: InsightsEngine over state.db
  (second WAL reader) — overview (sessions/messages/tool-calls, in/out/total tokens, avg session
  duration, active days), models.dev-backed USD cost estimation per session/model (`get_model_info`
  pricing
- provider hinted from config, unknown → "cost unknown"), model breakdown sorted by tokens, source
  breakdown (hermes platform breakdown), tool-call breakdown from `role='tool'` rows (top 30),
  activity patterns (hour-of-day + Mon-first weekday buckets, peak detection), top-5 sessions by
  tokens with title/date
- archived sessions excluded, `--source` filter parity, terminal renderer with █ bar charts (hermes
  `_bar_chart`), `format_duration_compact` + K/M token formatting, JSON report via serde, skill usage
  breakdown scanning assistant `tool_calls` JSON for `skill_view`/`skill_manage` calls (per-skill
  loads/edits + last-used dates, summary totals, ranked `top_skills` — hermes
  `_get_skill_usage`/`_compute_skill_breakdown` semantics), `get_usage_breakdown` tools+skills payload
  (hermes dashboard-route shape), and the compact markdown `format_gateway` renderer that backs the
  gateway `/insights` slash reply
- P328 adds `GET /api/analytics/models` (per-model sessions/messages/tokens/last-used over a 1-365
  day window, store-side GROUP BY — hermes `/api/analytics/models` parity
- cost/capability columns stay in the Models-view catalog tables) rendered as the Models-view usage table

<a id="pets-agent-pet-hermes-cli-pets-py"></a>

#### Pets (`agent/pet/` + `hermes_cli/pets.py`) — ✅ core

- `src/pets.rs` + `ulnclaw pets list|install|select|show|off|scale|remove|doctor|hatch`: petdex
  mascot engine — public manifest fetch (petdex.dev, 300 s in-process cache + background prefetch,
  petdex-host-pinned asset downloads), profile-scoped store under `<home>/pets/<slug>/` (pet.json +
  spritesheet: install/load/list/resolve/rename/remove/zip-export/idle-frame thumbnails with
  anti-traversal slugs), atlas taxonomy inference (8-row legacy vs 9-row Codex sheets) with state
  aliases (waving/jumping/running), `derive_pet_state` activity→animation mapping (error→failed,
  celebrate→jump, completed→wave, awaiting-input→waiting, tool-running→run, reasoning→review), and
  terminal rendering in four modes — kitty graphics protocol (chunked APC transmit +
  Unicode-placeholder virtual placement payloads with row/column diacritics), iTerm2 inline images,
  hand-rolled DEC sixel (median-cut ≤255-color quantizer), and a truecolor Unicode half-block fallback
  with a legibility floor — driven by `[display.pet]` config (enabled/slug/scale
  0.1–3.0/render_mode/unicode_cols) persisted by select/off/scale
- LLM pet hatch pipeline (`agent/pet/generate/` → `src/pets_atlas.rs` + `src/pets_generate.rs` +
  `ulnclaw pets hatch`): base-draft → grounded row-strip generation → frame extraction → atlas
  compose/validation → store registration with hermes-verbatim prompts, chroma-key background removal
  (border flood-fill + saturated-key fast path + hole repair), xcorr cell registration/normalization,
  running-left mirroring, idle fallback, 4-wide concurrent row generation with 3 attempts each,
  `[pets]` config for the OpenAI-compatible images endpoint (image_base_url/image_api_key/image_model
- key falls back to OPENAI_API_KEY/ULNCLAW_API_KEY, model to gpt-image-2), `--style` hints
  (pixel/plush/clay/sticker/flat-vector/3d-toy/painterly/auto), `--drafts N` drafts-only mode and
  `--base <path>` hatch-from-image
- REPL `/pet` (toggle/list/scale/off/<slug> adopt) + `/hatch <description>` slash commands (hermes
  cli_commands_mixin semantics, progress printing included)
- P126 ported the desktop generate overlay: the desktop shell's hatch dialog (prompt + style + draft
  count → base-draft grid pick → live row progress → spritesheet preview + auto-adopt) rides new
  gateway hatch jobs (`POST /api/pets/hatch`, `GET /api/pets/hatch/:id`, `POST
  /api/pets/hatch/:id/pick|cancel`, `GET /api/pets/hatch/:id/draft/:index`). Known diffs: one
  OpenAI-compatible endpoint instead of hermes' Nous/OpenRouter/Krea provider registry
- sheets are PNG-encoded (the `image` crate has no WebP encoder) while decoding accepts both

<a id="suggested-automations-cron-suggestions-py-suggestions-cmd-py"></a>

#### Suggested automations (`cron/suggestions.py` + `suggestions_cmd.py`) — ✅ core

- `cron/suggestions.rs`: JSON store at `<home>/cron/suggestions.json` (owner-only writes via
  tmp+rename) with hermes semantics — pending/accepted/dismissed statuses, dedup-key latching (decided
  keys never re-offered), MAX_PENDING=5 backlog cap, source validation
  (catalog/blueprint/usage/integration), resolution by id / 1-based pending index / exact title
- `accept` materializes the stored job_spec into a real cron job via `CronStore` and latches accepted
- `clear_resolved` prunes accepted records only (dismissed kept for dedup memory)
- curated 4-entry starter catalog (daily briefing, important-mail monitor, weekly review, workday
  start reminder — prompts adapted self-contained, schedules verified against `parse_schedule`) with
  idempotent `seed_catalog_suggestions`
- shared dispatch `handle_suggestions_command` behind REPL `/suggestions [accept N|dismiss
  N|catalog|clear]` and `ulnclaw suggestions` CLI (accept/add/schedule + dismiss/no/reject alias
  parity, usage text)

<a id="status-report-hermes-cli-status-py-hermes-cli-subcommands-status-py-timefmt-py"></a>

#### Status report (`hermes_cli/status.py` + `hermes_cli/subcommands/status.py` + `timefmt.py`) — ✅ core

- `status.rs`: port of `show_status` — panel header + Environment (version / home / config.toml /
  .env), Model+Provider+Base URL, API Keys (config.toml `model.api_key` row + 20-entry vendor env
  table with alternate fallback, every value passed through `redact_key`), Terminal Backend, Browser
  (endpoint + binary discovery), Gateway (listen / auth key / multiplex
- `--deep` adds gateway-port TCP probe), Scheduled Jobs (active/total + next run), Sessions (total +
  freshest), Skills (installed + pending suggestions), Updates (git upstream check, 6h cache), footer
  pointers to doctor/init
- `relative_time()` is the timefmt.py port (just now / Nm / Nh / yesterday / Nd / date)
- CLI `ulnclaw status [--all] [--deep]` (`--all` shares the default redacted rendering)

<a id="log-viewer-file-logging-hermes-cli-logs-py-hermes-cli-subcommands-logs-py-hermes-logging-py-rotating-handlers"></a>

#### Log viewer + file logging (`hermes_cli/logs.py` + `hermes_cli/subcommands/logs.py` + `hermes_logging.py` rotating handlers) — ✅ core

- `logs.rs`: viewer port — `LOG_FILES` registry (agent/errors/gateway), `_parse_since` (Ns/m/h/d
  cutoffs), timestamp/level/logger-name regexes (logger regex extended for Rust `::` targets),
  `_matches_filters` (level>= / session substring / since / component prefixes), `_read_last_n_lines`
  (whole-file <=1MiB, growing backward chunks beyond), `_read_tail` (20x window when filtered),
  `list_logs` (size + age table), `tail_log` header/filter text parity, `_follow_log` 300ms poll
- writer port — `RotatingFile` (max_bytes x backup_count shift rotation, agent.log 5MBx3 INFO+,
  errors.log 2MBx2 WARNING+, gateway.log 5MBx3 target-filtered) + `HermesLogFormat` (`YYYY-MM-DD
  HH:MM:SS,mmm LEVEL [session] target: message`) wired into tracing via per-file layers
- `COMPONENT_PREFIXES` adapted to ulnclaw module paths
- CLI `ulnclaw logs [agent|errors|gateway|list] [-n] [-f] [--level] [--session] [--since] [--component]`
- P325 exposes `GET /api/logs` (inventory without `file`, filtered per-file tail with `file` +
  level/component/search/session/since) and the desktop Doctor logs panel gains the file picker +
  search (hermes `/api/logs` parity)

<a id="self-updater-hermes-cli-subcommands-update-py-update-cmd-py-git-core"></a>

#### Self-updater (`hermes_cli/subcommands/update.py` + `update_cmd.py` git core) — ✅ core

- `update.rs`: `--check` port of `_cmd_update_check` — branch resolution (`--branch` > current
  branch > master, hermes `_resolve_update_branch`), shallow-repo awareness (`--depth 1` fetch +
  presence-only SHA compare), upstream-preferred fetch for the default branch with origin fallback,
  fetch-error classification (network / auth / generic), compare-ref verification, behind-count via
  rev-list
- apply path port of `_cmd_update_impl` git core — auto-stash (`--include-untracked`, unmerged-index
  cleanup, `ulnclaw-update-autostash-<ts>` names), fork detection via origin URL + auto-added
  `upstream` remote (`_is_fork` / `_add_upstream_remote`, skipped for local-path origins), `git merge
  --ff-only` (diverged history reported, never force-touched), stash restore with conflict guidance,
  old..new commit log, then `cargo build --release` as the Rust dependency-refresh equivalent
- Python-specific machinery (venv/pip/npm, Windows locking, desktop hand-off, docker/nix, systemd
  restarts) is N/A for a compiled Rust binary
- CLI `ulnclaw update [--check] [--branch N] [-y]`
- P324 exposes `GET /api/update/check` (non-applying drift report) + `POST /api/update` (in-place
  apply) with an update panel in the desktop Doctor view (hermes `/api/hermes/update*` parity)

<a id="backup-restore-hermes-cli-backup-py-hermes-cli-subcommands-backup-py"></a>

#### Backup & restore (`hermes_cli/backup.py` + `hermes_cli/subcommands/backup.py`) — ✅ core

- `backup.rs`: full zip backup (hermes `run_backup` — exclusion sets
  `_EXCLUDED_DIRS/_SUFFIXES/_NAMES` adapted, self-exclusion of the output zip, progress/errors
  summary, `ulnclaw-backup-<ts>.zip` naming, dir-output handling) with WAL-safe SQLite snapshots via
  `sqlite backup()` (`safe_copy_db`, hermes `_safe_copy_db`) +
  `verify_sqlite_integrity`/`is_zeroed_sqlite_file`/`copy_db_and_verify`
- import (hermes `run_import` — `validate_backup_zip` markers, `detect_prefix` incl.
  `.ulnclaw`/`ulnclaw`, zip-slip-guarded staging overlay, `_IMPORT_SKIP_NAMES` runtime-state
  protection, `_SECRET_FILE_NAMES` 0600 tightening)
- quick snapshots (hermes `create/list/restore_quick_snapshot` + `_prune_quick_snapshots` —
  manifest.json, traversal-proof ids, atomic-ish .db replace, keep=20 pruning, max_file_size skip for
  pre-update)
- cron safety net `restore_cron_jobs_if_emptied` (counts `cron_jobs` in state.db instead of jobs.json)
- pre-update hook wired into `ulnclaw update` + pre-import snapshot + post-import safety net in `ulnclaw import`
- CLI `ulnclaw backup [-o] [-q] [-l]` / `backup list|restore <id>|prune [keep]` / `ulnclaw import <zip>`

<a id="fallback-chain-cli-hermes-cli-fallback-cmd-py-fallback-config-py"></a>

#### Fallback chain CLI (`hermes_cli/fallback_cmd.py` + `fallback_config.py`) — ✅ core

- `fallback.rs`: runtime chain already existed (`[model] fallbacks` specs +
  `agent::with_fallback_specs` / `parse_fallback_spec`)
- this adds the management CLI — `list` (primary + numbered chain, hermes `cmd_fallback_list` text),
  `add <provider:model>` (rejects the primary itself via same-deployment compare and exact duplicates,
  case-insensitive provider), `remove <N|provider:model>`, `clear` (TTY confirm, `-y` to skip)
- storage written by line-level config.toml editing (`save_chain`: replace/insert `fallbacks =
  [...]` inside `[model]`, preserving comments/ordering, creates the file when absent)
- hermes interactive picker replaced by explicit spec argument (ulnclaw has no curses picker)
- CLI `ulnclaw fallback [list|add|remove|clear] [-y]`

<a id="active-session-leases-hermes-cli-active-sessions-py"></a>

#### Active session leases (`hermes_cli/active_sessions.py`) — ✅ core

- `active_sessions.rs`: cross-process lease registry at `<home>/runtime/active_sessions.json`
  guarded by flock on `active_sessions.lock` (hermes `_FileLock`)
- entries carry lease_id/session_id/surface/pid + `/proc/<pid>/stat` start time so recycled PIDs
  cannot spoof liveness (hermes psutil create_time pairing)
- `prune_dead` reclaims leases of dead processes on every mutation
- `try_acquire/release/transfer_active_session`, `release_orphaned_leases`,
  `active_session_registry_snapshot`, `summarize_holders` ("desktop x4, cli, oldest Nh ago") +
  `active_session_limit_message` parity
- cap configured via `[gateway] max_concurrent_sessions` (0/unset disables
- hermes top-level/gateway.* resolution), enforced at chat REPL startup with a Drop-released lease
  (gateway request path is stateless per-request and not slot-limited)

<a id="config-management-cli-hermes-cli-config-py-config-command"></a>

#### Config management CLI (`hermes_cli/config.py` config_command) — ✅ core

- `config_cmd.rs`: `show` (panel header + paths + full config with secret-key redaction via
  `status::redact_key`), `get <key> [--json]` (dotted paths into config.toml
- ALL_CAPS keys resolve through process env + `.env` like hermes `_is_env_config_key`), `set <key>
  <value> [--force]` (scalar coercion bool/int/float/array/table/string, nested table creation,
  unknown-section advisory hermes parity
- env-style keys written to `.env`), `unset <key>` (config.toml or `.env` line removal), `path` /
  `env-path`, `edit` ($EDITOR)
- storage rewrite via toml round-trip (TOML replaces hermes YAML
- comments are the documented trade-off)
- P318 added the raw escape hatch — `GET/PUT /api/config/raw` serves and atomically replaces
  config.toml verbatim (parse-validated, comments preserved), with a Raw TOML dialog in the desktop
  Config view (hermes `/api/config/raw` parity)
- P320 adds `/api/env` (list file/process posture without values, set/delete env-style keys) with an
  env-keys manager in the Config view (hermes `/api/env` parity)
- P336 adds `POST /api/env/reveal` (unredacted value for one key from `.env`/process env,
  rate-limited to 5 reveals per 30 s — hermes env-reveal parity) plus `GET /api/config/defaults` and
  `GET /api/config/schema` (flattened dotted-path leaves with type + default — lean hermes config
  defaults/schema parity), surfaced in the desktop Config view as per-key 👁 reveal buttons and a
  schema reference section

<a id="setup-dump-version-hermes-cli-dump-py-build-info-py"></a>

#### Setup dump & version (`hermes_cli/dump.py`, `build_info.py`) — ✅ core

- `ulnclaw dump [--show-keys]`: plain-text, copy-pasteable setup summary — version + git
  SHA/commit-date, os, profile, home, model/provider, effective terminal backend with `TERMINAL_ENV`
  override note, `api_keys:` set/not-set/redacted with the shell-only-vs-`.env` mismatch warning
  (managed backends read `.env`, not the login shell), `features:` toolsets / MCP servers / memory
  provider / gateway listen+auth / cron active-total / skills / checkpoints, plus non-default
  `config_overrides:`
- `ulnclaw version [--no-update-check]`: version line + install directory/method + live update
  status via the `update --check` machinery
- git-less installs fall back to a baked `.ulnclaw_build_sha` marker (hermes `.hermes_build_sha` parity)
- P321 exposes `GET /api/ops/dump` (always redacted, desktop Doctor ops panel — hermes `/api/ops/dump` parity)

<a id="import-agent-setups-hermes-cli-agent-import-py"></a>

#### Import agent setups (`hermes_cli/agent_import.py`) — ✅ core

- `ulnclaw import-agent [claude-code|codex] [--source DIR] [--dry-run] [--overwrite]`:
  detect→parse→map→apply with per-item imported/skipped/conflict/error records
- claude-code: `CLAUDE.md` → `memory/MEMORY.md` entries (headings become context prefixes, code
  blocks/tables skipped, dedup), `mcpServers` from `.claude.json` + `settings.json` → config.toml
  `[[mcp.servers]]` (name conflicts kept, secret-looking env vars stripped and reported), `skills/` →
  `skills/claude-code-imports/`, permission rules reported as converted patterns (no ulnclaw allowlist
  surface)
- codex: `AGENTS.md` + `memories/*.md` → memory entries, `config.toml [mcp_servers.*]` →
  `[[mcp.servers]]`, `skills/` → `skills/codex-imports/`
- memory merges back up the store first (`.bak.<ts>`), 20k-char migration budget
- credential files never read, dry-run writes nothing

<a id="sessions-retitle-skills-hermes-cli-sessions-cmd-py-retitle-skills"></a>

#### Sessions retitle-skills (`hermes_cli/sessions_cmd.py retitle-skills`) — ✅ core

- `ulnclaw sessions retitle-skills [--limit N] [--apply]` (dry run by default):
  `list_skill_scaffolded_sessions` (titled sessions whose first user turn matches the `[IMPORTANT: The
  user has invoked the` scaffold), `describe_skill_invocation` re-derives the typed invocation from
  bundle + single-skill formats (quoted name, `User instruction:` / `alongside the skill invocation:`
  extraction, excerpt-joint split, whitespace collapse), `generate_title_forced` bypasses the
  auto-title gate, `_is_titlelike` rejects command-output candidates, unique-title collisions dedupe
  via `get_next_title_in_lineage` (`base #2`, `#3`, …)
- P223 output parity: hermes `every title already reflects the user's request.` / `✓ Re-titled N
  session(s).` summaries + full subcommand long description

<a id="secrets-vaults-agent-secret-sources"></a>

#### Secrets vaults (`agent/secret_sources/`) — ✅ core

- `src/secrets.rs` + `ulnclaw secrets status|sync [--apply]`: external secret sources applied at
  startup before any provider reads env (hermes env-loader hook). Three sources with full hermes
  precedence semantics — mapped outranks bulk, first claim wins, `preserve_existing` beats everything,
  `override_existing` beats pre-existing `.env`/shell but never another source, bootstrap-token vars
  are write-protected. `command`: any KEY=VALUE helper via `/bin/sh -c` (keepassxc-cli / secret-tool /
  tmpfs cat), hard timeout degrades to "no value", stderr discarded, 1 MiB output cap, quotes/comments
  parsed. `bitwarden`: Bitwarden Secrets Manager via `bws secret list <project> --output json`
  (managed `<home>/bin/bws` preferred over PATH, `BWS_SERVER_URL` passthrough, pinned v2.0.0
  auto-install from bitwarden/sdk-sm releases — sha256-verified zip, zip-slip-guarded extraction,
  staged 0755 install). `onepassword`: mapped `op://vault/item/field` bindings resolved via `op read
  -- <ref>` with a minimal allowlisted child env, empty values refused, per-reference failures degrade
  to warnings. Fetch errors are one-line warnings, never fatal. TTL fetch caches
  (`src/secrets_cache.rs` port of `agent/secret_sources/_cache.py`): atomic 0600 writes under
  `<home>/cache/` (0700 dir), TTL 0 disables both cache layers symmetrically, only complete error-free
  pulls are cached
- the Bitwarden cache is AES-256-GCM **encrypted** at rest (HKDF-SHA256 key derived from the
  bootstrap token, cache key bound as AAD, legacy plaintext cache deleted on migration). Interactive
  setup wizards: `secrets bitwarden setup|install|status|token|disable` (hermes 5-step flow — binary
  install → token → region → project picker via `bws project list` → test fetch → config save
- non-TTY fast path requires `--access-token`/`--server-url`/`--project-id`) and `secrets
  onepassword setup|status|set|remove|disable`. `secrets bitwarden token` rotates the access token
  without re-running the wizard (hermes `cmd_token`: masked prompt or `--access-token`, `0.` shape
  warning, probe-before-store via `bws project list` with the NEW credential unless `--no-verify`,
  configured-project visibility warning, .env persist + both cache layers dropped). Not ported: the
  Windows bws asset path stays untested

<a id="credential-pool-agent-credential-pool-py-hermes-cli-auth-py-dashboard-api-credentials-pool"></a>

#### Credential pool (`agent/credential_pool.py` + `hermes_cli/auth.py` + dashboard `/api/credentials/pool`) — ✅ core

- P330 lean port: `src/credential_pool.rs` — `<home>/credentials-pool.json` store of manual
  per-provider API-key entries. Gateway `GET /api/credentials/pool` (per-provider rows with 1-based
  indexes, redacted token previews, request counts), `POST /api/credentials/pool` (provider
  normalization + default `key #N` labels), `DELETE /api/credentials/pool/:provider/:index` (sticky by
  construction — manual entries have no re-seeding source). Rotation: highest-priority tier first,
  then least-used entry
- request counts persist best-effort (atomic store writes, no cross-process lock).
  `UlncLawConfig::resolve_api_key` resolves config literal > pooled entry > env vars, so pool
  membership is the curation signal
- the desktop Config view gained the pool section (rows + add/remove). Known differences: no
  automatic seeding from env/OAuth/config sources, no suppression/removal-step registry, no OAuth
  singleton entries, no `hermes auth` CLI surface, and the xAI pool proxy adapter stays unported

<a id="iron-proxy-egress-firewall-hermes-egress-agent-proxy-sources-iron-proxy-py"></a>

#### iron-proxy egress firewall (`hermes egress`, `agent/proxy_sources/iron_proxy.py`) — ✅ core

- `src/iron_proxy.rs` + `src/egress_cmd.rs` + `ulnclaw egress
  install\|setup\|start\|stop\|restart\|reload\|status\|disable\|config` + `/egress` slash: managed
  iron-proxy v0.39.0 (github.com/ironsh/iron-proxy, Apache-2.0) TLS-intercepting egress proxy for
  Docker sandboxes — the sandbox receives minted per-provider proxy tokens
- the daemon swaps them for the real upstream credentials (read from its OWN environment) on
  allowlisted hosts and rejects everything else. `install [--force]`: pinned-release download with
  SHA-256 checksum + best-effort GPG detached-signature verification via system `gpg` (ephemeral
  keyring
- hard-fails on a present-but-bad signature). `setup [--tunnel-port N]
  [--from-bitwarden\|--no-bitwarden] [--rotate-tokens] [--restart\|--no-restart]`: openssl CA (4096
  RSA, 10 years, 0600 atomic key write), token minting for every known provider in
  env/`<home>/.env`/Bitwarden (8 bearer + 3 header-auth providers — Anthropic `x-api-key`, Azure
  `api-key`, Gemini `x-goog-api-key` with `GOOGLE_API_KEY` alias collapse), token preservation on
  re-setup unless rotation is confirmed interactively (mappings backup), SigV4/GCP uncovered-provider
  warnings, writes `proxy.yaml` (v0.39 schema: allowlist + secrets transforms with fail-closed
  `require: true` + per-provider `match_headers` + query matching, docker-bridge bind on Linux
  (RFC1918-validated, never 0.0.0.0) / loopback on Docker Desktop, default SSRF deny CIDRs
  (IMDS/loopback/RFC1918/CGNAT/v4-mapped v6), loopback bearer-auth management API) + 0600
  `mappings.json`, enables `[proxy]` config keys (never silently downgrades bitwarden→env), stops a
  running daemon + optional restart. `start`: detached spawn with a minimal allowlisted env (mapped
  real secrets only, alias mirroring, proxy-chain strip, NO_COLOR), per-start nonce + pidfile/nonce
  files (O_EXCL/O_NOFOLLOW/ownership checks), PID-recycling defense via /proc environ → cmdline
  basename → ps fallback + starttime re-verification before SIGKILL, optional startup Bitwarden BSM
  refresh (fail-closed without `proxy.allow_env_fallback`), 5 s port-listening wait with log-tail
  diagnostics. `reload`: hot ruleset swap via management `POST /v1/reload` (422 validation / 401
  stale-key handling). `status [--show-tokens]`/`disable`/`config` and the `/egress` slash share a
  read-only snapshot (binary/version/config/CA/pid/listening/mappings/uncovered). Divergences: branded
  `ULNCLAW_IRON_PROXY_*` env vars (hermes `HERMES_IRON_PROXY_*`)
- archive extraction via system `tar`
- branded CA subject
- Docker-scope enforcement only (hermes v2026.8.3)

<a id="computer-use-tools-computer-use"></a>

#### Computer Use (`tools/computer_use/`) — ✅ core

- `src/computer_use.rs` + `ulnclaw computer-use status|doctor|install`: background desktop control
  via the cua-driver daemon (MCP over stdio, hermes `cua_backend.py`). Full hermes tool schema
  (capture som/vision/ax, click family by SOM element index or coordinates, drag, scroll, type, key
  combos, set_value, wait, list_apps/list_windows/focus_app, cua_browser_* typed-browser passthrough).
  Hermes precedence/approval semantics: capture + listings are free, every other action goes through
  the approval callback and fails closed unattended. Lazy shared MCP session
  (`start_session`/`end_session`, `set_config` max_image_dimension, cursor-overlay policy incl.
  `--no-overlay` auto-detect + `CUA_DRIVER_RS_TELEMETRY_ENABLED=0` default). `doctor` drives
  cua-driver's `health_report`. P228 adds the client-side vision post-processing layer: every action
  result's `screenshot_png_b64` is decoded and, when its longest edge exceeds `[computer_use]
  max_image_dimension`, downscaled client-side (aspect-preserving, `width`/`height` refreshed,
  fail-open) — belt-and-braces over the driver-side `set_config` cap so SOM/vision payloads stay
  bounded even with drivers that ignore it. The desktop TCC surface is wired too: `GET
  /api/tools/computer-use/status` reports `can_grant` and `POST
  /api/tools/computer-use/permissions/grant` spawns `cua-driver permissions grant` as a polled
  background action (macOS-only, 400 elsewhere — hermes semantics). Not ported: embedded-daemon/socket
  mode and context-level screenshot eviction (no reference implementation ships in the v2026.8.3
  checkout — the `tools/` tree is absent
- not inventing semantics without one)

<a id="plugins-hermes-cli-plugins-py-agent-shell-hooks-py"></a>

#### Plugins (`hermes_cli/plugins.py`, `agent/shell_hooks.py`) — ✅ core

- `src/plugins.rs` + `ulnclaw plugins list|install|update|remove|enable|disable|accept-hooks`:
  Rust-native port of the hermes plugin architecture via the shell-hook wire protocol (a static binary
  can't import Python plugins). Directory plugins at `<home>/plugins/<name>/plugin.toml` (manifest:
  hooks + `[[tools]]`)
- tools register as `plugin__<name>__<tool>` and run as subprocesses with `{"tool", "arguments"}`
  JSON on stdin. Config shell hooks `[hooks] <event> = ["cmd"]` with hermes first-use consent
  (`shell-hooks-allowlist.json`, `auto_accept` / `ULNCLAW_ACCEPT_HOOKS`). Full hermes `VALID_HOOKS`
  catalog (23 events)
- the core fires the 13 hermes emits at runtime: `pre_tool_call` (block decisions veto before
  approval), `post_tool_call`, `transform_llm_output`,
  `on_session_start`/`on_session_end`/`on_session_reset` (`/new`)/`on_session_finalize` (REPL exit),
  `pre_llm_call` (context responses append to the turn's user message, hermes turn-context semantics),
  `post_llm_call`, `pre_api_request`/`post_api_request`/`api_request_error` (around every provider
  call), and `pre_gateway_dispatch` (skip/rewrite platform messages BEFORE the allowlist gate)
- the remaining 10 are catalog-only in hermes v2026.8.3 too. `ulnclaw hooks list|test|revoke|doctor`
  (hermes `hooks` CLI) inspects consent state, fires default payloads, and probes each consented hook.
  P326 exposes the consent surface as `GET /api/ops/hooks` (per-command
  consented/pending/unknown-event state + valid events + allowlist census), `POST
  /api/ops/hooks/accept-all` and `POST /api/ops/hooks/revoke`, rendered in the desktop Plugins view
  (hermes `/api/ops/hooks` parity). P178 added the hermes git-install lifecycle: `plugins install
  <url|owner/repo[/subdir]> [--force] [--enable|--no-enable]` (shallow clone via non-interactive git
  with a 60 s cap, GitHub browser-URL/`#subdir`/`.git/`-boundary identifier resolution, subdir
  traversal guard, manifest-name discovery from plugin.toml or plugin.yaml, sanitized target under
  `<home>/plugins/`, interactive enable prompt), `plugins update <name>` (git pull inside the
  installed clone), and `plugins remove <name>` (directory removal). P234 ported the hook output-spill
  guard (`tools/hook_output_spill.py`): hook-injected `context` is appended to every subsequent API
  call, so any blob over `[hooks.output_spill] max_chars` (default 10K) is spilled to
  `<home>/hook_outputs/<session>/<uuid>.txt` and the prompt carries a head/tail preview + path
- I/O failures degrade to a bounded 'spill write failed' preview, never a failed turn
  (enabled/max_chars/preview_head/preview_tail/directory knobs
- session ids are path-sanitized). P351 exposes the marketplace pipeline over
  `/api/dashboard/plugins*` + `/api/dashboard/agent-plugins*` + `/api/dashboard/plugin-providers`
  (active dashboard-plugin list + rescan, a merged hub over a curated `plugin-hub/index.json` +
  local-directory candidates with install/update/remove, memory/context provider selection persisted
  to `[plugins]`, and `dashboard.hidden_plugins` visibility toggles — hermes plugin-hub parity)
- per-plugin static asset hosting (`/dashboard-plugins/*`) stays unported. Hermes Python plugin
  imports, entry-point packages, and provider registrations are not ported

<a id="messaging-platforms-gateway-platforms"></a>

#### Messaging platforms (`gateway/platforms/`) — ✅ core

`src/messaging.rs` — the hermes platform-gateway architecture runs inside `ulnclaw gateway`:
adapters normalize incoming chat messages into a `MessageEvent`, a per-chat session
(`platform-<name>-<chat>` via `create_named_session`) carries conversation continuity, and replies
go back through the platform with hermes-style chunking. Thirty self-contained adapters — nineteen
long-running loops:

**Telegram** — Bot API long-polling getUpdates/sendMessage, clarify inline keyboards with
  callback_query tap routing

**Discord** — Gateway v10 websocket IDENTIFY/heartbeat/MESSAGE_CREATE/INTERACTION_CREATE + REST
  send, clarify embed+buttons with interaction-callback routing

**Slack** — Socket Mode events_api envelopes + chat.postMessage, assistant-thread typing status via
  assistant.threads.setStatus — "is thinking..." default with a 30 s+ elapsed-time "still working… (Xm
  YYs)" heartbeat, `typing_status_text` override, Block Kit clarify buttons with interactive-envelope
  routing

**Signal**
- signal-cli HTTP daemon: SSE inbound with keepalive/stale-health reconnect, JSON-RPC 2.0 outbound
  with rate-limit retry, Note-to-Self promotion + outbound-echo suppression, group gating via
  `group_allowed_users` (`*` wildcard) with require-mention filtering, attachments via `getAttachment`
  base64 + mime sniffing + ADTS→m4a ffmpeg remux, `MEDIA:` replies as `base64Attachments`
- `[messaging.signal]` or SIGNAL_HTTP_URL/SIGNAL_ACCOUNT

**Weixin**
- WeChat personal accounts via the Tencent iLink Bot API: long-poll getupdates with persisted
  sync-buf resume, message-id + content-fingerprint dedup, DM/group intake policies
  (pairing/allowlist/open/disabled) mapped onto the ulnclaw allowlist∪pairing gate, disk-backed
  per-peer context_token echo store with session-expired tokenless fallback sends, AES-128-ECB
  encrypted CDN media both directions (image/video/file/voice with an SSRF host allowlist), 2000-char
  markdown-aware chunking with copy-friendly line wrapping + text debounce batching, getconfig typing
  tickets, QR login via `ulnclaw weixin login`
- `[messaging.weixin]` or WEIXIN_ACCOUNT_ID/WEIXIN_TOKEN

**QQ**
- official QQ Bot API v2: WebSocket gateway with Hello/Identify/Resume/heartbeat and hermes
  close-code semantics (4004 token refresh, 4006/4007/4009 session reset, 4008 rate-limit backoff,
  4914/4915 stop), C2C/group @-mention/guild-channel/guild-DM events with 300s message dedup, markdown
  (msg_type 2) or stripped-plain-text replies riding passive-reply msg_id with `msg_seq` generation,
  outbound media via inline-base64 under 8 MB or the three-step chunked upload (upload_prepare →
  presigned COS PUT + upload_part_finish → complete) with daily-quota (40093002) and part-retry
  (40093001) handling, voice notes via `asr_refer_text` first then raw audio into the `[stt]`
  pipeline, quoted-message (message_type 103) context merging, INTERACTION_CREATE acknowledgements +
  inline keyboards (exec-approval ✅ 允许一次 / ⭐ 始终允许 / ❌ 拒绝 callback buttons via the `send_exec_approval`
  contract, button clicks route back through the approval gateway with operator authorization — c2c
  must match the chat user, group/guild clicks must pass the allowlist ∪ pairing intake gate
- update-prompt ✓/✗ keyboards persist `y`/`n` answers to the `.update_response` file
- QR scan-to-configure onboarding ported (`ulnclaw qq login` — hermes `qr_register`:
  `create_bind_task` / `poll_bind_result` portal APIs on `q.qq.com` (`QQ_PORTAL_HOST` override),
  terminal QR + URL display, 2 s polling under a 600 s default deadline with up to 3 expired-QR
  refreshes, `client_secret` decrypted locally with AES-256-GCM (IV ‖ ciphertext ‖ tag layout, key
  never leaves the CLI)
- credentials persist to `<home>/qq/credentials.json` and back the config resolvers as a last fallback
- P220 ported the full setup wizard (`ulnclaw qq setup` — hermes `_setup_qqbot`): scan-or-manual
  credential choice, DM security-policy selection (pairing / open / allowlist with self-add and
  comma-list prompts) persisted into `[messaging.qq]` (`dm_policy` + `allow_from`, merge-safe TOML
  edits), and home-channel selection persisted as `QQBOT_HOME_CHANNEL` in `<home>/.env`
- hidden secret input via crossterm raw mode))
- `[messaging.qq]` or QQ_APP_ID/QQ_CLIENT_SECRET

`, and

**Yuanbao**
- Tencent Yuanbao app bots: WebSocket gateway sessions bootstrapped via an HMAC-SHA256 `sign-token`
  HTTP handshake (Beijing +08:00 timestamps, cached sign tokens), a hand-rolled protobuf wire codec
  (`src/yuanbao_proto.rs`: ConnMsg envelopes, AUTH_BIND/BIND_ACK, ping, push acks, 30 s private/group
  heartbeats), inbound push decoding with a 1.5 s per-sender debounce, DM/group intake policies
  (pairing/allowlist/open/disabled) mapped onto the allowlist∪pairing gate, markdown-aware 4000-char
  chunked replies sent over the WS (send-c2c/send-group), hermes no-reconnect close codes
  (4012/4013/4014/4018/4019/4021), outbound media ported (`MEDIA:` reply paths upload via
  `genUploadInfo` temporary COS credentials + a signed global-accelerate PUT — hermes
  `yuanbao_media.py` — then dispatch as TIMImageElem with parsed PNG/JPEG/GIF/WebP dimensions, md5
  uuid and TIM `image_format`, or TIMFileElem, caption appended as a TIMTextElem)
- stickers ported (hermes `yuanbao_sticker.py`: 59-entry builtin catalog with
  exact/containment/description/fuzzy lookup, inbound TIMFaceElem rendered as `[emoji: <name>]`,
  `STICKER:<name>` reply tags send catalog stickers as TIMFaceElem with the `data` JSON payload, proto
  `index` field 9)
- inbound media resolution/download ported (hermes ExtractContentMiddleware +
  MediaResolveMiddleware: image/file refs render as `[image|ybres:RID]` / `[file:name|ybres:RID]`
  anchors, resourceId exchange via `/api/resource/v1/download` with one 401 token-refresh retry,
  size-capped download into the media cache as attachments, anchors patched to local paths, 24 h
  resourceId cache with oldest-25% eviction, hermes placeholder filter
- quote/observed-media backfill ported (an adapter-side message-content cache + per-group observed
  buffer stand in for the hermes transcript store — ulnclaw transcripts carry no platform message ids:
  quoted already-local anchors inject as-is, leftover `[kind|ybres:RID]` anchors re-resolve,
  quote-free group turns backfill up to 12 refs from the last 50 messages newest-first with rid dedup,
  plus the `[Replying to: "..."]` quote pointer with a 500-char snippet
- forwarded WeChat chat records (elem_type 1009) deep-parse into the prompt — ForwardMsgData
  protobuf out of the TIMCustomElem ext_map, per-record 1000-char caps, `[kind|ybres:RID]` media
  markers resolving through the same download pipeline, `用户附言：` caption footer — while anchor patching
  matches by resourceId)
- `[messaging.yuanbao]` or YUANBAO_APP_ID/YUANBAO_APP_SECRET/YUANBAO_BOT_ID

**Email**
- hermes `platforms.email` plugin: IMAP implicit-TLS INBOX polling (UID SEARCH UNSEEN + UID FETCH
  RFC822) with a bounded seen-UID set seeded at startup, SMTP replies with `Re:` subject threading
  (`In-Reply-To`/`References`, generated Message-IDs) and multipart `MEDIA:` attachments, RFC 2047
  header decoding, text/plain-first body extraction with HTML-strip fallback, noreply/bulk-mail
  filtering, and fail-closed `Authentication-Results` SPF/DKIM/DMARC From-domain verification when the
  allowlist gates access (GHSA-rxqh-5572-8m77 parity
- opt-out `require_authenticated_sender = false` / EMAIL_TRUST_FROM_HEADER)
- `[messaging.email]` or EMAIL_ADDRESS/EMAIL_PASSWORD/EMAIL_IMAP_HOST/EMAIL_SMTP_HOST

**Mattermost**
- hermes `platforms.mattermost` plugin: REST v4 + WebSocket event stream —
  `authentication_challenge` WS auth with 30 s pings and exponential-backoff reconnect that stops on
  permanent 401/403, `posted` events with post-id dedup and channel-type mapping, `allowed_channels`
  whitelist + `require_mention` @bot gating with `free_response_channels` exemptions and mention
  stripping, thread-root replies (`reply_mode = "thread"`), authenticated file download into the media
  cache and multipart upload for outbound `MEDIA:` tags, 4000-char posts with `disable_mentions`
  props, REST typing indicator
- `[messaging.mattermost]` or MATTERMOST_URL/MATTERMOST_TOKEN

**Matrix**
- hermes `platforms.matrix` plugin core over the raw Client-Server API without an SDK: password
  login or access token (`whoami` discovery), long-polling `/sync` with initial-sync backfill skip,
  `m.room.message` text/notice handling with reply-fallback stripping, `allowed_users`/`allowed_rooms`
  gates + mention requirement with display-name/@user-id patterns and `free_response_rooms`, mxc://
  media download (authenticated-first) and upload for `MEDIA:` replies, transactional `m.text` sends
  chunked at `max_message_length` (default 16000, clamped 500..65535)
- E2EE is NOT supported — `m.room.encrypted` rooms are skipped with a warning
- `[messaging.matrix]` or MATRIX_HOMESERVER + MATRIX_ACCESS_TOKEN or MATRIX_USER_ID/MATRIX_PASSWORD

**DingTalk**
- hermes `platforms.dingtalk` plugin, hand-rolled Stream Mode without the Python SDK: `POST
  /v1.0/gateway/connections/open` gateway handshake + WebSocket CALLBACK frames on
  `/v1.0/im/bot/messages/get` with ACK replies, text/rich-text/audio-recognition/file-name/doc-card
  extraction, `allowed_users`/`allowed_chats` gates with `require_mention` (hermes default false) +
  `isInAtList` + regex `mention_patterns` wake words + `free_response_chats`, `downloadCode` media
  resolution through `/v1.0/robot/messageFiles/download` with cached OAuth2 access tokens,
  sessionWebhook markdown replies (20000-char chunks, hermes markdown normalization, 5-minute expiry
  margin, LRU-500 cache), Thinking/Done emoji reactions via `POST /v1.0/robot/emotion/reply|recall`
  (fire-and-forget 🤔Thinking on every inbound message, swapped to 🥳Done once after the final reply,
  idempotent per chat — hermes `_send_emotion`/`_fire_done_reaction`)
- AI streaming cards via card_1_0 when `card_template_id` is set (create STREAM card instance →
  deliver to the `dtv1.card//IM_GROUP.<conversationId>` / `IM_ROBOT.<senderStaffId>` space → one-shot
  full-content `streaming` update with `isFinalize`, webhook-markdown fallback on any failure,
  `robot_code` defaults to `client_id`, DM cards without `senderStaffId` skipped — hermes
  `_create_and_stream_card`)
- `[messaging.dingtalk]` or DINGTALK_CLIENT_ID/DINGTALK_CLIENT_SECRET/DINGTALK_CARD_TEMPLATE_ID/DINGTALK_ROBOT_CODE

**WeCom**
- hermes `platforms.wecom` plugin: WeCom AI Bot WebSocket gateway — `aibot_subscribe` handshake, 30
  s app-level pings, `aibot_msg_callback` events with msgid dedup, mixed/text/voice/appmsg/quote
  extraction with leading-mention stripping, `dm_policy`/`group_policy`
  (pairing/allowlist/open/disabled) gates, base64-or-URL inbound media with the WeCom AES-256-CBC
  `aeskey` file scheme (key-as-IV, PKCS#7), markdown replies via `aibot_respond_msg` bound to the
  inbound req_id (required for groups) with `aibot_send_msg` proactive-DM fallback, three-step
  `aibot_upload_media_*` chunked upload (512 KiB chunks, md5, 100-chunk cap) for native attachments
- inbound text batching merges the WeCom client's ~4000-char splits — session-scoped debounce (0.6 s
  quiet period, 2.0 s when the latest chunk sits at the 3900-char split threshold
- `WECOM_TEXT_BATCH_DELAY_SECONDS`/`WECOM_TEXT_BATCH_SPLIT_DELAY_SECONDS`, 0 disables), voice notes
  and media dispatch immediately (hermes `_enqueue_text_event`/`_flush_text_batch`)
- `[messaging.wecom]` or WECOM_BOT_ID/WECOM_SECRET

**Home Assistant**
- hermes `platforms.homeassistant` plugin: WebSocket API `/api/websocket` auth handshake
  (`auth_required` → `auth` → `auth_ok`) + `subscribe_events` state_changed stream with ping/pong
  keepalive and the hermes 5/10/30/60 s reconnect schedule, closed-by-default event filters
  (`watch_domains`/`watch_entities`/`watch_all` + `ignore_entities`) with per-entity cooldowns (30 s
  default), domain-specific human-readable formatting
  (climate/sensor/binary_sensor/light/switch/fan/alarm_control_panel, generic fallback), events
  dispatched on the synthetic `ha_events` channel, replies delivered as persistent notifications via
  the REST API (`persistent_notification/create`, 4096-char chunks, REST-only sends to avoid racing
  the event listener)
- the `notify/notify` standalone out-of-process sender is ported: a credential-only sender registers
  whenever HASS_URL + HASS_TOKEN are set (even with the live adapter disabled) and posts to
  `/api/services/notify/notify` with `{message, target}` (hermes `_standalone_send`)
- a live adapter overwrites the slot with its persistent-notification sender
- `[messaging.homeassistant]` or HASS_TOKEN/HASS_URL

**WhatsApp** — hermes `platforms.whatsapp` plugin, Baileys HTTP-bridge transport with the full
  bridge supervisor

- : the Baileys bridge (`scripts/whatsapp-bridge/bridge.js` + `package.json`) is bundled into the
  binary and synced to `<home>/scripts/whatsapp-bridge/` at runtime
- `src/whatsapp_bridge.rs` replicates the hermes adapter's connect lifecycle — `node`/`npm` PATH
  resolution, `npm install` gated by a `package.json` hash stamp (`node_modules/.hermes-pkg-hash`),
  stale-bridge cleanup via pidfile re-validation (`bridge.pid` carries PID + kernel start time so a
  recycled PID is never signalled) and LISTEN-state port occupants only, spawn of `node bridge.js
  --port N --session DIR --mode MODE` with stdout/stderr appended to `bridge.log`, two-phase readiness
  wait (HTTP up ≤15 s, then `status == "connected"` ≤15 s more, proceeding with a warning otherwise),
  and the `scriptHash` staleness handshake (`/health` reports the bridge's own source hash) when
  adopting an already-running bridge. Auto-spawn (`auto_spawn`, default true
- `WHATSAPP_AUTO_SPAWN`) applies only when `bridge_url` targets `127.0.0.1:<bridge_port>` — other
  URLs keep the external-bridge client behavior. Intake: waits for `/health` `connected` (QR pairing
  states surfaced with a pointer at `bridge.log`, `botJid` captured for mention detection), polls
  `/messages` every second, drops broadcast pseudo-chats and from-self echoes while self-chat owner
  messages (bridge `fromOwner` flag, `WHATSAPP_FORWARD_OWNER_MESSAGES`) pass intake with the `[owner
  reply]` prefix (hermes `_OWNER_REPLY_PREFIX`), DM allowlist∪pairing gates plus group
  `allowed_channels`/require-mention gating with `free_response_channels` opt-outs, inbound media
  (bridge-cached absolute paths or URLs) downloaded into the content-addressed media cache with
  bridge-mime fallbacks, fire-and-forget `/read` receipts, replies via `/send` (4000-char chunks) and
  `MEDIA:` tags via `/send-media` with extension-derived mediaType
- native polls via `/send-poll` (hermes `send_poll` — clarify prompts with 2–12 clean choices render
  as single-select polls whose picked option arrives as plain text resolving the pending question,
  numbered-text fallback), location pins via `/send-location` (hermes `send_location`), and message
  editing via `/edit` (hermes `edit_message`, transport primitive)
- inbound votes/locations surface as text events
- `[messaging.whatsapp]` or
  WHATSAPP_BRIDGE_URL/WHATSAPP_BRIDGE_PORT/WHATSAPP_SESSION_PATH/WHATSAPP_AUTO_SPAWN/WHATSAPP_ALLOWED_USERS

**IRC**
- hermes `platforms.irc` plugin: zero-dependency TCP client over rustls — hermes registration
  sequence (PASS/NICK/USER, 30 s RPL_WELCOME wait, optional NickServ IDENTIFY, JOIN), 433
  nick-collision retries with incrementing suffixes, PING/PONG keepalive, CTCP ACTION folding (`/me` →
  `* nick text`), channel intake requires `nick:`/`nick,`/`nick ` addressing (stripped before
  dispatch), allowlist is opt-in — empty allows all (hermes IRC semantics, nicks carry no identity),
  markdown-stripped replies (IRC variant: images → url, links → `text (url)`) split at the 510-byte
  line limit with 0.3 s flood pauses
- `[messaging.irc]` or IRC_SERVER/IRC_PORT/IRC_NICKNAME/IRC_CHANNEL

**ntfy**
- hermes `platforms.ntfy` plugin: HTTP streaming subscription `GET /<topic>/json?poll=false` NDJSON
  with 2/5/10/30/60 s reconnect backoff (reset when the stream stayed alive ≥ 60 s) and fatal 401/404
  stops, Bearer-or-Basic token auth, message-id dedup (300 s window, 1000 entries), `hermes-agent`
  echo-tag loop prevention, hermes trusted-channel identity model (user_id = topic
- publisher-controlled titles never authorize), replies POST to the publish topic with the echo tag
  + optional `X-Markdown`, 4096-char cap
- `[messaging.ntfy]` or NTFY_TOPIC/NTFY_SERVER_URL/NTFY_TOKEN

**SimpleX**
- hermes `platforms.simplex` plugin: WebSocket client for the `simplex-chat` daemon —
  corrId-correlated command frames (`hermes-` prefix echo filter), contact-request auto-accept
  (`/accept`), `rcvFileDescrReady` → fire-and-forget `/freceive`, `newChatItems` intake with
  own-message + `rcvMsgContent` filters, DM allowlist by contact id OR display name ∪ pairing,
  `SIMPLEX_GROUP_ALLOWED` group gate (`*` wildcard), daemon-local media cached with deferred
  voice-note delivery on `rcvFileComplete`, 0.8 s quiet-period text batching, replies via `@<id>` DMs
  and structured `/_send #<id> json` groups (voice notes vs documents by extension), 8000-char chunks
- `[messaging.simplex]` or SIMPLEX_WS_URL/SIMPLEX_ALLOWED_USERS

- plus eleven gateway-mounted webhook platforms (WhatsApp Cloud, Microsoft Graph change
  notifications, the generic webhook platform, BlueBubbles, Feishu, Twilio SMS, Microsoft Teams, LINE,
  Google Chat, the Raft wake endpoint, and the A2A agent server — detailed below). Hermes pairing
  semantics: every platform is allowlist-gated, empty allowlist fails closed and logs the ids to add.
  Interactive pairing codes (hermes `gateway/pairing.py` port, `src/pairing.rs`): unauthorized senders
  receive an 8-char CSPRNG code (salted-SHA-256 at rest, 1-hour expiry, 3 pending per platform, one
  request per user per 10 minutes, 5 failed approvals lock the platform out for an hour)
- `ulnclaw pairing list|approve|revoke|clear-pending` manages grants, and approved users join the
  allowlist as a union at the auth gate (`[messaging] pairing = true` default). Media attachments
  (hermes media-cache pipeline port, `src/media_cache.rs`): inbound Telegram
  photo/document/video/audio/voice (getFile download, largest photo size), Discord `attachments`, and
  Slack `files` (bot-bearer downloads) are cached content-addressed under `<home>/media-cache/`
  (SHA-256 names, hermes mime→ext table, 25 MB cap) and handed to the agent as path references with
  vision_analyze/video_analyze/read_file hints (hermes text-fallback semantics)
- outbound `MEDIA:<path>` reply tags become native uploads on Telegram (sendPhoto/sendDocument),
  Discord (multipart), and Slack (modern `files.getUploadURLExternal` → PUT →
  `files.completeUploadExternal` flow)
- media-only inbound messages flow without text.

**WhatsApp Cloud** — hermes `whatsapp_cloud.py` port, `src/webhook_platforms.rs`

: gateway-mounted `/webhooks/whatsapp` with the Meta verify handshake (hub.challenge echo),
`X-Hub-Signature-256` HMAC verification over the raw body, text + image/document/audio/video/sticker
ingress through the same allowlist∪pairing + plugin gates (inbound media downloaded via the Graph
`/media` object with Meta's per-type size caps into the content-addressed cache, captions carried as
message text), and Graph-API chunked text replies plus two-step native media sends (`/media`
multipart upload → media-id message). Microsoft

**Graph change-notification ingress** — hermes `msgraph_webhook.py` port

- : `/webhooks/msgraph` validationToken echo + required clientState verification, notifications
  surface as internal events with hermes semantics — receipt dedup (`id:<notification id>` ledger,
  5000 default, FIFO eviction), `accepted_resources` filter (exact / sub-path / `prefix*` wildcard),
  prompt rendering (custom `{dotted.path}` template via `prompt`, else pretty-printed sorted-JSON dump
  truncated at 4000 chars), `msgraph:<subscriptionId>` chat scope with receipt-key-or-sha1 message
  ids, hermes status codes (202 accepted/duplicate, 403 whole-batch auth failure, 400
  malformed/resource-not-accepted, 413 oversize)
- not ported: the `notification_scheduler` hook (no consumer exists in hermes either), source-CIDR
  allowlist + standalone health endpoint (hermes runs a standalone aiohttp server
- ulnclaw mounts the shared gateway).

**Generic webhook platform** — hermes `webhook.py` port

: `[messaging.webhook]` routes mounted at `/webhooks/hook/<name>` with multi-scheme signature
verification (Svix `svix-*` headers with base64 `whsec_` keys, GitHub `X-Hub-Signature-256`, GitLab
`X-Gitlab-Token`, timestamp-bound generic V2 with a no-V1-downgrade guard, legacy V1,
`INSECURE_NO_AUTH` for testing), a 300 s replay window, per-route fixed-window rate limiting
(default 30 req/min), delivery-id idempotency (`X-Webhook-Delivery-Id` or `svix-id`, 1 h TTL),
header event filtering (`X-Webhook-Event`/`X-GitHub-Event`/`X-Gitlab-Event`), `{event}`/`{body}`
prompt templates, delivery targets (`log`/`telegram`/`discord`/`slack`/`whatsapp_cloud`), and
`deliver_only` zero-LLM push notifications.

**BlueBubbles iMessage bridge** — hermes `bluebubbles.py` port

- : `[messaging.bluebubbles]` mounts `/webhooks/bluebubbles` on the gateway with password auth
  (query param — BlueBubbles webhooks cannot send custom headers — or
  `x-password`/`x-guid`/`x-bluebubbles-guid` headers), JSON payloads with a form-encoded fallback,
  hermes event gates (`new-message`/`message`/`updated-message` only
- from-me messages and tapback reactions 2000–2005/3000–3005 are silently acked), chat-GUID
  resolution through an LRU-500 cache with strict `chatIdentifier` matching (no participant fallback —
  hermes #24157) plus v1.9+ `chats[0]` extraction, attachment downloads into the content-addressed
  media cache, paragraph-split 4000-char replies with `chat/new` for address targets, multipart
  attachment sends, and startup ping + server-info + idempotent webhook registration.

**Feishu/Lark** — hermes `platforms.feishu` plugin, both transports — `connection_mode =
  "websocket"` is the hermes default

- : websocket mode runs the lark_oapi long connection natively (`src/feishu_ws.rs`: `POST
  /callback/ws/endpoint` AppID/AppSecret handshake returning the conn URL (`device_id`/`service_id`
  query params) + ClientConfig, hand-rolled protobuf `Frame` codec, CONTROL ping every PingInterval
  (default 120 s) with pong-payload ClientConfig re-tuning, split DATA packets reassembled by
  `message_id`/`seq` with a 5 s TTL, event frames dispatched without token/signature checks (SDK
  `_do_without_validation`) and acknowledged with the frame echoed back + `biz_rt` header and
  `{"code":200}` payload, jittered reconnect — ReconnectNonce 30 s jitter, ReconnectInterval 120 s,
  ReconnectCount −1 = forever)
- `connection_mode = "webhook"` keeps the gateway-mounted `/webhooks/feishu` with verification-token
  gating before challenge echo, SHA-256 webhook signature (`timestamp+nonce+encrypt_key+body`) when
  `encrypt_key` is set, event-id dedup, `im.message.receive_v1` text with mention-placeholder
  normalization (`@_user_N` → `@name`), image/file/audio/post handling with tenant-token resource
  downloads into the media cache, group require-mention gating, text replies via
  `/open-apis/im/v1/messages` (chat_id/open_id receive-id resolution) and image/file uploads for
  `MEDIA:` tags
- satellite handlers: `drive.notice.comment_add_v1` document-comment agent (hermes
  `feishu_comment.py` port — event parse + self/recipient/notice-type filters,
  `feishu_comment_rules.json` 3-tier access rules (exact `docType:token`/`wiki:token` key > wildcard
  `*` > top-level, field-by-field enabled/policy/allow_from fallback, allowlist∪pairing policies with
  `feishu_comment_pairing.json` approvals, mtime hot reload), OK reaction while the agent works,
  parallel doc-meta + comment batch_query, whole-document vs local-thread timeline assembly with
  hermes' windowed selection + referenced-doc link extraction and wiki node resolution, hermes prompt
  builders, per-document agent session (`comment-doc:<file_type>:<file_token>`), 4000-char chunked
  reply into the comment thread with whole-comment fallback on error 1069302) and
  `vc.bot.meeting_invited_v1` meeting invites (hermes `feishu_meeting_invite.py` port — parsed into a
  join-the-meeting prompt dispatched as a DM to the inviter through the normal pipeline)
- reactions ported (Typing processing badge via `/open-apis/im/v1/messages/<id>/reactions` while the
  agent works — removed on completion, swapped for CrossMark on failure, `FEISHU_REACTIONS` default on
- user reactions on the bot's own messages route as `reaction:<added|removed>:<emoji>` synthetic
  events after a bot-authorship check, bot/app-origin reactions dropped to break the lifecycle loop —
  hermes `on_processing_start`/`on_processing_complete`/`_handle_reaction_event`)
- interactive card actions ported (hermes `send_exec_approval` / `send_update_prompt` +
  `_on_card_action_trigger`): exec-approval cards (orange header, fenced 3000-char command preview, ✅
  Allow Once / ✅ Session / ✅ Always / ❌ Deny with session tiers hidden under smart deny) via the
  `send_exec_approval` contract, update-prompt cards (✓ Yes / ✗ No), webhook-mode card callbacks
  resolve approvals through the approval gateway under operator authorization (allowlist ∪ pairing,
  callback-chat match) and return the inline resolved card (`P2CardActionTriggerResponse` JSON),
  update-prompt answers written atomically to `.update_response`
- adaptations: button values carry stateless `approve:<session>:<decision>` payloads instead of
  hermes in-memory approval state, WebSocket mode drops CARD frames exactly like the lark SDK (card
  actions are webhook-only), non-approval card actions route as synthetic COMMAND events (`/card <tag>
  <json>` through the allowlist∪pairing intake gate with a 15-min token dedup window
- the dispatch runs detached so the webhook answers `{}` at once
- action-value JSON keys sort rather than keeping hermes insertion order)
- read receipts (`im.message.message_read_v1`) explicitly ignored on both transports (hermes `_on_message_read_event`)
- `[messaging.feishu]` (`connection_mode` websocket|webhook, `domain` feishu|lark) or
  FEISHU_APP_ID/FEISHU_APP_SECRET/FEISHU_VERIFICATION_TOKEN/FEISHU_ENCRYPT_KEY/FEISHU_CONNECTION_MODE/FEISHU_DOMAIN).

**Twilio SMS** — hermes `platforms.sms` plugin — the standalone aiohttp webhook server on
  `SMS_WEBHOOK_PORT` is replaced by a gateway-mounted route

- : `/webhooks/twilio` accepts form-encoded Twilio callbacks with `X-Twilio-Signature` HMAC-SHA1
  validation (url + sorted key/value concatenation, base64 digest, default-port variant fallback per
  the Twilio docs, fail-closed without `SMS_WEBHOOK_URL` unless `SMS_INSECURE_NO_SIGNATURE=true`), 64
  KiB body cap, own-number echo suppression, allowlist∪pairing intake
  (`SMS_ALLOWED_USERS`/`SMS_ALLOW_ALL_USERS`, pairing codes delivered by SMS), and markdown-stripped
  1600-char chunked replies via the Twilio Messages REST API (HTTP Basic auth)
- MMS media handling not ported
- `[messaging.sms]` or TWILIO_ACCOUNT_SID/TWILIO_AUTH_TOKEN/TWILIO_PHONE_NUMBER/SMS_WEBHOOK_URL.

**Microsoft Teams** — hermes `platforms.teams` plugin — the `microsoft-teams-apps` Python SDK is
  replaced by raw Bot Framework protocol

- : `/webhooks/teams` accepts `message` activities with activity-id dedup (300 s window), `<at>`
  mention HTML stripping, conversation-type mapping (personal/groupChat/channel), sender gating
  (aadObjectId or id) via allowlist ∪ pairing, attachment intake with the hermes skip rules (html
  mirrors, adaptive/hero cards) — file-consent `downloadUrl` payloads, `image/*` and
  bearer-authenticated `contentUrl` downloads into the media cache — and replies via OAuth2
  client-credentials tokens (`login.microsoftonline.com/<tenant>/oauth2/v2.0/token`,
  `https://api.botframework.com/.default` scope, cached) posted to
  `{serviceUrl}v3/conversations/<conv>/activities` markdown activities
- the serviceUrl is validated against the hermes Bot Framework host allowlist (SSRF guard) and
  conversation ids against the Bot Framework charset
- exec approvals render as AdaptiveCard v1.4 prompts with Action.Execute Allow Once/Allow
  Session/Always Allow/Deny buttons, `adaptiveCard/action` invoke taps resolve the session's blocking
  approval under a default-deny allowlist gate (empty `TEAMS_ALLOWED_USERS` rejects every click) and
  reply with a replacement card echoing the decision, and `/approve [all] [session|always]` / `/deny
  [all]` chat commands work on every platform (hermes slash-command semantics)
- the summary-writer paths are not ported
- `[messaging.teams]` or TEAMS_CLIENT_ID/TEAMS_CLIENT_SECRET/TEAMS_TENANT_ID/TEAMS_ALLOWED_USERS.

**LINE** — hermes `platforms.line` plugin — the dedicated aiohttp server is replaced by a
  gateway-mounted route

- : `/webhooks/line` verifies `X-Line-Signature` (base64 HMAC-SHA256 over the raw body keyed by the
  channel secret, constant-time), 1 MiB body cap, webhook-event-id dedup (LRU-1000), source resolution
  (user/group/room) with hermes' three-allowlist gate
  (`LINE_ALLOWED_USERS`/`LINE_ALLOWED_GROUPS`/`LINE_ALLOWED_ROOMS`, `LINE_ALLOW_ALL_USERS` dev escape)
  ∪ pairing, text plus image/video/audio/file content downloads (`api-data.line.me`) into the media
  cache (stickers/locations degrade to text notes), loading-indicator posts, and replies that prefer
  the single-use reply token with metered Push fallback — markdown stripped with URLs preserved
  (`[label](url)` → `label (url)`), 4500-char bubbles, 5-message call budget with ellipsis truncation,
  the slow-LLM postback-button state machine (hermes PR #18153: when the agent is still running past
  `slow_response_threshold` — default 45 s, 0 disables — the reply token is spent on a Template
  Buttons bubble whose postback action carries a request id
- the finished reply lands in an in-memory PENDING → READY/ERROR → DELIVERED cache (1 h TTL, 24 h
  for PENDING) and is delivered on tap, repeat taps get "Already replied ✅", system busy-acks bypass
  the cache), and outbound `MEDIA:` tags — LINE only accepts publicly reachable HTTPS media URLs, so
  local files are registered under random 30-minute tokens and served by the gateway at
  `/line/media/<token>/<filename>` (allowed-roots defence, 410 after expiry,
  `public_url`/`LINE_PUBLIC_URL` required)
- images cap at 10 MB, audio/video at 200 MB, video messages get a 1×1 fallback PNG preview
- `[messaging.line]` (`public_url`, `slow_response_threshold`, button-copy overrides) or
  LINE_CHANNEL_ACCESS_TOKEN/LINE_CHANNEL_SECRET/LINE_PUBLIC_URL/LINE_SLOW_RESPONSE_THRESHOLD/LINE_PENDING_TEXT/LINE_BUTTON_LABEL/LINE_DELIVERED_TEXT/LINE_INTERRUPTED_TEXT.

**Google Chat** — hermes `platforms.google_chat` plugin, both inbound transports

- : `/webhooks/googlechat` verifies the Google ID token in `Authorization: Bearer` via Google's
  `tokeninfo` endpoint (audience + service-account email match — hermes' local google-auth cert
  verification is replaced by the online probe), routes only `MESSAGE` envelopes (BOT senders skipped,
  message-name dedup 300 s window), prefers `argumentText` over `text`, maps DIRECT_MESSAGE spaces to
  DMs, gates senders by email/user-resource allowlist ∪ pairing, keeps replies in the inbound thread,
  and sends through the Chat REST API (`POST /v1/{space}/messages`, 4000-char chunks) with
  service-account RS256 JWT-bearer tokens (cached, `chat.bot` scope)
- native file attachments ride the per-user OAuth `media.upload` flow (`oauth.py` port,
  `src/google_chat_oauth.rs`): Google hard-rejects service accounts on media.upload, so each user
  grants the `chat.messages.create` scope once via the in-chat `/setup-files` command (PKCE
  installed-app flow with the `http://localhost:1` redirect expected to fail — the user pastes the
  failed URL or code back
- subcommands: bare status, `start`, `revoke`, `<code-or-url>` exchange
- per-sender-email token slots under `<home>/google_chat_user_tokens/` plus a legacy single-user
  slot), host-side CLI `ulnclaw google-chat-oauth client-secret|auth-url|auth-code|revoke|check`
- `MEDIA:` replies upload as the chat's most recent sender (multipart `attachments:upload` →
  `messages.create` with the returned `attachmentDataRef`, both user-token-authed,
  `messageReplyOption` for threads), 401/403 invalidates the cached token and falls back to a
  setup-instructions text notice carrying the host path
- typing-card patching ported: a "ulnclaw is thinking…" marker card posts before the turn
  (`typing_status_text` override, hermes `send_typing`) and the reply PATCHes it in-place via
  `messages.patch?updateMask=text` — first chunk patches, overflow chunks create, patch failure falls
  back to creating, empty replies patch "(interrupted)", the consumed-sentinel slot guards against
  duplicate markers (hermes `_typing_messages`/`_TYPING_CONSUMED_SENTINEL` semantics)
- inbound also rides Pub/Sub when `pubsub_subscription` (`GOOGLE_CHAT_PUBSUB_SUBSCRIPTION`) is set —
  REST pull against the subscription with a `pubsub`-scoped service-account token (same semantics as
  hermes' gRPC streaming-pull, no gRPC dependency), CloudEvents envelopes (`ce-type` attribute) with
  all three hermes payload formats (Workspace Add-ons `chat.messagePayload`, native Chat API
  `{type:MESSAGE}`, relay flat), membership/card events acked with logging, at-least-once dedup via
  the same message-name window, unconditional ack (malformed deliveries included), and the hermes
  supervisor (full-jitter exponential backoff, 2 s base / 120 s cap, fatal after 10 attempts or on
  auth/permission errors)
- `[messaging.google_chat]` or
  GOOGLE_CHAT_SERVICE_ACCOUNT_FILE/GOOGLE_CHAT_HTTP_EVENTS_AUDIENCE/GOOGLE_CHAT_HTTP_EVENTS_SERVICE_ACCOUNT/GOOGLE_CHAT_PUBSUB_SUBSCRIPTION.

**Buzz**
- Block's Nostr-based human+agent collaboration platform
- hermes `platforms.buzz` plugin

- : inbound transport selection (hermes `transport`): `auto` (default) prefers the
  NIP-42-authenticated WebSocket subscription and falls back to CLI polling when it cannot
  authenticate within 20 s, `websocket` requires it, `poll` keeps the CLI loop. The WS path answers
  the relay's `AUTH` challenge with a signed kind-22242 event (`src/nostr_auth.rs` — dependency-light
  BIP-340 schnorr + nsec bech32 decode, hermes `nostr_auth.py` port
- key from `BUZZ_PRIVATE_KEY` or a `~/.config/buzz/*credentials*.json` file, optional
  `BUZZ_AUTH_TAG` NIP-OA attestation), subscribes per channel (`kinds=[9]`, `#h`, `since` resume) plus
  the kind-44100 membership feed, routes events through the same dedup/mention/allowlist machinery as
  polling, and reconnects with 1→30 s backoff
- the own pubkey is derived from the key when `BUZZ_PUBKEY` is unset. The poll path shells out to
  the local `buzz` CLI (`buzz messages get --channel <id> --limit 50 [--since <ts>]`) every 4 s per
  configured channel (`dm:` prefix marks DM handles). Both keep only Nostr kind-9 chat events, dedup
  by event id (500-entry cap), suppress own-pubkey echoes, require-mention gate channels (full pubkey,
  ≥6-char pubkey prefix, or `@mention`
- the mention is stripped before dispatch) while DMs always pass, filter senders by an optional
  pubkey allowlist, and reply via `buzz messages send --channel <id> --content -` with the body on
  stdin. a 👀 "seen" tapback is added after every dispatched message via buzz-cli `reactions add`
  (best-effort, hermes `send_reaction`)
- DM discovery mirrors hermes (`_discover_dms` + `_seed_channel` + the issue #68871 classifier):
  startup seeds every watched conversation's high-water mark from its newest events (history is marked
  seen, never replayed
- transiently unreadable channels fall back to "now") and discovers existing DMs via `buzz dms list`
  with a `buzz channels list` fallback for hosted relays that return `[]`
- mid-run, kind-44100 membership events p-tagged to us advance the `_membership_since` subscribe
  cursor and rediscover conversations, subscribing the live WS to each new one as `hermes-buzz-dm-<n>`
  (fresh DMs dispatch from their beginning), while the poll transport rediscovers every fifth sweep
  (`_DM_DISCOVERY_EVERY=5`)
- conversations leaked in via `channels list` named "DM" with an empty description start as groups
  and latch to chat_type=dm on the first structural p-tagged un-mentioned message (`_maybe_latch_dm`
- explicitly configured channels and real named/described channels never reclassify)
- group management is not ported
- `[messaging.buzz]` or
  BUZZ_CHANNELS/BUZZ_CLI/BUZZ_PUBKEY/BUZZ_POLL_INTERVAL_MS/BUZZ_RELAY_URL/BUZZ_PRIVATE_KEY/BUZZ_CREDENTIALS_FILE/BUZZ_TRANSPORT/BUZZ_AUTH_TAG.

**Photon**
- iMessage via the Photon Spectrum sidecar
- hermes `platforms.photon` plugin, sidecar-client transport — ulnclaw does not spawn or bundle the Node sidecar
- run it alongside and point `[messaging.photon] sidecar_url` at it

- : waits for the sidecar's `/healthz`, consumes the `GET /inbound` NDJSON stream (messageId dedup,
  48 h window) with typed content intake (`space`/`sender`/`content` — `text` | `richlink` | `group` |
  `attachment` | `voice` nodes, attachments cache inline base64 `data` with a flat-shape local-path
  fallback), suppresses own-send echoes (1000-id ledger), renders rich links as title/summary/URL and
  suppresses iMessage rich-link preview art (`.pluginpayloadattachment` marker within 30 s of the
  link), gates senders by allowlist ∪ pairing, gates group chats on a `require_mention` wake word
  (configurable regex patterns, hermes `hermes` wake-word default, stripped before dispatch), sends
  `/typing` indicators (5 s per-chat cooldown), and posts 8000-char capped replies to `/send`
  (`{spaceId, text}` + markdown `format` hint, `PHOTON_MARKDOWN` kill-switch) — URL-only replies ride
  `/send-richlink` with plain-text fallback
- reactions (iMessage tapbacks) are ported: opt-in lifecycle tapbacks via `PHOTON_REACTIONS` (👀
  while processing, swapped remove-then-add for 👍 on success / 👎 on agent error), sidecar `/react` +
  `/unreact`, and inbound tapbacks route to the agent as `reaction:added:<emoji>` synthetic text only
  when they target a bot-sent message (`targetDirection: outbound` or the target is in the own-send
  ledger) — tapbacks never trigger the mention gate (hermes
  `on_processing_start`/`_add_reaction`/reaction routing)
- the agent-facing `send_message action="react"` surface is ported in P259 (the `send_message`
  tool's react/unreact actions reach Telegram via `setMessageReaction`, with the most-recent-message
  fallback recalled from the channel directory)
- `[messaging.photon]` or
  PHOTON_SIDECAR_URL/PHOTON_SIDECAR_TOKEN/PHOTON_ALLOWED_USERS/PHOTON_REQUIRE_MENTION/PHOTON_MENTION_PATTERNS/PHOTON_MARKDOWN.

**Raft** — hermes `platforms.raft` plugin, both halves

- : `/webhooks/raft/wake` requires the `x-raft-bridge-token` header (auto-generated 32-byte hex
  token when unset, shared with the spawned bridge via `RAFT_CHANNEL_TOKEN`), caps bodies at 16 KiB,
  validates `raft-activity.v1` events with safe-scalar extraction, dispatches them as
  `raft:<sessionId>` chats, and returns the agent reply in the JSON response body
- on gateway startup the adapter spawns the bridge itself (hermes `_spawn_bridge`: `raft --profile
  $RAFT_PROFILE agent bridge --wake-adapter wake-channel --wake-channel-endpoint <gateway wake url>`,
  stdin devnull, SIGTERM + 5 s grace + SIGKILL on shutdown
- missing `raft` binary or unset `RAFT_PROFILE` degrade to wake-only mode)
- `[messaging.raft]` or RAFT_BRIDGE_TOKEN/RAFT_PROFILE.

**A2A** — hermes `platforms.a2a` plugin — Agent2Agent v1.0 server surface

- : publishes the Agent Card at `GET /.well-known/agent-card.json` (plus legacy
  `/.well-known/agent.json`) and speaks JSON-RPC 2.0 at `POST /a2a` — `message/send` dispatches the
  inbound message parts through the normal agent pipeline and returns a completed task with the reply
  as an artifact (in-memory ledger capped at 100 tasks), `tasks/get`/`tasks/list`/`tasks/cancel`
  inspect and manage it
- `message/stream` (SSE) and push-notification config answer with capability-false errors
- optional bearer gate (`A2A_TOKEN`), 1 MiB request cap
- hermes' dedicated `A2A_PORT` HTTP server is replaced by the gateway router
- `[messaging.a2a]` or A2A_AGENT_NAME/A2A_AGENT_DESCRIPTION/A2A_PUBLIC_URL/A2A_TOKEN. Inbound voice
  notes enter the audio STT pipeline (see the Speech-to-text row): transcripts are echoed back as 🎙️
  messages and enrich the turn. Interactive clarify prompts render as native WhatsApp buttons/list
  sheets (see the Interactive clarify row)
- Telegram gets native inline keyboards (one numbered button per choice plus an ✏️ Other (type
  answer) row, `cl:<id>:<idx|other>` callback payloads resolved in the getUpdates loop — hermes
  `send_clarify` layout)
- Discord gets native buttons (orange ❓ embed + self-contained content mirror, numbered button
  labels truncated at word boundaries within the 80 UTF-16 cap, `clarify:<id>:<idx|other>` custom ids,
  24-choice cap, INTERACTION_CREATE routing with ephemeral auth/stale notices and UPDATE_MESSAGE
  resolution edits, 300 s visual expiry disabling the buttons — hermes ClarifyChoiceView)
- Slack gets Block Kit buttons (mrkdwn-escaped ❓ section, per-choice buttons with
  `hermes_clarify_choice_<idx>` action ids packing `<id>|<idx>` values plus a ✏️ Other… row,
  block_actions routing over `interactive` envelopes with the double-click ledger, chat.update outcome
  rewrites — hermes `_handle_clarify_action`)
- Yuanbao media/sticker channels are ported (see the Yuanbao clause)
- P225: Telegram exec approvals render as inline keyboards — ✅ Allow Once / Session / Always + ❌
  Deny paired into 2-per-row buttons, `ea:<choice>:<id>` callback data mapped to the session through
  an approval-id registry (hermes `_approval_state`), resolve-first-render-after so stale taps say ⌛
  Approval expired instead of claiming success (hermes #63501), same allowlist∪pairing auth union as
  clarify taps
- the `/approve` text flow stays the fallback. P259: every inbound event records into the channel
  directory (`src/channel_directory.rs`), powering the agent-facing `send_message` tool (cross-channel
  sends, target listing, emoji reactions — see the `send_message` row). P337 added the
  dashboard-facing `/api/messaging/platforms` wire surface: `GET` catalog of all 26 platforms with
  per-platform enabled/configured posture, redacted env rows and a disabled→not_configured→connected
  state ladder
- `PUT /api/messaging/platforms/:id` toggles `[messaging.<id>].enabled` and sets/clears the platform's env keys in `.env`
- `POST /api/messaging/platforms/:id/test` answers an `{ok, state, message}` posture probe — lean
  hermes `/api/messaging/platforms` parity


<a id="interactive-clarify-tools-clarify-gateway-py-whatsapp-interactive"></a>

#### Interactive clarify (`tools/clarify_gateway.py` + WhatsApp interactive) — ✅ core

- `src/clarify_gateway.rs` + messaging integration — the `clarify` tool works in messaging sessions:
  prompts register in a bounded gateway registry (hermes state cap), render on the platform (WhatsApp
  `interactive.type=button` ≤3 choices / `type=list` 4+ with the ✏️ Other row, numeric labels with
  full choice text in the body, 20/24/72-char caps, `cl:<id>:<idx|other>` button ids — hermes
  `send_clarify` layout
- Telegram renders inline keyboards (one numbered button per choice + ✏️ Other (type answer) row,
  `cl:<id>:<idx|other>` callback_data within the 64-byte cap — hermes layout
- taps answer via answerCallbackQuery and edit the prompt with the resolution — numeric taps resolve
  index→choice text, Other flips to text-capture, unauthorized taps get ⛔, expired/evicted prompts get
  the ⚠️ notice)
- Discord renders an embed + button view (hermes ClarifyChoiceView layout — interaction-callback
  resolution, Other flips to text-capture, 300 s visual expiry skips prompts awaiting typed answers
- Slack renders Block Kit buttons (hermes action-id/value layout, Other flips to text-capture,
  outcomes rewrite the prompt via chat.update, atomic double-click guard)
- the Baileys bridge adapter (`[messaging.whatsapp]`) renders 2–12-choice clarifies as native
  single-select polls instead — hermes Baileys `send_clarify`, picked option arrives as plain text —
  with numbered-text fallback), and block the turn until resolved. Taps route through
  `_dispatch_interactive_reply` semantics: index→choice-text resolution, Other flips to text-capture
  (`mark_awaiting_text` + ✏️ prompt), unauthorized taps are claimed without dispatch, stale ids fall
  back to text dispatch with the button title. The next plain message in a session resolves an
  awaiting clarify instead of starting a turn (hermes `_maybe_intercept_clarify_text`). Remaining
  platforms receive numbered-text prompts
- `appr:`/`sc:` prefixes (gateway approvals / slash-confirm) fall through to text like hermes' no-waiter path

<a id="speech-to-text-tools-transcription-tools-py-gateway-stt-pipeline"></a>

#### Speech-to-text (`tools/transcription_tools.py` + gateway STT pipeline) — ✅ core

- `src/stt.rs` — hermes' audio STT pipeline: `[stt]` config
  (enabled/echo_transcripts/provider/language + per-provider blocks with hermes defaults), built-in
  providers `local_command` (command escape hatch, `ULNCLAW_LOCAL_STT_COMMAND`), `groq`
  (whisper-large-v3-turbo), `openai` (whisper-1), `mistral` (Voxtral), `xai`, `elevenlabs` (Scribe),
  `deepinfra` (live-catalog model discovery), all OpenAI-compatible multipart uploads
- custom command providers via `[stt.providers.<name>]` + legacy top-level blocks with the built-ins-always-win invariant
- gateway voice notes (audio/* attachments) are transcribed before the turn with hermes semantics —
  local-command fallback on provider failure, empty-transcript sentinel (#41603), neutral failure
  marker kept out of the prompt, `🎙️ "<transcript>"` echoes (stt.echo_transcripts), WAV/ffprobe
  duration notes when STT is disabled
- `transcribe_audio` agent tool (opt-in `stt` toolset) with model/language overrides. P327 exposes
  the pipeline to the desktop as `POST /api/audio/transcribe` (base64 data-URL voice note in,
  transcript out
- 25 MiB cap, mime validation, provider-readiness gate — hermes `/api/audio/transcribe` parity
- TTS is ported as `POST /api/audio/speak` over the `[tts]` openai/elevenlabs providers — P344) with
  a composer mic button and a 🔊 read-aloud action on assistant replies. Known difference: hermes'
  default `local` provider (faster-whisper, Python) cannot embed in the static binary —
  `stt.local.command` or a cloud provider takes its place

<a id="oauth-login-skill-sync-hermes-cli-portal-cli-py-tools-skills-sync-client-py"></a>

#### OAuth login + skill sync (`hermes_cli/portal_cli.py`, `tools/skills_sync_client.py`) — ✅ core

`src/oauth.rs` + `src/skills_sync.rs`: service-agnostic port of hermes' portal auth + Skill Sync.
`ulnclaw auth login` runs the RFC 8628 Device Authorization Grant against any configured `[oauth]`
provider (device_authorization_url/token_url/client_id/scopes) with authorization_pending/slow_down
handling, token storage at `oauth_tokens.json` (0600), refresh-token grant,
`status`/`refresh`/`logout`/`open`. `ulnclaw sync status|pull|push|now|enable|disable|device` keeps
hermes' exact UX: opt-in skill sync with stable device id + device label, INERT gate reporting when
no `[sync] base_url` is set, pull never clobbers local skills. Transport is generic: HTTP(S) REST
with bearer auth (OAuth token or `[sync] api_key`) or a shared directory for offline/NAS sync.
Nous-Portal-specific subscription features and org proposal approval flows are not ported

<a id="oauth-upstream-proxy-hermes-cli-proxy"></a>

#### OAuth upstream proxy (`hermes_cli/proxy/`) — ✅ core

- `src/proxy_cmd.rs` + `ulnclaw proxy start|status|providers` (P179): local OpenAI-compatible proxy
  that lets external apps ride the user's stored OAuth subscription instead of a static API key.
  Listens on `127.0.0.1:8645` (`[proxy] host/port`), mounts `/v1/*` with a path allowlist
  (`/chat/completions`, `/completions`, `/embeddings`, `/models`, `/responses`), discards the client's
  bearer, attaches the `oauth_tokens.json` access token (auto-refresh 60 s before expiry, persisted),
  one-shot force-refresh retry on upstream 401/429, 10 MB request cap (hermes `MAX_REQUEST_BYTES`),
  hop-by-hop header stripping, identity-preserving byte stream (SSE preserved), `/health` probe.
  Divergence: the adapter is provider-agnostic (`[proxy] upstream_url`) instead of hermes' Nous-Portal
  subscription resolver and xAI credential-pool adapter (the pool store itself is now ported lean —
  see the credential-pool row)
- the iron-proxy egress firewall IS ported — see the dedicated iron-proxy row

<a id="desktop-gui-apps-desktop-electron"></a>

#### Desktop GUI (`apps/desktop` Electron) — ✅ core

- `desktop-electron/` — **ulnclaw desktop**: since v0.7.0 ulnclaw ships a faithful port of hermes'
  Electron desktop (v2026.8.3) itself, retiring the earlier Tauri 2 shell: React 19 + Vite renderer
  (sixteen views + command palette + themes/fonts + language picker), xterm.js terminal panes, file
  tree + git-review sidebar, sessions browser, live turn streaming over a JSON-RPC WebSocket +
  HTTP/SSE against the ulnclaw gateway, and an Electron main process that spawns/supervises the
  bundled statically linked gateway binary (`ULNCLAW_DESKTOP=1`, health probes with capped respawn,
  boot diagnostics, tray + native menus, `ulnclaw://` deep links, single-instance handoff,
  window-state persistence). The gateway's JSON-RPC WS surface answers the desktop's full method
  catalog — session lifecycle (`session.create` mints the id with hermes' lazy-row contract and
  persists per-session model/cwd/title overrides that WS turns enforce via the model lock
- resume/close/title/interrupt), prompt.submit with queued/interrupted flags,
  approval/clarify/sudo/secret responses, config get/set, env/MCP reload, model options, slash
  completion + exec, message reactions and wake-word surface, session analytics and lifecycle tools
  (`session.usage` / `session.context_breakdown` / `session.status` / `session.save` transcript export
  / `session.branch` lineage forks / `session.compress` LLM summarisation mirroring the `/compress`
  slash / `session.redirect` busy-turn queueing / `session.activate` / `session.active_list` liveness
  snapshot, plus the `session.create` seed `messages` + `parent_session_id` branch contract), the pet
  sprite surface (`pet.info` spritesheet payload / `pet.info.meta` / `pet.gallery` with petdex
  manifest merge / `pet.thumb` / `pet.scale` / `pet.disable` / `pet.export`), portal billing fail-open
  stubs (`billing.state` / `subscription.state` answer logged-out
- charge / step-up / auto-reload / subscription mutations answer the typed `unavailable` refusal
  envelope), `browser.manage` status/connect/disconnect over the CDP override, `command.dispatch`
  fallback routing, honest-unsupported `handoff.*`, `preview.restart` detached dev-server restarts,
  `setup.status` / `setup.runtime_check` onboarding probes, and the streaming-TTS WebSocket. Since
  v0.7.1 the packaged shell also self-updates silently (electron-updater against the GitHub release
  channel
- installs on quit — an ulnclaw addition, hermes desktop has no shell auto-update)

<a id="sessions-browse-hermes-cli-sessions-cmd-py-browse-curses-picker"></a>

#### Sessions browse (`hermes_cli/sessions_cmd.py browse` + curses picker) — ✅ core

- `ulnclaw sessions browse [--source S] [--limit N]`: raw-mode TUI on a TTY (crossterm port of the
  curses picker — alternate screen, ↑/↓/PgUp/PgDn/Home/End navigation with scrolling, live
  type-to-filter with backspace, green `▶` selection highlight, Enter selects, bare `q` quits while no
  filter is active, Esc clears the filter first and quits on the second press, single-step ↑/↓ wrap
  around the list, a dim column-header strip (Title/Preview · Active · Src · ID) and a bottom footer
  with the cursor position + filtered-from count, "terminal too small" guard, Enter delivered as LF
  (`Ctrl+J`) also accepted) with a plain numbered-stdin fallback for pipes/CI
- newest-activity-first rows (title → first-user-message preview fallback, relative time, source,
  truncated id), substring filter over title/preview/id/source, `tool`-source sessions excluded unless
  `--source` is given (hermes semantics)
- P165 added the owning-project badge: rows whose cwd falls under a project folder render a `⌂ slug`
  prefix and the live filter also matches project slugs (both pickers, best-effort against
  `projects.db`)
- selection relaunches the current binary with `--resume <id>` (hermes `relaunch`)
- store query `list_sessions_for_browse` returns the picker rows in one SQL statement
- P177 upgraded the raw-mode TUI interactions beyond the hermes picker: a right-hand details pane
  (terminal ≥ 90 cols) shows the highlighted session's full title, complete id, source, owning
  project, cwd, last-active time (relative + absolute local time), and the wrapped first-user-message
  preview
- `Tab` cycles a per-source filter (all → each source present → all, rendered as a green `[source:
  …]` header chip) and `F2` toggles recent-first ↔ alphabetical (untitled-last) sort (`[sort: A-Z]`
  chip)
- the char-aware text-layout helpers live in `src/tui_text.rs` with unit tests
- P224 raw-mode interaction upgrades: `F1` dismissible keybinding help overlay, `F5` reloads the
  session list from disk mid-browse (fresh store connection
- a live gateway may create sessions), `F8` archives the highlighted session after an inline `y`
  confirmation (hermes `set_session_archived`, auto-reload + transient green footer notice),
  `Shift+Tab` cycles the source filter backwards, and PgUp/PgDn/Home/End/Ctrl+C were already supported
- P340 raw-mode interaction upgrades: `/` opens a transcript-search prompt — Enter runs the FTS5
  message-body search (LIKE fallback, 200 hits) through a fresh store connection, narrows the list to
  matching sessions and shows the match snippet in the details pane, Esc clears the results
- `F9` deletes the highlighted session forever after an inline `y` confirmation (hermes
  `delete_session`, auto-reload + footer notice)

<a id="session-resume-live-session-continuity-cli-py-resume-continue"></a>

#### Session resume & live-session continuity (`cli.py --resume/--continue`) — ✅ core

- Global `-r/--resume <id-or-prefix>` + `-c/--continue` flags on `chat` and `run`: the whole REPL
  conversation lives in ONE session row (previously every turn created a fresh row)
- resume seeds the REPL history from `load_messages` (system rows dropped), prints `Resuming
  session: <id> (title)`, and each turn runs through `run_with_session` against the same id
- `/new` rotates to a fresh session key + resets the per-session goal manager
- `latest_session_id` picks the `--continue` target by last activity (archived skipped)
- all id-taking `sessions` actions (`show`/`export`/`recap`/`delete`/`rename`) accept unique
  prefixes via `resolve_session_id`
- P232 ported hermes' `-c <session-name>` title lookup: `-c/--continue` now takes an optional value
  — bare `-c` keeps the most-recent-session behavior, while `-c NAME` resolves by exact id / unique
  prefix first, then title (`resolve_session_by_title` — numbered `title #N` lineage variants prefer
  the newest, archived skipped, LIKE wildcards escaped), and projects the result forward through
  compression chains (`compression_tip`) so resumes land on the live tip instead of a stale compressed
  parent


<a id="feature-parity"></a>

## Feature parity

| hermes feature | ulnclaw | Notes |
|---|---|---|
| Agent loop with tool calling | ✅ | iteration budget, usage accounting, step callbacks |
| SQLite state store (`hermes_state.py`) | ✅ | sessions/messages/system_prompts/state_meta/async_delegations schema, FTS5 with LIKE fallback, lineage (parent sessions) |
| Session recovery (`session_recovery.py`) | ✅ core | `ulnclaw sessions recover <db> [--out FILE]`: offline, non-destructive — source copied (with WAL/SHM/journal sidecars) to a disposable dir, canonical rows copied into a fresh current-schema db with rowid salvage over damaged tables, orphaned messages get reconstructed session rows, FTS rebuilt, integrity-checked, JSON report; never repairs in place or overwrites the active db |
| Environment probe (`tools/env_probe.py`) | ✅ | one deterministic Python-toolchain line in the system prompt when the terminal backend is local: python3/python versions, pip-module availability, `pip`↔`python3` version mismatch, PEP 668 externally-managed marker (uv neutralizes it); silent on healthy machines; process-wide cache built by a single background worker, callers wait ≤10s then fail open; remote backends (docker/ssh) skip the probe; `[agent] environment_probe` toggle (default true) |
| Context compression (`conversation_compression.py`) | ✅ | budget-triggered, middle-turn summarization via secondary model call, keeps system prompt + first user message + recent tail; summary call honors `[auxiliary.compression]` … [details](#context-compression-conversation-compression-py) |
| Streaming think-block scrubber (`agent/think_scrubber.py`) | ✅ | `think_scrubber.rs`: stateful suppression of `<think>`/`<thinking>`/`<reasoning>`/`<thought>`/`<REASONING_SCRATCHPAD>` blocks in streamed deltas — every content delta is fed through the state machine in `call_with()` (open tags survive chunk boundaries; unterminated opens are boundary-gated), the held-back partial-tag tail is flushed at stream end, and the non-stream path runs the complete-string `strip_think_blocks`; closed pairs are always suppressed, opens only at block boundaries, so prose that merely mentions a tag is never over-stripped |
| Session title generator (`agent/title_generator.py`) | ✅ | `title_generator.rs`: fire-and-forget auto-titling after the first exchange (background task, never adds latency to the reply) — first-2-user-turns guard, existing-title guard, `[auxiliary.title_generation]` routing (`language` pin, `enabled` kill switch with `is_truthy_value` semantics, default true); 500-char snippets, reasoning-block scrub on the answer, quote/"Title:"-prefix/first-line/80-char cleanup; atomic `set_auto_title_if_empty` persistence so a manual title set while generation was in flight wins; the optional title callback and portal/accounting tags are not ported (no live UI surface) |
| Persistent goals — the Ralph loop (`hermes_cli/goals.py`) | ✅ core | `goals.rs`: standing goals that survive across turns — after every assistant turn a `goal_judge` auxiliary model decides done/continue/wait; continuation prompts are fed back as … [details](#persistent-goals-the-ralph-loop-hermes-cli-goals-py) |
| Timezone-aware clock (`hermes_time.py`) | ✅ | `hermes_time.rs`: IANA timezone resolution `ULNCLAW_TIMEZONE` → `HERMES_TIMEZONE` → config `timezone` → server-local (invalid names warn + fall back, cached with `reset_cache()`); system prompt gets a date-only "Conversation started" line + Model/Provider (byte-stable all day for prefix-cache, hermes PR #20451); compression summaries carry a `Current date` anchor |
| Approval system (`approval.py`) | ✅ | command normalization (backslash-joins, `${IFS}`, comment strip), hardline floor (block), recoverable-costly (confirm); REPL y/N prompt; gateway run approvals ( … [details](#approval-system-approval-py) |
| Threat-pattern scanning (`threat_patterns.py`) | ✅ core | advisory injection scan for tool results re-entering context |
| Toolsets (`toolsets.py`) | ✅ | all 33 toolset definitions incl. composition (`includes`), `coding` default |
| Tool registry (`registry.py`) | ✅ | check_fn gating, toolset grouping, max result size truncation |
| Provider abstraction (`runtime_provider.py`) | ✅ | OpenAI-compatible (OpenAI/OpenRouter/DashScope/Ollama/llama.cpp), native Anthropic Messages transport (`anthropic_messages`: system param, tool_use/tool_result blocks, SSE streaming, max_tokens ceilings, OAuth bearer), keyless local providers |
| Provider fallback chain (`fallback_providers`, `try_activate_fallback`) | ✅ core | `[model] fallbacks = ["provider:model", ...]`: on a failed model call the chain advances (lazy per-entry clients, credential fallback to the main key), the activated fallback stays live for the turn, and the next turn restores the primary (hermes `restore_primary_runtime`); delegated/cron children inherit the specs |
| Auxiliary model routing (`auxiliary_client.py`) | ✅ core | `[auxiliary.<task>]` per-task provider/model/base_url/api_key/key_env overrides (`compression`, `vision`, `title_generation`); `"auto"`/blank inherits the main runtime; main client reused when nothing is overridden |
| models.dev catalog (`agent/models_dev.py`) | ✅ core | `models_dev.rs`: fetches `https://models.dev/api.json` with a three-tier cache — in-memory (1h TTL, stale served immediately while a background thread refreshes) → disk … [details](#models-dev-catalog-agent-models-dev-py) |
| Config (`config.yaml`) | ✅ | `config.toml` + `.env` file, profiles, env precedence |
| Skills system | ✅ | discovery, frontmatter, linked files, `/skill-name` invocation scaffolding (hermes `build_skill_invocation_message` port: activation note + skill body + skill-directory/supporting-file hints + user-instruction marker, `skill_usage` bump; recognized by `sessions retitle-skills`) |
| Memory system | ✅ | MEMORY.md/USER.md with prompt injection |
| Cron scheduler | ✅ | job store + schedule parsing + poll loop (`cron::run_scheduler`) |
| MCP client (`mcp_tool.py`) | ✅ core | stdio JSON-RPC: initialize/tools/list/tools/call; `[[mcp.servers]]` config; tools registered as `mcp__<server>__<tool>`; OSV malware preflight for npx/uvx/pipx launches … [details](#mcp-client-mcp-tool-py) |
| MCP channel bridge (`mcp_serve.py`) | ✅ core | P260 full port of hermes `mcp_serve.py` (`src/mcp_serve.rs`, `ulnclaw mcp serve [--verbose]`): stdio JSON-RPC 2.0 MCP server exposing messaging conversations to any MCP client … [details](#mcp-channel-bridge-mcp-serve-py) |
| ACP adapter (`acp_adapter/`) | ✅ core | P261 port of hermes `acp_adapter/` (`src/acp_adapter.rs`, `ulnclaw acp [--verbose]`): Agent Client Protocol stdio server for editors (Zed). Bidirectional newline-delimited … [details](#acp-adapter-acp-adapter) |
| Batch runner (`batch_runner.py`) | ✅ core | P262 full port of hermes `batch_runner.py` (`src/batch_runner.rs` … [details](#batch-runner-batch-runner-py) |
| Send CLI (`hermes_cli/send_cmd.py`) | ✅ core | P263 full port of hermes `hermes send` (`ulnclaw send`): script/cron/CI message delivery with no LLM and no agent loop — `--to platform[:chat[:thread]]` target grammar, message body from positional / `--file PATH` / `-` / piped stdin (TTY-aware), `--subject` header prepend, `--list [platform]` channel-directory rendering merged with configured-but-undiscovered platforms, `--json` raw result, `--quiet` exit-code-only mode, hermes exit-code contract (0 ok / 1 delivery failure / 2 usage error). Rides the P259 `send_message` pipeline including the standalone Telegram/Discord/Slack REST path (bot-token platforms need no running gateway) |
| Slack native slashes + app manifest (`hermes_cli/slack_cli.py` + slack adapter slash flow) | ✅ core | P265 port of hermes `hermes slack manifest` + the Slack socket-mode slash path: … [details](#slack-native-slashes-app-manifest-hermes-cli-slack-cli-py-slack-adapter-slash-flow) |
| Gateway monitoring + OTLP export (`agent/monitoring/*`) | ✅ core | P266 full port of the hermes gateway monitoring plane (`src/monitoring.rs`): content-free `GatewayHealthEvent`/`GatewayDiagnosticEvent`/`CronExecutionEvent` projections with a … [details](#gateway-monitoring-otlp-export-agent-monitoring) |
| Setup wizard (`hermes_cli/setup.py`) | ✅ core | P267 lean port of the hermes interactive setup wizard (`src/setup_cmd.rs`): `ulnclaw setup [model\|terminal\|gateway\|tools\|agent] [--quick] [--reset] [--non-interactive]` — … [details](#setup-wizard-hermes-cli-setup-py) |
| Model picker (`cmd_model` / `select_provider_and_model`) | ✅ core | P268 port of hermes `hermes model`: `ulnclaw model [--refresh]` — TTY-gated interactive provider + model switcher. Provider roster (built-ins + user `[providers.<slug>]` entries, current marked/default), credential prompt into `.env` when missing, models.dev catalog model list (agentic models, 40-row cap, `Type a model name manually…` fallback when the catalog is offline/unknown), config persistence, post-switch summary (provider/model/endpoint/key state). `--refresh` clears the models.dev picker cache first. Also hardened `models_dev::fetch_from_network` to build/drop the reqwest blocking client on a scoped OS thread so catalog fetches never panic inside the async gateway/dispatch context |
| Desktop launcher + command aliases (`cmd_gui` / `cmd_login` / `cmd_logout` / journey aliases) | ✅ core | P269 port of the remaining small hermes surfaces: `ulnclaw gui [--binary PATH] [--dev]` (alias `desktop`) resolves the packaged `ulnclaw desktop` executable (`--binary` → `ULNCLAW_DESKTOP_BINARY` → `desktop-electron/release/{linux,win,mac}-unpacked`) and spawns it detached, `--dev` runs the unpackaged app via `npm start` in `desktop-electron/`, missing binaries print build instructions; top-level `ulnclaw login` / `ulnclaw logout` aliases for `auth login` / `auth logout` (hermes `cmd_login`/`cmd_logout`); `ulnclaw learning` / `ulnclaw memory-graph` visible aliases for `journey` (hermes journey parser aliases) |
| Dynamic webhook subscriptions (`hermes_cli/webhook.py`) | ✅ core | P270 port of hermes `hermes webhook`: … [details](#dynamic-webhook-subscriptions-hermes-cli-webhook-py) |
| WhatsApp Cloud setup wizard (`hermes_cli/setup_whatsapp_cloud.py`) | ✅ core | P271 lean port of hermes `hermes whatsapp-cloud`: `ulnclaw whatsapp-cloud` — TTY-gated interactive wizard with hermes' field-shape validators (Phone Number ID vs … [details](#whatsapp-cloud-setup-wizard-hermes-cli-setup-whatsapp-cloud-py) |
| WhatsApp bridge status (`hermes whatsapp` surface) | ✅ core | P272 read-only diagnostics replacing hermes' Baileys wizard: `ulnclaw whatsapp [status]` — platform/mode state, resolved bridge URL (bundled auto-spawn vs external), Node.js detection, script-directory install/deps freshness (inspected without side effects), pidfile process state (running/stale/foreign), live `/health` probe with connected-as-JID / QR-pairing summaries, and next-step hints. The wizard's install+pair role is filled by the gateway-supervised bridge (deps install on first start, QR on startup in bridge.log) — documented difference |
| CLI (`hermes_cli/`) | ✅ core | chat REPL with slash commands (incl. `/rollback [N\|hash] [file]`, `/rollback diff <N>`, `/diff` checkpoint commands, `/recap`, `/goal` + `/subgoal` standing-goal loop, `/kanban` … [details](#cli-hermes-cli) |
| Git working diff (`working_diff.py`) | ✅ | `ulnclaw diff [--staged\|--all] [--dir PATH] [paths...]` + REPL `/gitdiff [staged\|all]`: working/staged/all modes, untracked files folded in via `git diff --no-index` (50-file cap), timeouts; the checkpoint-based REPL `/diff` remains separate |
| Delegation | ✅ | SubAgentRunner trait, depth limit, child sessions |
| Mixture of Agents (`moa_loop.py`, `moa_config.py`) | ✅ core | `[moa.presets.<name>]` reference fan-out + aggregator synthesis (`ulnclaw moa run/list/delete`, REPL `/moa <prompt>`); parallel references, loud/silent degraded policy, all-failed … [details](#mixture-of-agents-moa-loop-py-moa-config-py) |
| HTTP gateway (`gateway/platforms/api_server.py`) | ✅ core | `ulnclaw gateway`: OpenAI-compatible `/v1/chat/completions` (session continuity via `X-Ulnclaw-Session-Id`, `stream: true` SSE token streaming with `hermes.tool.progress` events) … [details](#http-gateway-gateway-platforms-api-server-py) |
| TUI/web/app surfaces | ✅ core | TUI: chat REPL + raw-mode session picker (`sessions browse`); web: HTTP gateway serves OpenAI-compatible + session APIs with local-app CORS, so any browser dashboard works against it; desktop: Electron shell (`desktop-electron/`, see Desktop GUI row) |
| Sandbox env scrub + passthrough (`environments/local.py` blocklist, `env_passthrough.py`) | ✅ | terminal/execute_code children get the process env minus a provider/tool credential blocklist and venv markers (`VIRTUAL_ENV`/`CONDA_PREFIX`); skill `required_environment_variables` (registered on `skill_view`) and `[terminal] env_passthrough` allowlist variables — protected provider credentials and `AUXILIARY_*_API_KEY`/`GATEWAY_RELAY_*` dynamic secrets are always refused (hermes GHSA-rhgp-j443-p4rf, fail closed) |
| Environments (`tools/environments/`) | ✅ core | `terminal` backends: local (default), docker (`ensure_docker_container` inspect→run), ssh (BatchMode, identity file); `[terminal] backend/container/image/ssh_host/...`; modal/daytona/vercel deferred |
| Checkpoint manager (`checkpoint_manager.py`) | ✅ | v2 shared shadow git store (`<home>/checkpoints/store`): per-project refs/indexes, transparent pre-edit snapshots (once per turn before `write_file`/`patch`), list/restore/diff/prune CLI, size caps, oversize-file filter, orphan/stale auto-prune; P317 exposed checkpoints over HTTP — `GET /api/checkpoints/status`, `GET /api/checkpoints?dir=`, `POST /api/checkpoints/restore`, `POST /api/checkpoints/prune` — with a Doctor panel (store census, per-project rows, prune) |
| Browser supervisor | ✅ | auto-launches managed headless Chrome/Chromium for `ULNCLAW_BROWSER_CDP=auto` |
| Camofox backend (`tools/browser_camofox.py`) | ✅ core | `browser/camofox.rs`: `CAMOFOX_URL` REST anti-detect browser (Camoufox) backend — all 12 browser tools route through REST (tab sessions, accessibility snapshots with refs … [details](#camofox-backend-tools-browser-camofox-py) |
| Cloud browser providers (`agent/browser_provider.py` + `agent/browser_registry.py` + `plugins/browser/*`) | ✅ core | `browser/cloud.rs`: `CloudBrowserProvider` trait + registry port — **Browserbase** (`BROWSERBASE_API_KEY`+`BROWSERBASE_PROJECT_ID`, `POST /v1/sessions` with … [details](#cloud-browser-providers-agent-browser-provider-py-agent-browser-registry-py-plugins-browser) |

### Detailed notes (feature parity)

<a id="context-compression-conversation-compression-py"></a>

#### Context compression (`conversation_compression.py`) — ✅

- budget-triggered, middle-turn summarization via secondary model call, keeps system prompt + first
  user message + recent tail
- summary call honors `[auxiliary.compression]` routing. P233 ported the three-layer tool-result
  persistence (`tools/tool_result_storage.py` + `budget_config.py`): layer 2 persists any tool result
  over its threshold (default 100K chars
- `read_file` pinned to infinity against persist→read→persist loops) to
  `<tmp>/ulnclaw-results/<call-id>.txt` THROUGH the terminal backend (local fs write / docker+ssh
  stdin pipe, so the file is reachable wherever commands run) and swaps in a `<persisted-output>`
  1500-char preview + path the model can `read_file`
- layer 3 enforces the 200K-char per-turn aggregate budget, spilling the largest non-persisted results first
- failed writes degrade to an inline truncation notice, never silent loss

<a id="persistent-goals-the-ralph-loop-hermes-cli-goals-py"></a>

#### Persistent goals — the Ralph loop (`hermes_cli/goals.py`) — ✅ core

- `goals.rs`: standing goals that survive across turns — after every assistant turn a `goal_judge`
  auxiliary model decides done/continue/wait
- continuation prompts are fed back as normal user messages until the goal is achieved,
  paused/cleared, or the turn budget (default 20) exhausts. Completion contracts
  (outcome/verification/constraints/boundaries/stop_when) via inline `field: value` goal lines or
  `/goal draft` (aux-model drafted)
- subgoals (`/subgoal`) fold into judge + continuation prompts
- WAIT verdicts park the loop on a background-process pid/session or a deadline without burning
  turns (`/goal wait <pid>`, auto-clear on release)
- fail-open judging with auto-pause after 3 consecutive parse or 5 transport failures
- goal state persists in `state_meta` keyed `goal:<session_id>` (survives restarts,
  `migrate_goal_to_session` for session rotation)
- REPL `/goal` + `/subgoal` slash commands
- kanban goal loop stays desktop-side

<a id="approval-system-approval-py"></a>

#### Approval system (`approval.py`) — ✅

- command normalization (backslash-joins, `${IFS}`, comment strip), hardline floor (block), recoverable-costly (confirm)
- REPL y/N prompt
- gateway run approvals (`POST /v1/runs/:id/approval`, once/session/always/deny, SSE
  `approval.request`), fail-closed `[approvals] timeout` (default 300s), `always` grants persisted
  across restarts
- messaging sessions approve in-chat (hermes `tools/approval.py` gateway registry port,
  `src/approval_gateway.rs`): the approve callback registers a blocking per-session entry, renders the
  prompt on the platform (Teams AdaptiveCard buttons, `/approve` text fallback elsewhere, credentials
  redacted first), and `/approve [all] [session|always]` / `/deny [all]` chat commands or card taps
  resolve the oldest pending entry
- `[approvals] mode = manual|smart|off` — smart mode asks an auxiliary guardian LLM
  (prompt-injection-hardened prompt, operator `smart_policy` on the trusted channel) and escalates to
  a human when unsure, `off` auto-approves below the hardline floor
- `cron_mode = deny|approve` governs unattended cron runs (deny = fail-closed default)
- P221 completed the smart-approval surface: `denial_breaker_threshold` (default 3) — after N
  consecutive guardian DENY verdicts the unattended deny message escalates from "do not retry" to a
  hard-stop instruction (report to the user / ask for a manual run), any approval resets the streak —
  and `deny` fnmatch globs that block matching commands unconditionally BEFORE the mode=off/yolo
  bypass (case-insensitive `*`/`?`/`[...]` semantics). P237 ported the tirith pre-exec content scanner
  (`tools/tirith_security.py`): every gated terminal command is additionally scanned by the external
  `tirith` binary (homograph URLs, pipe-to-interpreter, terminal injection)
- the exit code is the verdict (0 allow / 1 block / 2 warn) and JSON stdout only enriches findings
  (cap 50) / summary (cap 500 chars)
- block and warn both become approvable warnings merged into the confirm reason
  (`_format_tirith_description`), operational failures (spawn error / timeout / unknown exit / signal
  death) honour `[security] tirith_fail_open` (default true), `.app`-TLD lookalike warns are
  suppressed, 3 consecutive crashes open a process-lifetime circuit breaker, and warn-once dedupe
  keeps fail-open misconfigurations quiet
- resolution order is PATH → `<home>/bin/tirith` → auto-install from GitHub releases (SHA-256 always verified
- cosign provenance when on PATH
- 24 h disk failure marker `.tirith-install-failed`
- `ensure_installed` downloads in a background thread at agent startup so startup never blocks)
- `[security] tirith_enabled/tirith_path/tirith_timeout/tirith_fail_open` with
  `TIRITH_ENABLED`/`TIRITH_BIN`/`TIRITH_TIMEOUT`/`TIRITH_FAIL_OPEN` env overrides
- platforms without tirith builds (Windows) silently fall back to the pattern guards

<a id="models-dev-catalog-agent-models-dev-py"></a>

#### models.dev catalog (`agent/models_dev.py`) — ✅ core

- `models_dev.rs`: fetches `https://models.dev/api.json` with a three-tier cache — in-memory (1h
  TTL, stale served immediately while a background thread refreshes) → disk
  (`$ULNCLAW_HOME/models_dev_cache.json`, any age) → singleflight network with 5-minute process-wide
  failure backoff
- provider ID mapping + identity fallback, context/capability lookups (case-insensitive,
  `:cloud`/`-cloud` suffix fallback), agentic catalog filters (noise patterns + Google hidden list),
  `get_provider_info`/`get_model_info`
- `ULNCLAW_MODELS_DEV_URL` mirror override (http(s)/file), `ULNCLAW_MODELS_DEV_CACHE` path override
- gateway `/api/model/options` picker inventory (P168) + `?refresh=true`
- CLI `ulnclaw models providers\|list\|info\|refresh`

<a id="mcp-client-mcp-tool-py"></a>

#### MCP client (`mcp_tool.py`) — ✅ core

- stdio JSON-RPC: initialize/tools/list/tools/call
- `[[mcp.servers]]` config
- tools registered as `mcp__<server>__<tool>`
- OSV malware preflight for npx/uvx/pipx launches (`osv_check.py` port: MAL-* advisories block,
  fail-open, 1h verdict cache, `OSV_ENDPOINT`/`OSV_CHECK_CACHE_TTL` overrides). P235 ported the stdio
  watchdog (`mcp_stdio_watchdog.py`) as a kernel primitive: on Linux each MCP child is spawned with
  `prctl(PR_SET_PDEATHSIG, SIGKILL)` so a hard ulnclaw death (kill -9 / crash) reaps the server
  instead of orphaning it to race the next startup's upstream session (hermes spawns a supervisor
  process for the same effect
- macOS/Windows keep graceful-exit reaping). P236 ported the schema cache + lazy startup
  (`mcp_schema_cache.py` + `mcp_tool.py` `_register_from_cache_sync`): per-server tool manifests
  persist to `<home>/cache/mcp_schema_cache.json` (0600 file in a 0700 `cache/` dir, atomic staged
  rename) keyed by server name + SHA-256 fingerprint of the connection config (same payload shape as
  hermes: command/args/url/transport/tools filters
- env excluded, like hermes)
- live registration write-through-refreshes the manifest after every successful connect, and a `lazy
  = true` server with a matching cache entry registers its tools WITHOUT spawning the child — the
  first tool call runs the OSV preflight, spawns the server, and reconciles the live tool list against
  the stale manifest (cache refreshed
- phantom tools fail fast with a stale-schema error instead of being deregistered, since ulnclaw
  handlers hold no registry handle
- identical cache entries skip the rewrite). P238 ported `/reload-mcp` + the slash-confirm primitive
  (`tools/slash_confirm.py` + `cli.py _confirm_and_reload_mcp/_reload_mcp`): REPL `/reload-mcp` warns
  that rebuilding the tool surface invalidates the provider prompt cache and prompts Approve Once /
  Always Approve / Cancel (gated by `approvals.mcp_reload_confirm`, default on
- "always" persists `false` via the config writer), then re-reads config.toml fresh, drops every
  `mcp:*` tool, reconnects all servers (lazy servers re-register from cache), reports
  Reconnected/Added/Removed + tool count, and injects a change note at the END of the conversation so
  the model sees the new surface while the prompt-cache prefix survives
- the gateway-side confirm primitive (`src/slash_confirm.rs`: session-keyed pending registry,
  confirm_id supersede semantics, pop-before-run double-callback guard, 300 s staleness timeout,
  once/always/cancel choices) backs platform button / `/approve`-text resolution paths. P239 ported
  the remote MCP transports (hermes `mcp_tool.py` Streamable HTTP + SSE paths, hand-rolled
  JSON-RPC-over-HTTP in place of the Python MCP SDK): `[[mcp.servers]]` entries with a `url` connect
  over Streamable HTTP (POST with `Accept: application/json, text/event-stream`, JSON or SSE reply
  bodies, `Mcp-Session-Id` capture/echo, protocol version 2025-03-26), `transport = "sse"` selects the
  pre-2025-03-26 SSE protocol (GET stream → `endpoint` event → POST + `message`-event replies
  correlated by id), and static `headers` (e.g. `Authorization`) ride on every request
- the schema-cache fingerprint now carries the real url/transport. P240 ported MCP OAuth 2.1 + PKCE
  (`tools/mcp_oauth.py` + `tools/mcp_oauth_manager.py` core, hand-rolled in place of the MCP SDK's
  `OAuthClientProvider`): `[[mcp.servers]] auth = "oauth"` (+ optional `[mcp_servers.<name>.oauth]`
  client_id/client_secret/scope/redirect_port/redirect_uri/redirect_host/client_name) runs
  protected-resource → authorization-server metadata discovery, RFC 7591 dynamic client registration,
  S256 PKCE authorization-code flow with a loopback callback server (auto port, state validation, 300
  s timeout, browser auto-open), token exchange + refresh grants
- tokens/registrations/metadata persist 0600 under
  `<home>/mcp-tokens/<server>.{json,client.json,meta.json}` (hermes layout)
- unattended sessions (cron) use cached/refreshable tokens or fail fast
- 401s on remote requests trigger one recovery (refresh → re-auth, per-server dedupe) and a single
  request replay. P241 ported stdio environment filtering (hermes `_build_safe_env` +
  `_interpolate_env_vars`): stdio MCP children spawn with a cleared environment plus a filtered copy —
  only safe baseline vars (PATH/HOME/USER/LANG/LC_ALL/TERM/SHELL/TMPDIR plus the case-insensitive
  Windows process/location set), `XDG_*` vars, secret-source-tagged vars (keys present in
  `<home>/.env`, the dotenv layer ulnclaw itself loaded), and vars explicitly declared in the server's
  `env` block pass through, so ambient API keys/tokens/credentials never leak to MCP servers
- `${VAR}` and Cursor-style `${env:VAR}` placeholders in `command`/`args`/`env` values resolve
  against the secret scope first, then the process environment, and unset references keep the literal
  placeholder. P242 ported the dashboard-mediated OAuth bridge (hermes `tools/mcp_dashboard_oauth.py`
  + `web_server.py` flow registry/routes): the gateway serves `POST /api/mcp/servers/<name>/auth`
  (validates the server is remote and OAuth-bound — stdio servers authenticate via env keys,
  header-auth servers via headers
- 8-pending-flow cap → 429 and per-server dedupe → 409, 15-minute TTL GC), `GET
  /api/mcp/oauth/flows/<flow_id>` (status + tool list) and the browser redirect target `GET
  /api/mcp/oauth/callback/<server>` — an open route (no bearer gate) where the `state` parameter is
  the protection: constant-time comparison, replay rejected with 409, provider errors surface as 400
- the OAuth machinery detects the active flow through a task-local (hermes contextvar) and publishes
  the authorization URL / waits for the callback through the flow instead of binding a loopback port,
  with the redirect_uri resolved to the gateway callback URL and registered consistently at dynamic
  client registration
- the worker backs up stored tokens before forcing a fresh authorization and restores them on
  failure, then probes the authorized server for its tool list (shown in the flow snapshot).
  Divergence: the ulnclaw gateway is a standalone daemon without the agent's tool registry, so hermes'
  live in-process reconnect is skipped — new tokens take effect on the next session / lazy spawn. P243
  ported the OAuth manager hardening (hermes `tools/mcp_oauth_manager.py`): a process-wide
  `OAuthManager` (`src/mcp/oauth_manager.rs`) keeps a per-server tokens-file mtime watermark — a
  pre-request disk watch (one `stat()` per POST, hermes `HermesMCPOAuthProvider` pre-flow reload)
  picks up tokens another process refreshed on disk (cron job, dashboard bridge, other profile)
  without a restart, and 401 recovery is deduplicated by the FAILED access token through shared
  in-flight futures (hermes `handle_401` / `pending_401`, Claude Code `pending401Handlers` design
  reference): N concurrent calls that 401 on the same token fire exactly one recovery —
  external-refresh short-circuit first, then refresh grant / re-auth — every waiter shares the
  outcome, and a spawned driver task (hermes `_inflight_tasks`) completes the recovery even if the
  first requester is cancelled
- the reauth hook now carries the failed token. P244 wired the messaging-adapter delivery paths
  (hermes `gateway/run.py` intercept + `_request_slash_confirm` + `platforms/base.py
  send_slash_confirm`): `/reload-mcp` in a platform chat is gated by `approvals.mcp_reload_confirm`,
  registers the pending confirm BEFORE sending (no button-click race), and renders native buttons
  through `PlatformSender::send_slash_confirm` where a platform overrides it — text fallback otherwise
- incoming replies intercept before dispatch: `/approve`/`yes`/`ok`/`confirm` → once,
  `/always`/`remember` → always, `/cancel`/`no`/`deny`/`nevermind` → cancel (slash form, plain text
  and `!`-prefixed Slack-style), live tool approvals take precedence, unrelated messages fall through
  and stale confirms are dropped
- on confirmation the reload runs against fresh config, reports reconnected/added/removed + tool
  count, and injects the `[system note]` change note at the END of the session history (prompt-cache
  prefix survives)
- `always` persists `approvals.mcp_reload_confirm = false`

<a id="mcp-channel-bridge-mcp-serve-py"></a>

#### MCP channel bridge (`mcp_serve.py`) — ✅ core

- P260 full port of hermes `mcp_serve.py` (`src/mcp_serve.rs`, `ulnclaw mcp serve [--verbose]`):
  stdio JSON-RPC 2.0 MCP server exposing messaging conversations to any MCP client (Claude Code,
  Cursor, Codex, …). OpenClaw's 9-tool bridge surface plus the hermes `channels_list` extra:
  `conversations_list` (platform/search filters, limit clamp 1–200), `conversation_get`,
  `messages_read` (user/assistant only, 2000-char cap, chronological with total_in_session),
  `attachments_fetch` (`MEDIA:<path>` extraction by message row id), `events_poll`/`events_wait`
  (cursor queue, 1000-event cap, 200 ms state.db-mtime-gated polling with startup baseline — hermes
  #13414 no-replay semantics — long-poll capped at 300 s), `messages_send` (rides the P259
  `send_message` logic incl. standalone Telegram/Discord/Slack REST delivery without a live gateway),
  `channels_list` (channel directory with sessions-index fallback),
  `permissions_list_open`/`permissions_respond` (live-session approval ledger, decision validation).
  Platform sessions read from `state.db` rows keyed `platform-<name>-<chat>` (hermes #9006 shape via
  `COALESCE(session_key, id)`)
- display names enrich from titles + the channel directory

<a id="acp-adapter-acp-adapter"></a>

#### ACP adapter (`acp_adapter/`) — ✅ core

P261 port of hermes `acp_adapter/` (`src/acp_adapter.rs`, `ulnclaw acp [--verbose]`): Agent Client
Protocol stdio server for editors (Zed). Bidirectional newline-delimited JSON-RPC 2.0 with outbound
request/response matching. Surface: `initialize` (protocol v1, loadSession + image/embeddedContext
prompt capabilities), `authenticate` (no methods advertised), `session/new` / `session/load`
(history replay from `acp-<sessionId>` store rows as user/agent message chunks), `session/prompt`
(text + image blocks → native multimodal turn via `run_with_session_images`, streaming
`session/update` notifications: `agent_message_chunk`, `agent_thought_chunk`,
`tool_call`/`tool_call_update` with kind mapping + open-call id queue, `plan` updates from todo
results — hermes events.py parity), cooperative `session/cancel`,
`session/set_mode`/`session/set_model` acks, unknown-method `-32601` with hermes benign-probe
semantics. Tool approvals ride `session/request_permission` (Allow Once / Always Allow / Reject —
hermes edit_approval parity)

<a id="batch-runner-batch-runner-py"></a>

#### Batch runner (`batch_runner.py`) — ✅ core

P262 full port of hermes `batch_runner.py` (`src/batch_runner.rs`, `ulnclaw batch --dataset-file X
--run-name R [--batch-size N] [--num-workers N] [--resume] [--verbose] [--max-iterations N] [--model
M]`): JSONL dataset loading with per-line validation, parallel batch processing (tokio
`buffer_unordered` worker pool — batches parallel, prompts sequential inside each worker, hermes
Pool semantics), checkpointing (`batch_runs/<run>/checkpoint.json` with completed indices +
per-batch stats, atomic tmp+rename writes, run_name-guarded loads), smart resume (content-scan of
saved `batch_*.json` ∪ checkpoint indices), hermes from/value trajectory format
(`system`/`human`/`gpt` turns with `<tool_call>` + `<tool_response>` XML pairing, reasoning
scratchpad passthrough), tool usage stats aggregation with JSON error-shape failure detection (incl.
terminal's nested `content.error` + `success:false` patterns), reasoning coverage stats, and a final
`summary.json`

<a id="slack-native-slashes-app-manifest-hermes-cli-slack-cli-py-slack-adapter-slash-flow"></a>

#### Slack native slashes + app manifest (`hermes_cli/slack_cli.py` + slack adapter slash flow) — ✅ core

- P265 port of hermes `hermes slack manifest` + the Slack socket-mode slash path: `ulnclaw slack
  manifest [--write [PATH]] [--name N] [--description D] [--long-description T |
  --long-description-file F] [--slashes-only] [--no-assistant | --agent-view]` emits the Slack app
  manifest registering every platform command as a native slash (`/ulnclaw` catch-all reserved first,
  hermes reserved-command skip list, 50-command clamp, name sanitizer, 175–4000-char long-description
  validation, assistant/agent/none messaging experiences with the matching scopes + events). The Slack
  adapter consumes `slash_commands` socket envelopes: auth-gated dispatch through the normal platform
  path with the reply delivered via `response_url` (`replace_original`, postMessage fallback) — hermes
  `_handle_slash_command` semantics. New shared layer `platform_slash.rs`: the hermes gateway
  direct-command subset (`/help` `/skills` `/tools` `/recap` `/title` `/usage` `/insights`) answers on
  EVERY messaging platform without an LLM turn
- `/skill-name` + `/<bundle>` expand into scaffolded agent turns (gateway session-chat parity)

<a id="gateway-monitoring-otlp-export-agent-monitoring"></a>

#### Gateway monitoring + OTLP export (`agent/monitoring/*`) — ✅ core

P266 full port of the hermes gateway monitoring plane (`src/monitoring.rs`): content-free
`GatewayHealthEvent`/`GatewayDiagnosticEvent`/`CronExecutionEvent` projections with a global bounded
emitter (fire-and-forget, drop-on-full at 1024), unconditional egress redaction (secrets→PII:
bearer/token/`***`/email/uuid/phone scrub — lookbehind-free port of hermes `_PHONE_RE` semantics),
persistent install-id minting, `[monitoring]` config with hermes defaults + interval floors (health
60s / diagnostics 5s, floors 5/1), built-in OTLP/HTTP JSON trace exporter (resourceSpans/scopeSpans,
`ulnclaw.*` attribute whitelist per span kind + 500-char clamp, `headers_env` resolved at export
time, `/v1/traces` appended once, timeout-flush batch loop). Gateway wiring: `gateway_started`
lifecycle event, periodic heartbeat sampler over live run state + enabled-platform count, cron
execution events from the scheduler, `ulnclaw monitoring status` surface. Scope is health + redacted
diagnostics only — no prompts, messages, tool args/results, or usage analytics

<a id="setup-wizard-hermes-cli-setup-py"></a>

#### Setup wizard (`hermes_cli/setup.py`) — ✅ core

P267 lean port of the hermes interactive setup wizard (`src/setup_cmd.rs`): `ulnclaw setup
[model|terminal|gateway|tools|agent] [--quick] [--reset] [--non-interactive]` — first-time mode
choice (Full / Blank Slate), reconfigure flow with current-value defaults on existing installs,
per-section runs, timestamped config backup before modification (#3522), non-interactive guidance on
no-TTY, provider picker (OpenAI/Anthropic/OpenRouter/DashScope/Ollama/llama.cpp/custom) with API
keys written to `.env`, terminal backend picker (local/docker/ssh) with per-backend fields,
26-platform messaging checklist with per-platform token prompts + follow-up guidance (`qq setup` /
`weixin login` / config.toml notes) and the hermes missing-home-channel hint, toolset multi-select
over the live registry, agent settings, and an end-of-wizard summary. Dropped vs hermes (documented
differences): Nous Portal quick-setup, OpenClaw migration offer, curses space-toggle checklist
(comma-separated numeric multi-select instead), TTS/telemetry sections

<a id="dynamic-webhook-subscriptions-hermes-cli-webhook-py"></a>

#### Dynamic webhook subscriptions (`hermes_cli/webhook.py`) — ✅ core

- P270 port of hermes `hermes webhook`: `ulnclaw webhook subscribe <name> [--description D]
  [--events e1,e2] [--secret S] [--prompt P] [--skills s1,s2] [--deliver target] [--deliver-chat-id C]
  [--deliver-only] [--script CMD]`, `webhook list`, `webhook remove <name>`, `webhook test <name>
  [--payload JSON]` (signed HMAC-SHA256 POST). Subscriptions persist to `webhook_subscriptions.json`
  (atomic write, 0600 perms — holds per-route HMAC secrets
- name grammar + normalization, random 64-hex secret minting, hermes created_at format,
  `--deliver-only` validation). Gateway: `dynamic_webhook_route` mounts `/webhooks/:name` (static
  platform routes keep precedence), hot-reloads the file per request — no restart needed — and maps
  each subscription onto the existing generic-webhook pipeline via the extracted
  `process_generic_webhook` helper (signature/rate-limit/idempotency/event-filter/deliver)

<a id="whatsapp-cloud-setup-wizard-hermes-cli-setup-whatsapp-cloud-py"></a>

#### WhatsApp Cloud setup wizard (`hermes_cli/setup_whatsapp_cloud.py`) — ✅ core

- P271 lean port of hermes `hermes whatsapp-cloud`: `ulnclaw whatsapp-cloud` — TTY-gated interactive
  wizard with hermes' field-shape validators (Phone Number ID vs pasted-phone-number trap, `EAA`
  access-token prefix with OpenAI/Slack/GitHub mispaste diagnosis, 32-hex App Secret), auto-minted
  verify token with regen prompt, recipient-allowlist normalization, and the SETUP COMPLETE follow-up
  block (cloudflared tunnel, gateway, Meta webhook dashboard) adapted to ulnclaw paths
  (`/webhooks/whatsapp`, gateway port). Credentials persist to `[messaging.whatsapp_cloud]` in
  config.toml (ulnclaw resolution) instead of `.env`
- the analytics-only App ID/WABA ID step is omitted (no consumer)
- hermes exit-code contract (0 ok / 1 abort / 2 partial) preserved

<a id="cli-hermes-cli"></a>

#### CLI (`hermes_cli/`) — ✅ core

- chat REPL with slash commands (incl. `/rollback [N|hash] [file]`, `/rollback diff <N>`, `/diff`
  checkpoint commands, `/recap`, `/goal` + `/subgoal` standing-goal loop, `/kanban` inline board ops,
  `/egress` egress-proxy status, `/pet` + `/hatch` petdex surfaces), one-shot `run`,
  sessions/tools/skills/cron/checkpoints subcommands (incl. `sessions export --format md\|html` —
  SHA256-verified Markdown or standalone HTML + manifest —, `sessions recap`, `sessions recover`,
  `sessions prune`/`archive`/`stats`/`delete`/`rename`/`optimize`/`repair`/`browse`/`retitle-skills`,
  `kanban init`/`boards list|create|rm|switch|show|rename|set-workdir`/`create [--project P]`/`list
  [--workflow-template-id
  T]`/`show`/`ready`/`assign`/`claim`/`heartbeat`/`done`/`block`/`unblock`/`archive`/`comment`/`link`/`unlink`/`dispatch
  [--max-spawn N] [--dry-run]`/`gc`/`swarm <goal> --worker ASSIGNEE:TITLE[:skill,skill] [--worker ...]
  --verifier ASSIGNEE --synthesizer ASSIGNEE [--idempotency-key K] [--json]`/`specify [id |
  --all]`/`decompose [id | --all]`/`diagnostics [id] [--min-severity S] [--json]`/`schedule`/`promote
  [--force]`/`reclaim`/`reassign [--reclaim]`/`edit`/`set-model [--provider
  P]`/`attach`|`attachments`|`attach-rm`/`tail [--follow]`/`stats [--json]`/`watch [--assignee P]
  [--kinds K] [--interval S]` (kanban task engine: boards, TTL claim locks + stale takeover, hermes
  status lifecycle with icons, comments + event trail, `kanban_task_*` plugin hooks), `project
  create/list/show/add-folder/remove-folder/rename/set-primary/use/archive/restore/bind-board/scan
  [--root P --max-depth N]/repos [--clear]` (first-class project registry anchoring kanban worktrees +
  git-repo discovery cache), `secrets status/sync/bitwarden setup|install|status|disable/onepassword
  setup|status|set|remove|disable`, `egress
  install/setup/start/stop/restart/reload/status/disable/config` (managed iron-proxy sandbox egress
  firewall), `computer-use status/doctor/install`, `plugins
  list/install/update/remove/enable/disable/accept-hooks`, `hooks list/test/revoke/doctor`, `pairing
  list/approve/revoke/clear-pending`, `weixin login` (WeChat iLink QR-scan account setup), `auth
  login/status/refresh/logout`, `slack manifest` (Slack app manifest generator with native-slash
  registration), `monitoring status` (gateway monitoring / OTLP export status), `setup [section]
  [--quick] [--reset]` (interactive onboarding wizard), `model [--refresh]` (interactive provider +
  model switcher), `gui [--binary PATH] [--dev]` (desktop launcher
- alias `desktop`), `login`/`logout` (auth aliases), `webhook subscribe|list|remove|test` (dynamic
  webhook subscriptions), `whatsapp-cloud` (WhatsApp Business Cloud API setup wizard), `whatsapp
  [status]` (Baileys bridge diagnostics), `sync status/pull/push/now/enable/disable/device`,
  `uninstall --full/--dry-run/--yes` (code checkout + shell PATH entries + wrapper symlinks + optional
  home wipe
- hermes `uninstall.py` port — Windows registry/env steps not ported)), `moa run/list/delete`,
  `models providers/list/info/refresh` (models.dev catalog), `skills blueprints/schedule/unschedule`,
  `diff`, `init`

<a id="mixture-of-agents-moa-loop-py-moa-config-py"></a>

#### Mixture of Agents (`moa_loop.py`, `moa_config.py`) — ✅ core

- `[moa.presets.<name>]` reference fan-out + aggregator synthesis (`ulnclaw moa run/list/delete`, REPL `/moa <prompt>`)
- parallel references, loud/silent degraded policy, all-failed early return, joined-fallback on aggregator failure
- P169 ported the persistent facade + traces + privacy filter: `[model] provider = "moa"` runs the
  whole agent loop on a preset (`model` selects the preset — hermes `build_moa_facade`): per-turn
  reference fan-out cached across tool-loop iterations (hermes reference cache), guidance attached at
  the END of the aggregator prompt (`_attach_reference_guidance`, prompt-cache stable), the caller's
  tools forwarded to the aggregator, aggregator-failure fallback returning the guidance block
- `[moa] save_traces`/`trace_dir` write one JSONL record per turn under `moa-traces/` keyed by
  sanitized session id (hermes `save_moa_turn`)
- `privacy_filter = display|full` redacts centralized secret shapes plus MoA email/formatted-phone
  patterns from reference surfaces (`display`) and additionally from the text injected into the
  aggregator prompt (`full`, hermes #59959)

<a id="http-gateway-gateway-platforms-api-server-py"></a>

#### HTTP gateway (`gateway/platforms/api_server.py`) — ✅ core

- `ulnclaw gateway`: OpenAI-compatible `/v1/chat/completions` (session continuity via
  `X-Ulnclaw-Session-Id`, `stream: true` SSE token streaming with `hermes.tool.progress` events),
  `/v1/responses` (stateful via `previous_response_id`, `stream: true` Responses-API SSE events),
  `/v1/models`, `/api/model/options` multi-provider picker inventory (P168, hermes `inventory.py`
  port: current-provider row enriched from models.dev with capability/cost maps, `[providers.<slug>]`
  config rows with bounded live `/models` probes, env-authenticated canonical rows,
  `include_unconfigured` skeleton rows with `auth_type`/`key_env`/`warning` setup hints,
  `explicit_only` filter, canonical declaration order, per-lab featured models + formatted pricing,
  `[model_catalog] excluded_providers`
- `?refresh=true` busts the catalog cache and re-probes), `/v1/capabilities`, `/v1/runs` (async runs
  + SSE events + stop + approval), `/api/sessions` CRUD + chat + chat/stream (slash passthrough:
  `/help`/`/skills`/`/tools`/`/recap`/`/title`/`/usage` execute without an LLM turn
- `/skill-name` + `/<bundle>` invocations expand into hermes skill-scaffold user turns — the hermes
  gateway/run.py skill-command-sharing port) + `PATCH` (title/end_reason) + `fork` + per-session model
  lock (enforced on every turn) + `recap` + `POST /api/sessions/prune|archive` bulk cleanup
  (filterable, dry-run preview — mirrors `sessions prune/archive`), `/api/jobs` cron HTTP API (CRUD +
  pause/resume/run + `GET /api/jobs/delivery-targets` + `POST /api/jobs/fire` Chronos NAS fire webhook
  — P219: `deliver` targets `local`/`origin`/platform/`platform:chat[:thread]`/`all` persist on the
  job and resolve at fire time against registered platform senders with home-channel env vars,
  `[SILENT]` suppression, wrapped deliveries, failure summaries and `last_delivery_error` tracking
- the fire webhook verifies NAS-minted JWTs (RS/ES, JWKS-URL or inline PEM, `purpose=cron_fire`)
  from `[cron.chronos]` config, answers 401/400/200-gone/202 and runs the job in the background with
  claim-based retry dedup), `/v1/skills`, `/v1/toolsets`, `/metrics` (Prometheus counters/gauges —
  ulnclaw ops extension), `/api/usage` (token accounting: process counters + all-time store totals +
  per-session rows — ulnclaw ops extension), `/v1/delegations` (background-delegation registry —
  ulnclaw ops extension), `/v1/browser/status|connect|disconnect` (live CDP endpoint control, hermes
  `/browser connect` parity — ulnclaw ops extension), `/api/uploads` (binary uploads into the
  content-addressed media cache — desktop clipboard-image paste), `/api/fs/*` (gateway-filesystem
  browser: `list` with hidden/vendor filtering, `read-text`/`write-text` with size caps + atomic
  tmp-file rename, `read-data-url` for attach downloads, `git-root` + `default-cwd` discovery,
  `download` attachment streaming with `?token=` query auth + `mkdir` (P335, hermes
  `/api/files/download|mkdir` parity) — hermes `/api/fs` parity), `/api/media` (serve one gateway-host
  image as a base64 data URL — image-extension allowlist, 25 MiB cap, containment to the gateway's
  `images`/`screenshots`/`cache`/`media-cache` roots
- hermes `/api/media` parity — P338), `/api/learning/graph` + `/api/learning/node` GET/PUT/DELETE
  (learning "star map": learned non-base skills + MEMORY.md/USER.md memory chunks with related-skill
  and lexical-overlap edges
- node detail/archive/edit mutations — hermes web_server `/api/learning/*` parity, CLI twin `ulnclaw
  journey`), `/api/projects` registry CRUD (folders, primary, archive/restore, active pointer, board
  binding with the workdir mirror) + `/api/projects/scan|repos` discovery (P162), `/api/backups` quick
  snapshots (list/create/prune + per-snapshot restore — hermes `/api/ops/backup` parity, CLI twin
  `ulnclaw backup`), bearer-token auth. Single-instance guard (P155, hermes `gateway run --replace`
  contract): the running gateway writes `<home>/gateway.pid` (pid + `/proc` start-time token —
  PID-reuse proof)
- a fresh start refuses while another live instance holds the file (stale records self-heal),
  `--replace` terminates the old instance (SIGTERM → SIGKILL escalation) and takes over, `--force`
  runs alongside

<a id="camofox-backend-tools-browser-camofox-py"></a>

#### Camofox backend (`tools/browser_camofox.py`) — ✅ core

- `browser/camofox.rs`: `CAMOFOX_URL` REST anti-detect browser (Camoufox) backend — all 12 browser
  tools route through REST (tab sessions, accessibility snapshots with refs,
  click/type/scroll/back/press, image extraction from snapshots, screenshots for vision)
- CDP overrides take priority
- `CAMOFOX_API_KEY` bearer auth, `CAMOFOX_USER_ID`/`CAMOFOX_SESSION_KEY` identity override +
  existing-tab adoption, Docker loopback URL rewriting (`CAMOFOX_REWRITE_LOOPBACK_URLS` + alias), VNC
  URL discovery from `/health`, SSRF private-page guard on reads, console/raw-CDP/dialogs report
  unsupported
- managed persistence via `CAMOFOX_MANAGED_PERSISTENCE` (stable UUIDv5 profile-scoped userId, hermes
  `browser.camofox.managed_persistence`)
- gateway + REPL browser status report the backend

<a id="cloud-browser-providers-agent-browser-provider-py-agent-browser-registry-py-plugins-browser"></a>

#### Cloud browser providers (`agent/browser_provider.py` + `agent/browser_registry.py` + `plugins/browser/*`) — ✅ core

`browser/cloud.rs`: `CloudBrowserProvider` trait + registry port — **Browserbase**
(`BROWSERBASE_API_KEY`+`BROWSERBASE_PROJECT_ID`, `POST /v1/sessions` with
keepAlive/proxies/advancedStealth/timeout knobs and the hermes 402 fallback chain, `REQUEST_RELEASE`
close), **Browser Use** (direct `BROWSER_USE_API_KEY` or managed Nous gateway via `[browser]
use_gateway`, `POST /browsers` with `X-Idempotency-Key` managed-mode semantics, `timeoutAt` expiry
authority, `PATCH {action: stop}` close), **Firecrawl** (`FIRECRAWL_API_KEY`, `POST /v2/browser`
with `FIRECRAWL_BROWSER_TTL`, DELETE close). Resolution mirrors `_resolve`: `"local"` disables cloud
mode, explicit name wins regardless of availability (precise missing-credentials errors), else
legacy walk browser-use → browserbase filtered by availability — firecrawl stays explicit-only so
web-extract keys never silently route to a paid browser. `with_session` lazily creates/caches the
session, retires expired endpoints, and `main` releases the session at exit (hermes atexit cleanup).
`[browser] cdp_url` config tier added to endpoint resolution (env > config > cloud > managed launch)


<a id="storage-layout"></a>

## Storage layout

```
~/.ulnclaw/                 (ULNCLAW_HOME override supported; HERMES_HOME honored for migration)
├── config.toml             main configuration
├── .env                    KEY=VALUE secrets (checked after process env)
├── state.db                SQLite: sessions, messages, cron_jobs, meta (+FTS5)
├── kanban.db               kanban board
├── memory/MEMORY.md        agent memory
├── memory/USER.md          user profile
├── skills/<name>/SKILL.md  skills
├── sessions/*.todos.json   per-session todo lists
├── images/  audio/         generated artifacts
├── sandboxes/              execute_code scripts
├── approvals.json          persisted "always" approval grants
├── media-cache/            content-addressed messaging attachments
├── pairing/                DM pairing stores ({platform}-pending/approved.json)
├── shell-hooks-allowlist.json   hook consent records
└── checkpoints/store/      shared shadow git store (per-project refs/indexes)
```

<a id="known-differences"></a>

## Known differences

- Approval UX is a terminal y/N prompt on the CLI (hermes has richer
  platform-specific flows); the gateway exposes run approvals over HTTP with
  once/session/always/deny semantics.  Chat-completions requests have no run
  context and auto-deny confirm-tier commands by design.  Smart-approval
  (LLM guardian) and cron approval modes are ported; unattended runs fail
  closed unless `cron_mode = "approve"`.
- Browser supervisor launches a local Chrome/Chromium directly; hermes drives
  an external `agent-browser` daemon. The Camofox REST backend is ported
  (incl. managed persistence, via `CAMOFOX_MANAGED_PERSISTENCE` instead of
  hermes' config.yaml knob); cloud browser providers (Browserbase,
  Browser Use, Firecrawl) are ported in `browser/cloud.rs`.
- The gateway implements the api_server platform subset (profile
  multiplexing under `/p/<profile>/...` IS ported — see the feature table).
- `/api/model/options` inventory: hermes' credential-pool rows and Nous
  free-tier gating are not ported (the pool store itself IS ported lean
  — see the credential-pool row — but the picker keeps one row per
  provider); the multi-provider row set, picker hints, featured models
  and pricing are ported (P168, see the HTTP gateway row).
- Compression uses a char/4 token estimate instead of a tokenizer.
- `patch` fuzzy chain implements all 9 hermes strategies; similarity is an
  LCS-based ratio (difflib.SequenceMatcher stand-in), so edge-case thresholds
  can differ slightly from CPython's matcher.
- Environments cover local/docker/ssh; hermes' modal/daytona/vercel backends
  and their credential flows are not ported.
- Checkpoints skip hermes' legacy pre-v2 store migration (fresh stores only)
  and the volume-identity orphan heuristic (workdir existence is used).
- Secrets: `secrets sync` dry-runs compare against the env the startup hook
  already applied (hermes behaves the same); the Bitwarden `token` rotation
  subcommand IS ported (`secrets bitwarden token [--access-token] [--no-verify]`
  — validates the new token against Bitwarden before storing, so a bad paste
  never bricks the working token, and drops the old token-fingerprint caches);
  the bws auto-install Windows asset path is untested.
- Computer-use: driver payloads (SOM screenshot b64, AX trees) pass through
  without the hermes PNG post-processing / multimodal eviction layer; the macOS TCC grant flow is exposed to the desktop (`/api/tools/computer-use/permissions/grant` spawns `cua-driver permissions grant` as a polled background action, macOS-only); the embedded-daemon socket mode is not
  ported; `install` shells out to the upstream trycua installer script.
- Plugins: ulnclaw plugins are subprocesses speaking the hermes shell-hook JSON protocol
  (directory plugins + `[hooks]` config), not Python imports
  - the core fires every hook event hermes v2026.8.3 emits at runtime (13 of 23 — the other 10 are
    catalog-only in hermes itself)
  - pre_verify has no ulnclaw verify loop to attach to
  - the `ulnclaw kanban` engine now fires the kanban_task_claimed/completed/blocked hooks on
    claim/done/block, but the agent-side kanban_* tools now ride the same KanbanStore engine (P119
    unified the previously separate tables):
    - P122 ported the dispatcher tick (`kanban dispatch` CLI + `POST /api/kanban/dispatch`:
        stale-claim reclaim with live-pid extension, parent-done todo→ready promotion, ready-task worker
        spawn via detached `ulnclaw run` with ULNCLAW_KANBAN_TASK, live concurrency cap, spawn-failure
        auto-block after 2 tries)
    - P123 added the embedded gateway ticker (`[kanban] dispatch_in_gateway / dispatch_interval_secs /
        max_spawn`, default on/60 s/2) and the hermes kanban-stop nudge (one-shot workers that end
        without kanban_complete/block are re-prompted up to 2x, `ULNCLAW_KANBAN_STOP_NUDGE=0` disables)
    - P124 added per-task git-worktree isolation (`[kanban] worktrees`, default on: each dispatched
        worker runs in `<repo>/.worktrees/<task-id>` on branch `kanban/<task-id>`, reused across
        respawns; `ulnclaw kanban gc` removes trees of done/archived tasks, branches kept)
    - P125 ported the hermes kanban swarm (`hermes_cli/kanban_swarm.py`): `ulnclaw kanban swarm <goal>
        --worker ASSIGNEE:TITLE [--worker ...] --verifier ASSIGNEE --synthesizer ASSIGNEE [--json]`
        builds a workers→verifier→synthesizer graph — a root blackboard/audit task (created done), N
        ready workers briefed with the swarm protocol, a verifier linked to every worker, and a
        synthesizer linked to the verifier; the topology is posted as a `blackboard` comment + `swarm`
        event, and the existing dispatcher promotes verifier/synthesizer as their parents complete
        (`recompute_ready`)
    - P127 completed the swarm surface: worker skills passthrough (`--worker
        ASSIGNEE:TITLE:skill,skill`, verifier pinned to `requesting-code-review`, synthesizer to
        `humanizer` — hermes-verbatim), task-level `skills`/`max_runtime_seconds`/ `idempotency_key`
        columns (additive migrations; `kanban create --skill X --max-runtime N --idempotency-key K`,
        same fields on the gateway create API), idempotent swarm recovery (same key ⇒ topology rebuilt
        from the root blackboard, no duplicate graph), dispatcher `reap_timed_out` (SIGTERM + 5 s grace
        + SIGKILL, task back to ready with a `timed_out` event) and force-loaded skills inlined into the
        spawned worker's founding prompt (hermes passes `--skills` pairs)
    - P128 ported the triage pipeline (`hermes_cli/kanban_specify.py` + `kanban_decompose.py`):
        `kanban create --triage` parks an idea in a new `triage` column, `kanban specify` fleshes it
        into a Goal/Approach/ Acceptance-criteria spec via `auxiliary.triage_specifier` and promotes
        triage→todo, `kanban decompose` fans it into a 2-6 child dependency graph routed over the
        profile roster (`[kanban] orchestrator_profile / default_assignee / auto_promote_children`; root
        stays alive as the child-of-every-child wake-up card, Kahn cycle-checked, fail-soft outcomes for
        --all sweeps), and `kanban diagnostics` ports the `kanban_diagnostics.py` rule engine
        (hallucinated card ids, phantom prose refs, repeated spawn failures, worker crash-loops,
        stuck-blocked > 24 h, block/unblock cycling, stranded-in-ready, triage-without-aux) with hermes'
        thresholds and severity ordering
    - P129 completed the remaining hermes kanban CLI surfaces: schedule/promote (parent-gated, --force
        override)/reclaim/reassign (--reclaim)/edit/set-model, the attachments CLI
        (attach/attachments/attach-rm with stable ids), tail --follow event streaming, per-board status
        stats, and boards rename/set-workdir
    - P130 added the board-wide `kanban watch` live event stream (assignee/kind filters, hermes watch
        backend), hermes `board_stats` semantics on `kanban stats` (per-assignee counts + oldest-ready
        age + `--json`) and `kanban dispatch --json`
    - P131 ported the gateway notification substrate: the `kanban_notify_subs` table (task × platform
        × chat × thread primary key, caught-up cursor snapshot on subscribe, chat_type/profile/metadata
        self-heal), the `kanban notify-subscribe / notify-list / notify-unsubscribe` CLI surface,
        `unseen_events_for_sub` + `advance_notify_cursor` building blocks for the gateway notifier, and
        `kanban log [--tail N]` which prints a task's worker log from `<home>/kanban/worker-logs/` with
        hermes' partial-line-safe tail
    - P132 added the `task_runs` attempt-history table (hermes `Run` lifecycle: a run opens on claim
        carrying claim lock/TTL + runtime cap, heartbeats and the spawned worker pid are mirrored onto
        it, and it closes with hermes outcome semantics — completed / blocked / reclaimed / timed_out on
        done/block/reclaim/stale-release/timeout, plus instant synthesized runs for CLI completes on
        never-claimed tasks and dispatcher spawn failures; re-claim recovers stale active runs as
        `reclaimed`), the `kanban runs [--json] [--state-type status|outcome --state-name V]` CLI with
        hermes' table format, and `latest_run` / `latest_summary` store helpers
    - P133 wired the gateway dispatcher's auto-decompose path (hermes `_auto_decompose_tick`): each
        tick re-reads `[kanban] auto_decompose` (default on) / `auto_decompose_per_tick` (default 3)
        live from config so flipping the toggle stops a runaway fan-out on the next tick without a
        gateway restart (hermes #49638 fail-safe semantics — config read errors disable the pass), then
        decomposes up to N triage tasks via the auxiliary LLM before the dispatch fan-out, logging
        successes at info and no-op skips at debug
    - P134 completed the remaining hermes kanban CLI surface: `kanban context` (full
        `build_worker_context` port — capped body/attachments, prior-attempt run summaries with
        metadata, done-parent handoffs with relative-age staleness hints, assignee cross-task role
        history, capped comment thread; the `kanban_show` tool now returns the same `worker_context` so
        spawned workers read it without extra round-trips), `kanban repair` (integrity_check +
        content-addressed quarantine + index-scoped REINDEX auto-repair, fail-closed otherwise), `kanban
        assignees` (config roster merged with board assignees, per-status counts), `kanban daemon`
        (hermes-deprecated stub pointing at the gateway, `--force` keeps the standalone loop), and
        `ls`/`new` visible aliases
    - P135 wired notification delivery: the gateway runs a kanban notifier loop (hermes
        kanban_watchers notifier, 5 s tick) that polls `kanban_notify_subs`, claims unseen terminal
        events (completed/blocked/gave_up/crashed/timed_out/status, with archived/unblocked
        claimed-but-silent so they can't wedge later events), renders hermes' message formats (✔ done +
        handoff first line, ⏸ blocked + reason, ⏱ timed_out, ✖ crashed/gave_up, 🔄 status, @assignee +
        [board] tags) and sends them through the registered platform sender, advancing the per-sub
        cursor after delivery; subscriptions survive crash/retry cycles and are removed only when the
        task reaches done/archived (cursor handles dedup). Scoped vs hermes: no per-profile adapter
        ownership (single shared store), no thread routing or dead-chat drop (PlatformSender exposes no
        failure channel), sends assumed delivered
    - P136 ported the unified failure accounting + circuit breaker (hermes `_record_task_failure`):
        tasks grow `consecutive_failures` / `last_failure_error` / `max_retries` columns, every spawn
        failure and timed-out attempt consumes the retry budget, hitting the threshold (per-task
        `max_retries` > dispatcher limit > default 2) flips ready→blocked with a `gave_up` event
        (failures / effective_limit / limit_source / trigger_outcome payload), and the counter resets on
        completion and deliberate unblock (hermes fresh-start policy). CLI: `kanban create --max-retries
        N` (>= 1 validated, matching hermes) and the gateway create API accepts the same field
    - P137 added the dispatcher's worker-health detection (hermes `detect_crashed_workers` +
        `detect_stale_running`): every tick immediately reclaims running tasks whose worker pid died (30
        s launch-grace, `ULNCLAW_KANBAN_CRASH_GRACE_SECONDS` override; `crashed` event, run closed with
        outcome `crashed`, failure counted against the breaker) and running tasks past `[kanban]
        stale_timeout_seconds` (hermes `dispatch_stale_timeout_seconds`, default 14400, 0 disables,
        re-read live in the gateway loop) whose heartbeat is missing or older than an hour (worker
        SIGTERM→SIGKILL, `stale` event, run outcome `stale`, deliberately NOT counted as a failure —
        hermes policy); both surface in `DispatchResult.stale` / `.crashed`
    - P138 hardened the embedded dispatcher (hermes gateway loop): an exclusive `flock` singleton lock
        (`<home>/kanban/dispatcher.lock`) guarantees exactly one dispatching gateway per machine — a
        second gateway logs the contention and keeps serving HTTP without dispatching — plus
        stuck-dispatcher telemetry (warn when the ready queue stays non-empty for 6 consecutive ticks
        with zero spawns, throttled to 300 s)
    - P139 ported hermes' per-task workspaces: tasks gain `workspace_kind` (`scratch` default /
        `worktree` / `dir`), `workspace_path` and `branch_name` columns; `kanban create --workspace
        scratch|worktree|worktree:<path>|dir:<path>` and `--branch <name>` (worktree-only, hermes
        validation text) with the gateway create API accepting the same fields; the dispatcher resolves
        the workspace BEFORE spawn (hermes `resolve_workspace` / `_resolve_worktree_workspace`): scratch
        dirs under `<home>/kanban/workspaces/<id>`, `dir:` paths must be absolute (confused-deputy
        guard, hermes threat model), worktrees anchor on the board `default_workdir` (dispatcher-CWD
        fallback keeps the pre-
    - P139 behaviour; hermes raises instead) and materialize `<repo>/.worktrees/<task-id>` on branch
        `wt/<task-id>` (or `--branch`), reusing occupied sibling checkouts via a fresh tree; the
        resolved path + branch are persisted on the task row so retries reuse them, resolution errors
        count as `workspace:` spawn failures against the circuit breaker, `kanban claim` resolves +
        prints the workspace (hermes `_cmd_claim`), `[kanban] worktrees=true` keeps its meaning for
        tasks created without `--workspace`, and decompose children inherit the root's workspace
        kind/path (worktree children always get their own tree, hermes sibling policy)
    - P140 added the respawn guard + duration syntax: `kanban create --max-runtime` accepts
        `30s`/`5m`/`2h`/`1d` as well as bare seconds (hermes `_parse_duration`), and the dispatcher
        defers ready tasks that cannot benefit from an immediate retry (hermes `check_respawn_guard`) —
        `rate_limit_cooldown` (latest run ended `rate_limited` inside
        `ULNCLAW_KANBAN_RATE_LIMIT_COOLDOWN_SECONDS`, default 300, 0 disables), `blocker_auth` (last
        failure matches the quota/auth pattern), `recent_success` (completed run within 1 h without a
        deliberate re-queue) and `active_pr` (GitHub PR URL in a 24 h comment window); guarded tasks
        stay ready, each deferral emits a `respawn_guarded` event, and the gateway dispatch API reports
        them
    - P141 ported wake routing: tasks gain a `session_id` column stamped by the agent `kanban_create`
        tool (and accepted by the gateway create API), and when a subscribed task reaches a
        wake-eligible terminal event (`completed` / `gave_up` / `crashed` / `timed_out` / `blocked` —
        hermes `_WAKE_KINDS`) the notifier resumes the creator session by self-POSTing the hermes-format
        wake message (`[kanban] Task <id> <status>. …`) to the gateway's own `/v1/chat/completions` with
        `X-Ulnclaw-Session-Id` (hermes `_self_post_chat_completion`: loopback for wildcard binds, bearer
        key when configured, 600 s turn ceiling, 2/5/10 s backoff on 429 / transient errors, fail-fast
        on other HTTP errors); the wake runs best-effort and detached after the text ping so it cannot
        stall other subscriptions
    - P142 ported typed block kinds (hermes `block_task(kind=…)`): `kanban block --kind dependency`
        parks the task in `todo` (`dependency_wait` event) where parent gating + `recompute_ready`
        promote it automatically once the parents finish — no human, no cron; `needs_input` /
        `capability` / `transient` / untyped land in `blocked` with `block_kind` + `block_recurrences`
        persisted, and the unblock-loop breaker routes a task to `triage` (`block_loop_detected`) when
        the same cause re-blocks `BLOCK_RECURRENCE_LIMIT` (2) times after unblocks — recurrences survive
        unblock deliberately and reset only on completion; `unblock_task` now re-gates on open parents
        (blocked → `todo` while parents remain) matching hermes' invariant fix; the agent `kanban_block`
        tool and the gateway block API accept the kind
    - P143 completed the lifecycle CLI surface: bulk `kanban
        done/block/schedule/unblock/promote/archive` (multiple ids, hermes `task_ids` + `--ids`),
        `kanban done --summary/--metadata` storing the structured handoff (full summary + JSON facts) on
        the closing run while the `completed` event carries the first summary line (400-char cap) for
        notifiers, `kanban archive --rm` purging already-archived tasks with all related rows (guard:
        only archived tasks delete), `kanban unblock --reason` commenting before unblocking, `kanban
        promote --dry-run/--json` backed by a mutation-free `validate_promote`, `kanban watch --tenant`,
        and archive now closes an in-flight run as reclaimed + immediately promotes children whose
        archived parent was the last gate (`recompute_ready` treats archived parents as done, hermes
        semantics).
    - P144 added completion recovery: `kanban edit --result/--summary/--metadata` rewrites a done
        task's handoff (result text + latest completed run's summary/metadata, synthesizing a run row
        when none exists; emits `edited`), the terminal kanban tool gained `summary` + `metadata` so
        workers hand off structured facts, and blocking now writes a `BLOCKED: <reason>` comment before
        the state change (hermes `_cmd_block` parity).
    - P145 extended `recompute_ready` to the blocked column: a blocked task whose parents are all
        done/archived auto-recovers to ready (preserving `consecutive_failures`, `promoted` event)
        unless the block is sticky — latest `blocked`/`unblocked` event is a worker/operator `blocked`
        (#28712) — or the failure count already reached the effective limit (per-task `max_retries` >
        dispatcher `failure_limit` > default 2, #35072); the dispatcher passes its configured limit
        through `dispatch_once`.
    - P146 added the non-spawnable gate + health probe: `dispatch_once` takes the configured profile
        set and parks ready tasks whose assignee is not a configured profile in `skipped_nonspawnable`
        (claim-pulled control-plane lanes that must never auto-spawn — hermes
        #kanban-dispatcher-crash-loop), and the gateway dispatcher's stuck warning now consults
        `has_spawnable_ready` so a ready queue full of lanes reads as "correctly idle", firing only when
        spawnable work (unassigned or known-profile tasks) actually waits.
    - P147 ported completion artifacts: `kanban done --artifact <path>` (repeatable), the agent
        `kanban_done` tool (`artifacts` array) and the gateway complete API stage files living inside a
        managed scratch workspace into `<home>/kanban/attachments/<task>/` before any cleanup can erase
        them (25 MiB cap, missing/oversized declarations fail the completion with rollback), record them
        as `artifact` attachments with `attached` events, merge absolute deliverable paths mentioned in
        summary/result prose, and carry the final paths on the `completed` event + run metadata (hermes
        `kanban_complete( artifacts=[...])`, `_persist_scratch_completion_artifacts`,
        `_merge_completion_prose_artifacts`).
    - P148 ported the review column: `review` joins the status set (🔍); workers call `kanban review
        <id> [--reason]` / the `kanban_review` tool after opening a PR (running → review, worker run
        closed, `review_requested` event); `dispatch_once` grows a review loop sharing the max_spawn cap
        — unassigned review tasks land in `skipped_unassigned`, unknown assignees in
        `skipped_nonspawnable`, claimed review tasks open a fresh run without re-gating parents
        (`claim_review_task`), and the `sdlc-review` skill is force-loaded when installed under
        `<home>/skills/`; `has_spawnable_review` joins the gateway health probe.
    - P149 added the per-profile concurrency cap: `[kanban] max_in_progress_per_profile` (hermes
        #21582) refuses to spawn for an assignee already at its in-flight limit even with global
        headroom — counts seed from the running column each tick and count would-be spawns in dry runs;
        skipped tasks land in `skipped_per_profile_capped` (CLI line + dispatch JSON).
    - P150 ported the anti-hallucination completion gate: `kanban done --created-card <id>`
        (repeatable; also the agent `created_cards` array and the gateway complete API) verifies each
        claimed card — it must exist AND be created by the worker's profile, created under the worker's
        task id, or linked as the worker's child. Phantom ids emit `completion_blocked_hallucination`
        and block the completion without mutating anything (hermes `HallucinatedCardsError`); verified
        ids ride on the `completed` event, and unresolved `t_<hex>` references in summary/result prose
        are flagged after a successful completion via `suspected_hallucinated_references` (advisory,
        hermes `_scan_prose_for_phantom_ids`).
    - P151 added the per-tick dispatch lock (#35240): every `dispatch_once` tick runs under a
        non-blocking `flock` on `<kanban.db>.dispatch.lock`; a losing dispatcher (e.g. an orphan escaped
        a service restart) returns `skipped_locked = true` with zero DB writes and retries next interval
        — surfaced in the CLI (`dispatch: skipped …`) and the dispatch API JSON.
    - P152 added worker log rotation: per-task logs under `kanban/worker-logs/` rotate at `[kanban]
        worker_log_rotate_bytes` (default 2 MiB), keep one `.log.1` backup generation, and append within
        a generation so re-spawned attempts no longer truncate earlier output (hermes
        `worker_log_rotation_config`).
    - P153 closed the stale-worker race: dispatch now claims BEFORE spawning (hermes order) so the run
        row exists at spawn time; workers carry `ULNCLAW_KANBAN_RUN_ID` (hermes `HERMES_KANBAN_RUN_ID`)
        and their completions/blocks pass it as `expected_run_id` — an atomic `current_run_id` guard
        refuses a reclaimed attempt instead of clobbering the fresh one (CLI Done/ Block, the
        `kanban_complete`/`kanban_block` tools and the gateway complete/block APIs all thread it).
        Spawn/workspace failures of a claimed attempt now end the run, release the claim back to ready
        and count the failure (hermes `_record_spawn_failure`).
    - P154 added the `[kanban] max_in_progress` global concurrency cap (#33488): a tick whose board
        already runs at/above the cap returns early (the backlog stays ready, nothing is bucketed),
        otherwise the effective spawn cap clamps to the tighter of `max_spawn` and `max_in_progress` so
        the running column fills exactly to the cap — slow workers (local LLMs, resource-constrained
        hosts) drain before piled-up tasks time out.
    - P154 also ported the one-time scratch-workspace tip (hermes `_maybe_emit_scratch_tip`): the
        first scratch workspace materialized across the whole install logs a warning that scratch output
        is ephemeral (deleted when the task completes), records a `tip_scratch_workspace` event on the
        task, and touches the `.scratch_tip_shown` sentinel so the tip never repeats; worktree/dir
        workspaces are preserved by design and never tip.
    - P156 added kanban goal-mode workers (hermes `create --goal` / `--goal-max-turns`): a goal card
        spawns a worker that wraps its run in the Ralph-style judge loop IN THE SAME SESSION — after
        every turn the auxiliary judge (`[auxiliary.goal_judge]`) evaluates the latest response against
        the card's title+body; `continue` feeds a continuation prompt, `done` issues one explicit
        kanban_complete nudge and then blocks the card as judged-done-never-finalized, and an exhausted
        turn budget (or a reclaimed/archived task) ends in a sticky block for human review. Goal-card
        completions pass the #38367 judge gate on the CLI `kanban done` and the `kanban_complete` tool:
        a verdict other than `done` rejects the completion with the judge's reason (fail-open when no
        judge is configured or reachable; the gateway `/api/kanban` complete endpoint is deliberately
        not gated).
    - P157 added `create --initial-status running|blocked` (hermes `VALID_INITIAL_STATUSES`):
        `blocked` parks the card for human-ops review until unblocked — it wins over `--triage` — while
        `running` keeps the default flow (CLI, gateway create API).
    - P158 added the workflow-template hooks (hermes `workflow_template_id` / `current_step_key` task
        columns): external workflow engines stamp cards at create time (gateway create API carries both
        fields) and query them back via `kanban list --workflow-template-id` (SQL-level filter; gateway
        list API takes the same query param). The template engine itself lives outside the board in both
        projects.
    - P159 wired per-task model/provider overrides to the workers (hermes `model_override` /
        `provider_override`): a new `provider` task column, `kanban create --provider` / gateway create
        body, `kanban set-model [--provider P]` (provider clears together with the model;
        provider-without-model is rejected — hermes contract), global `-m/--model` + `--provider` CLI
        flags that win over config and profile (the flags the spawned `ulnclaw run` worker carries), and
        `dispatch_spawn` now passes `--model` / `--provider` from the card.
    - P160 ported hermes' first-class project registry (`projects_db` + `project` CLI): a per-profile
        `projects.db` with named multi-folder workspaces (`ulnclaw project
        create/list/show/add-folder/remove-folder/rename/set-primary/use/ archive/restore/bind-board` —
        slug uniqueness, primary-folder repointing, active-project pointer; `bind-board` also mirrors
        the primary repo into the bound board's `default_workdir`), plus `kanban create --project
        <id|slug>` (CLI + gateway create body): the project resolves at create time and anchors the
        worktree under the project's primary repo (`<repo>/.worktrees/<task-id>`) with a deterministic
        `<slug>/<task-id>[-<title-slug>]` branch, stored in a new `tasks.project_id` column;
        unresolvable links drop silently (hermes drop-dangling semantics).
    - P161 completed the projects subsystem with the repo-discovery scanner hermes never shipped (its
        `discovered_repos` cache table exists but only the Electron desktop walks the disk, in
        TypeScript): `ulnclaw project scan [--root PATH ...] [--max-depth N]` finds git checkouts
        (`.git` directory or worktree file; hidden + skip-listed dirs pruned, symlinks never followed,
        nested checkouts included) and records them with replace semantics + the `cli-scan:v1` policy
        key; `project repos [--clear]` lists/clears the cache.
    - P162 exposed the registry to the gateway for desktop surfaces: `/api/projects` CRUD (`PATCH`
        board binding mirrors the primary repo into the board's `default_workdir` exactly like CLI
        `bind-board`; folders add/remove, set-primary, archive/restore, hard delete, active pointer)
        plus `/api/projects/scan|repos` discovery.
    - P164 linked sessions to projects: `/api/sessions` rows (list + get) carry a `project` slug
        resolved by longest-prefix cwd match against `projects.db` folders (archived projects excluded;
        a missing store degrades to `project: null`), and the desktop sidebar renders it as a badge —
        the hermes desktop session-grouping-by-project contract.
    - Remaining deliberate kanban divergences: dispatch-time `default_assignee` application (ulnclaw
        spawns unassigned tasks on the default profile instead of skipping them).
- Messaging: image attachments are injected natively into the user turn
  as multimodal content parts (P226, hermes media-injection parity):
  `image/*` files ≤ 8 MB are base64-encoded into `data:` URLs and ride
  the turn on OpenAI-compatible (`image_url` parts) and Anthropic
  (base64 image blocks) providers; every other medium — and images when
  `[messaging] multimodal_injection = false` — stays a cached path
  reference the agent inspects with vision_analyze/video_analyze/read_file.
  Injected images are ephemeral to the live turn (session history keeps
  the text turn). Voice notes ARE transcribed via the
  `[stt]` pipeline, but the built-in `local` faster-whisper provider needs
  `stt.local.command` or a cloud provider in the static binary; Telegram
  clarify + exec-approval inline keyboards ARE ported (P225); all twenty-two of hermes' platform adapters
  are ported (WhatsApp Cloud + MS-Graph ingress, the generic webhook
  platform, Twilio SMS, Microsoft Teams, LINE, Google Chat, Raft, and
  A2A ride gateway webhook routes); the pairing
  flow pairs per sender id like hermes, but the configured allowlist stays
  chat/channel-id based (auth gate = allowlist OR approved pairing).
- OAuth/sync: the flows are provider-agnostic (any RFC 8628 endpoint) rather
  than bound to the Nous Portal; sync moves skill bundles only (no org
  proposal/approval workflow, no subscription gating).

<a id="http-route-parity-appendix"></a>

## HTTP route parity appendix (P339)

Route-level audit of hermes `web_server.py` (v2026.8.3, 112 routes) against
the ulnclaw gateway router (217 routes as of v0.7.0-rc13: the 198 core
routes of P351 plus the 19-route plugin REST namespace below). Every hermes
route is either covered — possibly under a different path — or resolved by
design; nothing is silently missing.

### Covered under ulnclaw paths

| hermes route | ulnclaw equivalent |
|---|---|
| `/api/files`, `/api/files/read` | `/api/fs/list`, `/api/fs/read-text` (+ `write-text`, `read-data-url`) |
| `/api/files/download` | `/api/fs/download` (P335, `?token=` query auth) |
| `/api/files/mkdir` | `/api/fs/mkdir` (P335) |
| `/api/files/upload`, `/api/files/upload-stream`, `/api/chat/image-upload` | `/api/uploads` (content-addressed media cache) |
| `/api/hermes/update/check` | `/api/update/check` (P324) |
| `/api/hermes/update` | `ulnclaw update` CLI (terminal progress; the gateway never self-mutates) |
| `/api/ops/doctor` | `/api/doctor` |
| `/api/ops/backup` | `/api/backups` (+ `/api/backups/:id/restore`, `/api/backups/prune`) |
| `/api/portal` | `/api/oauth/status` (P334, lean posture read) |
| `/api/providers/validate` | `/api/providers/custom-endpoints/validate` (P333) |
| `/api/status`, `/api/system/stats` | `/api/health`, `/api/system` (P334) |
| `/api/analytics/usage` | `/api/usage` + `/api/analytics/models` + `/api/insights` |
| `/api/learning/node` | `/api/learning/node` (GET/PUT/DELETE) |
| `/api/sessions/import` | same path (P348 — portable JSON round-trip via `GET /api/sessions/:id/export?format=json`) |
| `/api/providers/oauth*` | same paths (P350 — lean device-code catalog: list/start/poll/submit/cancel/disconnect over the `[oauth]` flow) |
| `/api/dashboard/plugins*`, `/api/dashboard/agent-plugins*`, `/api/dashboard/plugin-providers` | same paths (P351 — lean plugin hub: active list + rescan, merged hub payload over a curated `plugin-hub/index.json` + local-dir candidates with install/update/remove/enable/disable, memory/context provider selection persisted to `[plugins]`, `dashboard.hidden_plugins` visibility toggles) |
| `/api/plugins/kanban/*` (hermes mounts the kanban plugin's FastAPI router into the web server) | same paths (v0.7.0-rc13 — the desktop kanban plugin's REST namespace served natively over `KanbanStore`: `board` with hermes column order + card rollups, boards CRUD, task detail … [details](#api-plugins-kanban-hermes-mounts-the-kanban-plugin-s-fastapi-router-into-the-web-server) |
| `/api/media` | `/api/media` (P338) |
| `/api/audio/speak`, `/api/audio/speak-stream`, `/api/audio/elevenlabs/voices` | same paths (P344 — lean openai/elevenlabs `[tts]` providers; P349 adds the free `edge` provider; `speak-stream` is the streaming-TTS WebSocket — text in, 24 kHz int16 PCM frames out over the chunked openai/elevenlabs APIs with sentence cutting, idle flush and barge-in; providers without a chunked API answer one `fallback` frame and the client demotes to `/api/audio/speak`) |
| `/api/messaging/platforms` | `/api/messaging/platforms` (+ `PUT :id`, `POST :id/test`; P337) |
| `/api/webhooks`, `/api/webhooks/{name}`, `/api/webhooks/enable`, `/api/webhooks/{name}/enabled` | config-driven: webhook platforms are declared in `[messaging.*]` config; ingress rides the open `/webhooks/*` routes |
| `/api/model/auxiliary` | config-driven: `[auxiliary.<task>]` overrides in config.toml (no dashboard surface) |

#### Detailed notes

<a id="api-plugins-kanban-hermes-mounts-the-kanban-plugin-s-fastapi-router-into-the-web-server"></a>

##### `/api/plugins/kanban/*` (hermes mounts the kanban plugin's FastAPI router into the web server)

- same paths (v0.7.0-rc13 — the desktop kanban plugin's REST namespace served natively over
  `KanbanStore`: `board` with hermes column order + card rollups, boards CRUD, task detail
  (comments/events/attachments/links/runs/diagnostics), PATCH status semantics (`done`→complete,
  `blocked`→block, `scheduled`→schedule, `ready`→unblock, `archived`→archive, `running`→400), bulk
  ops, task log tail, multipart attachments, reassign/reclaim, aux-model estimate with a 20 s timeout
  guard (`{ok,est_tokens,complexity,rationale,model}` contract, never an HTTP error), profiles roster
  (config assignees ∪ stored descriptions) + PATCH, projects, orchestration GET/PUT over `[kanban.*]`
  config, dispatch delegation, assignees. Hermes mounts Python plugin routers
- ulnclaw serves the identical contract from the engine. The achievements plugin is not ported —
  only the kanban plugin ships in the vendored desktop)


### Resolved by design (won't port)

- **Gateway lifecycle** (`/api/gateway/start|stop|restart|drain`) — the
  desktop shell supervises the gateway child (`ULNCLAW_DESKTOP=1`), and the
  CLI or a service manager supervises standalone gateways; a gateway
  restarting itself on a dashboard request is exactly the failure mode the
  single-instance guard (P155) prevents. Handover is
  `ulnclaw gateway --replace`.
- **Dashboard actions** (`/api/actions/{name}/status`) — hermes polls this
  for dashboard-driven self-update/restart actions; ulnclaw runs updates
  from the CLI with terminal progress, so there is no long-running
  dashboard action to poll.
- **Memory providers** (`/api/memory/provider`,
  `/api/memory/providers/{name}/config|setup`) — ulnclaw memory is the
  file-based MEMORY.md/USER.md pair with the `memory` tool, census and
  targeted reset over `/api/memory`; pluggable third-party memory backends
  are out of scope.
- **Onboarding wizards** (`/api/messaging/telegram/onboarding/*`,
  `/api/messaging/whatsapp/onboarding/*`) — setup is CLI-side
  (`ulnclaw setup`, `ulnclaw whatsapp-cloud`; P267/P271); WhatsApp runs
  through a self-managed Baileys bridge whose pairing is its own startup
  flow, inspected by `ulnclaw whatsapp status` (P272).
- **Plugin static assets** (`/dashboard-plugins/{name}/{path}`) —
  per-plugin JS/CSS hosting for third-party dashboard extensions is not
  ported; the ulnclaw desktop Plugins view is native, and hub plugins
  contribute hooks/tools rather than browser assets. The marketplace/install
  pipeline itself is covered at the hermes paths (P351).
- **Provider OAuth** (`/api/providers/oauth*`) — covered at the same
  paths (P350): a lean device-code catalog over the service-agnostic
  `[oauth]` flow with start/poll/submit/cancel/disconnect sessions.
  Vendor-specific marketplace handshakes (Anthropic redirect, Codex,
  Nous portal) stay out of scope.
- **Model ensembles** (`/api/model/moa`) — Mixture-of-Agents ensemble
  routing is not ported; the single-gateway-model plus per-session model
  lock design supersedes it.
- **TTS providers** — the wire surface is covered (see table; P344)
  and edge/openai/elevenlabs are all ported; P349 implemented the free
  `edge` provider (Microsoft read-aloud websocket with the Sec-MS-GEC
  7.x DRM token exchange — no API key). Local models
  (piper/neutts/kittentts) stay out of scope.
- **Ops extras** (`/api/ops/checkpoints*`, `/api/ops/config-migrate`,
  `/api/ops/debug-share`, `/api/ops/import*`,
  `/api/ops/backup/download`) — checkpoints are superseded by
  `/api/backups` plus session fork; config migration is a one-time hermes
  upgrade path; debug bundles stay local (`ulnclaw doctor`); session
  import has a dedicated endpoint now (P348); backup download is superseded
  by the restore endpoint.
- **Curator** (`/api/curator/run`, `/api/curator/paused`) — learning
  curation is manual: node edit/archive over `/api/learning/node` and the
  desktop Learning view.

<a id="completion-status"></a>

## Completion status

The agent core is at parity with hermes-agent v2026.8.3 — the following
surfaces are all ported, completing the v2026.8.3 parity surface:

- every core tool;
- the full `sessions` surface (`list/show/search/export/recap/recover/prune/archive/stats/delete/rename/optimize/repair/browse`);
- startup resume (`--resume`/`--continue` with one-session-per-conversation continuity);
- the CDP browser client + attach layer + Camofox backend + cloud browser providers (Browserbase / Browser Use / Firecrawl, P264);
- the HTTP gateway (including the `/v1/browser/*` live-endpoint control and profile multiplexing);
- skills/bundles/memory/goals/checkpoints/cron/insights/doctor/pets;
- external secret sources (command helper / Bitwarden / 1Password);
- computer-use via cua-driver;
- the subprocess plugin system;
- messaging platform gateways (Telegram/Discord/Slack/Signal/Weixin/QQ/Yuanbao/Email/Mattermost/Matrix/DingTalk/WeCom/Feishu/HomeAssistant/SMS/WhatsApp/IRC/ntfy/SimpleX/Teams/LINE/Google Chat/Buzz/Photon/Raft/A2A);
- OAuth device-flow login + skill sync;
- the `send_message` cross-channel tool + channel directory (P259);
- the MCP channel bridge `ulnclaw mcp serve` (P260);
- the ACP editor adapter `ulnclaw acp` (P261);
- the parallel batch runner `ulnclaw batch` (P262);
- the scripted-messaging CLI `ulnclaw send` (P263);
- Slack native slashes + the `ulnclaw slack manifest` app-manifest generator (P265);
- gateway monitoring + OTLP health/diagnostics export (`[monitoring]`, P266);
- the interactive setup wizard `ulnclaw setup` (P267);
- the interactive model switcher `ulnclaw model` (P268);
- the desktop launcher `ulnclaw gui` + `login`/`logout`/`learning`/`memory-graph` aliases (P269);
- dynamic webhook subscriptions `ulnclaw webhook` (P270);
- the WhatsApp Cloud setup wizard `ulnclaw whatsapp-cloud` (P271);
- the WhatsApp bridge diagnostics `ulnclaw whatsapp status` (P272);
- the desktop GUI (`desktop-electron/`, a faithful port of hermes' Electron desktop) and the rest of the CLI.

The `sessions` surface intentionally omits only `optimize-storage`
(ulnclaw was built on the compact external-content FTS layout from day
one — there is no legacy layout to migrate); the `-c <session-name>`
title lookup was ported with P232.

Deliberately not ported (hermes surfaces outside the local-agent scope):

- the Electron desktop app itself — retracted in v0.7.0: ulnclaw now ships it directly (`desktop-electron/` is a faithful port of hermes desktop v2026.8.3, backed by the ulnclaw gateway; the earlier Tauri shell, its scoped-twin surface list and the desktop cosmetic/Electron-only exclusions are retired with it — the faithful port includes them all);
- Python plugin imports/entry-point packages and the provider registrations they carry (ulnclaw's plugin system uses the shell-hook wire protocol + directory plugins with the git install/update/remove lifecycle instead);
- Nous-Portal-specific subscription gating and org proposal approval workflows (OAuth/sync are provider-agnostic);
- the xAI credential-pool proxy adapter (the pool store is now ported lean — see the credential-pool row — but the xAI adapter itself stays unported);
- the SWE-bench-style datagen scripts (`mini_swe_runner.py`, `trajectory_compressor.py` — `ulnclaw batch` covers the parallel-run + trajectory pipeline they feed);
- the cua-driver embedded-daemon/socket mode and context-level screenshot eviction (P228 ported the vision post-processing half — client-side longest-edge enforcement of `max_image_dimension` on returned screenshots; the eviction half has no reference implementation in the v2026.8.3 checkout).

Additional CLI surfaces deliberately not ported (documented as
differences, audited against v2026.8.3):

- `hermes console` — the "safe" command console overlaps ulnclaw's chat REPL slash-command surface (`/help` `/tools` `/skills` `/sessions` …), which already answers without an LLM turn where hermes' console engine would;
- `hermes dashboard` / `hermes serve` — the standalone web-UI server is replaced by the `desktop-electron/` app plus the gateway's OpenAI-compatible API surface (the gateway keeps the hermes dashboard CORS model for local-app dashboards);
- `hermes honcho` — the Honcho AI hosted-memory integration (a third-party SaaS) has no ulnclaw counterpart; local memory + learned skills + the memory graph cover the in-process scope;
- `hermes migrate` — config-format migrations are N/A (ulnclaw has no legacy format to migrate from);
- `hermes claw` — the OpenClaw migration importer is N/A (ulnclaw has no OpenClaw lineage);
- `hermes profile` — profile management is architecturally different: ulnclaw uses `[profiles.<name>]` config overrides + gateway multiplex (`/p/<profile>/...` mirrors) rather than a separate home-per-profile CLI;
- `hermes whatsapp` — the Baileys bridge wizard is replaced by the gateway-supervised built-in bridge: `npm` dependencies install on first gateway start, QR pairing happens through the bridge's own startup flow, and `[messaging.whatsapp]` toggles the platform (no separate wizard process to keep in sync). Its inspection role is covered by `ulnclaw whatsapp status` (P272).
